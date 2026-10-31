// Service worker: talks to the Rust backend, keeps per-tab state, runs scripts.

const DEFAULTS = { backendUrl: "http://127.0.0.1:7777", mode: "replay", autoThreshold: 0.5, pace: "normal" };

// Like Claude in Chrome, the side panel is locked to the tab it was opened on:
// it shows only on that tab, hides when you switch away, and comes back (with its
// conversation) when you return. Each tab gets its own panel instance.
function disableGlobalPanel() {
  chrome.sidePanel.setPanelBehavior({ openPanelOnActionClick: false }).catch(() => {});
  chrome.sidePanel.setOptions({ enabled: false }).catch(() => {});
}
chrome.runtime.onInstalled.addListener(disableGlobalPanel);
chrome.runtime.onStartup.addListener(disableGlobalPanel);

chrome.action.onClicked.addListener((tab) => {
  if (tab.id == null) return;
  // No awaits before open(): it must run inside the click's user gesture.
  chrome.sidePanel.setOptions({ tabId: tab.id, path: `sidepanel.html?tabId=${tab.id}`, enabled: true });
  chrome.sidePanel.open({ tabId: tab.id }).catch(() => {});
});

async function settings() {
  const stored = await chrome.storage.local.get(DEFAULTS);
  // "Record me" used to be a mode; recording is now the per-tab "Learn from user" button.
  if (stored.mode === "record") stored.mode = DEFAULTS.mode;
  return { ...DEFAULTS, ...stored };
}

async function api(path, method = "GET", body) {
  const { backendUrl } = await settings();
  const resp = await fetch(backendUrl + path, {
    method,
    headers: body !== undefined ? { "content-type": "application/json" } : {},
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  const text = await resp.text();
  let data = null;
  try { data = text ? JSON.parse(text) : null; } catch (_) { data = { error: text }; }
  if (!resp.ok) throw new Error((data && data.error) || `${resp.status} ${resp.statusText}`);
  return data;
}

// ---------- per-tab state (session storage survives worker restarts) ----------
const tabKey = (tabId) => `tab:${tabId}`;

async function getTab(tabId) {
  const r = await chrome.storage.session.get(tabKey(tabId));
  return r[tabKey(tabId)] || { observationId: null, previous: null, result: null, page: null, lastRun: null, error: null };
}

async function setTab(tabId, state) {
  await chrome.storage.session.set({ [tabKey(tabId)]: state });
  // Let an open side panel refresh; nobody listening is fine.
  chrome.runtime.sendMessage({ type: "tabStateChanged", tabId }).catch(() => {});
}

chrome.tabs.onRemoved.addListener(async (tabId) => {
  // Closing the tab mid-demonstration still keeps what was recorded.
  const learning = await getLearn(tabId);
  if (learning.on) await saveLearning(tabId, learning).catch(() => {});
  chrome.storage.session.remove([tabKey(tabId), learnKey(tabId)]);
});

async function badge(tabId, text, color) {
  try {
    await chrome.action.setBadgeText({ tabId, text });
    if (color) await chrome.action.setBadgeBackgroundColor({ tabId, color });
  } catch (_) { /* tab gone */ }
}

function sendToTab(tabId, msg) {
  return new Promise((resolve) => {
    chrome.tabs.sendMessage(tabId, msg, (resp) => {
      void chrome.runtime.lastError;
      resolve(resp);
    });
  });
}

// ---------- core flow ----------
// Scripts that click can add rows or navigate; replaying them on every visit
// would duplicate entries, so auto mode only runs pure fill scripts.
function safeToAutoRun(script) {
  if (script.kind !== "steps") return false;
  return !script.body.some((s) => ["click", "submit", "navigate"].includes(s.op));
}

async function handleObserve(tabId, page, create = true) {
  const cfg = await settings();
  // While learning, every page the person passes through becomes a node so
  // the demonstration can be stored as a path through the graph.
  const learning = (await getLearn(tabId)).on;
  if (cfg.mode === "off" && !learning) return;
  if (learning) create = true;
  const state = await getTab(tabId);
  const previous = state.observationId
    ? { observation_id: state.observationId, script_id: state.lastRun ? state.lastRun.scriptId : null }
    : null;
  let result;
  try {
    result = await api("/v1/observe", "POST", { ...page, previous, create, generate: create && cfg.mode === "auto" });
  } catch (e) {
    if (!create && /no known node/.test(String(e.message))) {
      await setTab(tabId, { ...state, observationId: null, result: null, page, error: null });
      await badge(tabId, "");
      return;
    }
    await setTab(tabId, { ...state, page, error: String(e.message || e) });
    await badge(tabId, "!", "#b91c1c");
    return;
  }
  const next = {
    observationId: result.observation.id,
    previous,
    result,
    page,
    lastRun: null,
    error: null,
  };
  await setTab(tabId, next);
  const n = result.scripts.length;
  if (learning) await badge(tabId, "REC", "#b91c1c");
  else await badge(tabId, result.matched ? (n ? String(n) : "✓") : "?", result.matched ? "#15803d" : "#b45309");

  // Never replay over the person while they are demonstrating.
  if (cfg.mode === "auto" && n > 0 && !learning) {
    const top = result.scripts[0];
    if ((result.generated || top.confidence >= cfg.autoThreshold) && safeToAutoRun(top)) {
      await runScript(tabId, top.id);
    }
  }
}

async function runScript(tabId, scriptId) {
  const state = await getTab(tabId);
  if (!state.result) throw new Error("no observation for this tab yet");
  const script = state.result.scripts.find((s) => s.id === scriptId);
  if (!script) throw new Error("script not in current observation");
  const profile = await api("/v1/profile");
  let outcome;
  if (script.kind === "steps") {
    const { pace } = await settings();
    outcome = await sendToTab(tabId, { type: "execute", steps: script.body, profile, pace });
    if (!outcome) outcome = { ok: false, error: "content script did not respond" };
  } else {
    outcome = await runJs(tabId, script.body, profile);
  }
  const report = await api("/v1/runs", "POST", {
    observation_id: state.observationId,
    script_id: scriptId,
    ok: !!outcome.ok,
    failed_step: outcome.failed_step ?? null,
    error: outcome.error ?? null,
  });
  // Update the cached ranking so the popup reflects the new confidence.
  const fresh = await getTab(tabId);
  if (fresh.result) {
    const s = fresh.result.scripts.find((x) => x.id === scriptId);
    if (s) {
      s.confidence = report.confidence;
      s.successes = report.edge.successes;
      s.failures = report.edge.failures;
    }
    fresh.result.scripts.sort((a, b) => b.confidence - a.confidence);
  }
  fresh.lastRun = { scriptId, outcome, at: Date.now() };
  await setTab(tabId, fresh);
  return { outcome, report };
}

// Raw JS tool scripts run in the page's main world with `profile` in scope.
async function runJs(tabId, code, profile) {
  const wrapped = `(async (profile) => { try { ${code}\n; return { ok: true }; } catch (e) { return { ok: false, error: String(e && e.message || e) }; } })(${JSON.stringify(profile)})`;
  try {
    if (chrome.userScripts && chrome.userScripts.execute) {
      const results = await chrome.userScripts.execute({ target: { tabId }, world: "MAIN", js: [{ code: wrapped }] });
      const r = results && results[0];
      if (r && r.error) return { ok: false, error: String(r.error.message || r.error) };
      return (r && r.result) || { ok: true };
    }
    const results = await chrome.scripting.executeScript({
      target: { tabId },
      world: "MAIN",
      func: (src) => (0, eval)(src),
      args: [wrapped],
    });
    const r = results && results[0];
    return (r && r.result) || { ok: true };
  } catch (e) {
    return { ok: false, error: `js execution unavailable: ${e.message || e}. Enable "Allow User Scripts" for this extension.` };
  }
}

// ---------- "Learn from user" ----------
// The person demonstrates a task; every move is attributed to the page node
// the tab was on at the time, so a multi-page flow is saved as one script per
// page plus the transitions between them.
const learnKey = (tabId) => `learn:${tabId}`;
const LEARN_OFF = { on: false, startedAt: 0, segments: [] };

async function getLearn(tabId) {
  const r = await chrome.storage.session.get(learnKey(tabId));
  return r[learnKey(tabId)] || { ...LEARN_OFF };
}

async function setLearn(tabId, learning) {
  await chrome.storage.session.set({ [learnKey(tabId)]: learning });
  chrome.runtime.sendMessage({ type: "learnChanged", tabId }).catch(() => {});
}

// Moves arrive in bursts; apply them one batch at a time so none are lost.
let learnQueue = Promise.resolve();
function serialLearn(fn) {
  const next = learnQueue.then(fn, fn);
  learnQueue = next.catch(() => {});
  return next;
}

function appendMoves(tabId, actions) {
  return serialLearn(async () => {
    const learning = await getLearn(tabId);
    if (!learning.on || !actions || !actions.length) return learning;
    const state = await getTab(tabId);
    const observationId = state.observationId || null;
    const title = state.result ? state.result.observation.title || state.result.observation.host : "";
    let seg = learning.segments[learning.segments.length - 1];
    if (!seg || seg.observationId !== observationId) {
      seg = { observationId, title, url: state.page ? state.page.url : "", actions: [] };
      learning.segments.push(seg);
    }
    seg.actions.push(...actions);
    await setLearn(tabId, learning);
    return learning;
  });
}

function learnSummary(learning) {
  const moves = learning.segments.reduce((n, s) => n + s.actions.length, 0);
  const pages = new Set(learning.segments.filter((s) => s.actions.length).map((s) => s.observationId)).size;
  const recent = learning.segments.flatMap((s) => s.actions).slice(-6);
  return { on: learning.on, startedAt: learning.startedAt, moves, pages, recent };
}

async function startLearning(tabId) {
  await setLearn(tabId, { on: true, startedAt: Date.now(), segments: [] });
  await sendToTab(tabId, { type: "setRecording", on: true });
  // Make sure the starting page is a node even if the mode is Off or it has no fields.
  await sendToTab(tabId, { type: "reobserve" });
  await badge(tabId, "REC", "#b91c1c");
  return learnSummary(await getLearn(tabId));
}

async function saveLearning(tabId, learning) {
  const segments = learning.segments
    .filter((s) => s.observationId && s.actions.length)
    .map((s) => ({ observation_id: s.observationId, actions: s.actions }));
  const dropped = learning.segments.filter((s) => !s.observationId).reduce((n, s) => n + s.actions.length, 0);
  if (!segments.length) {
    throw new Error(dropped ? "the page was never matched to a node (is the backend running?)" : "nothing recorded yet");
  }
  const saved = await api("/v1/learn/session", "POST", { segments });
  if (dropped) saved.skipped.push(`${dropped} move(s) made before the page was observed`);
  return saved;
}

async function stopLearning(tabId, save) {
  const leftover = await sendToTab(tabId, { type: "takeRecording" });
  if (leftover && leftover.actions && leftover.actions.length) await appendMoves(tabId, leftover.actions);
  const learning = await serialLearn(() => getLearn(tabId));
  await setLearn(tabId, { ...LEARN_OFF });
  await sendToTab(tabId, { type: "setRecording", on: false });
  await badge(tabId, "");
  if (!save || !learning.on) return { saved: null };
  const saved = await saveLearning(tabId, learning);
  // Refresh this tab's node so the Memory tab lists the new scripts.
  await sendToTab(tabId, { type: "reobserve" });
  return { saved };
}

async function generateForTab(tabId) {
  const state = await getTab(tabId);
  if (!state.observationId) throw new Error("no observation for this tab yet");
  const script = await api("/v1/generate", "POST", { observation_id: state.observationId });
  const fresh = await getTab(tabId);
  if (fresh.result) {
    fresh.result.scripts = fresh.result.scripts.filter((s) => s.id !== script.id);
    fresh.result.scripts.unshift(script);
  }
  await setTab(tabId, fresh);
  return script;
}

async function setMode(mode) {
  await chrome.storage.local.set({ mode });
  const tabs = await chrome.tabs.query({});
  for (const t of tabs) {
    if (mode === "off" && t.id != null && !(await getLearn(t.id)).on) badge(t.id, "");
  }
}

// ---------- message router ----------
chrome.runtime.onMessage.addListener((msg, sender, respond) => {
  const tabId = msg.tabId ?? (sender.tab && sender.tab.id);
  const reply = (p) => p.then((data) => respond({ ok: true, data })).catch((e) => respond({ ok: false, error: String(e.message || e) }));
  switch (msg.type) {
    case "observe":
      reply(handleObserve(tabId, msg.page, msg.create !== false));
      return true;
    case "getMode":
      Promise.all([settings(), tabId != null ? getLearn(tabId) : LEARN_OFF]).then(([s, l]) => respond({ mode: s.mode, learning: !!l.on }));
      return true;
    case "learnActions":
      reply(appendMoves(tabId, msg.actions).then(() => null));
      return true;
    case "learnStart":
      reply(startLearning(tabId));
      return true;
    case "learnStop":
      reply(stopLearning(tabId, msg.save !== false));
      return true;
    case "learnStatus":
      reply(getLearn(tabId).then(learnSummary));
      return true;
    case "getTabState":
      reply(Promise.all([getTab(tabId), settings()]).then(([state, cfg]) => ({ state, cfg })));
      return true;
    case "run":
      reply(runScript(tabId, msg.scriptId));
      return true;
    case "generate":
      reply(generateForTab(tabId));
      return true;
    case "deleteScript":
      reply(api(`/v1/scripts/${encodeURIComponent(msg.scriptId)}`, "DELETE").then(async (r) => {
        const s = await getTab(tabId);
        if (s.result) s.result.scripts = s.result.scripts.filter((x) => x.id !== msg.scriptId);
        await setTab(tabId, s);
        return r;
      }));
      return true;
    case "reobserve":
      reply(sendToTab(tabId, { type: "reobserve" }));
      return true;
    case "setMode":
      reply(setMode(msg.mode));
      return true;
    case "setBackendUrl":
      reply(chrome.storage.local.set({ backendUrl: msg.backendUrl.replace(/\/+$/, "") }));
      return true;
    case "health":
      reply(api("/v1/health"));
      return true;
    case "getProfile":
      reply(api("/v1/profile"));
      return true;
    case "putProfile":
      reply(api("/v1/profile", "PUT", msg.profile));
      return true;
    default:
      return false;
  }
});
