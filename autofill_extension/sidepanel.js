// Side panel: agent chat, context documents, memory (scripts), profile.
const $ = (id) => document.getElementById(id);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// The tab this panel is locked to (set by background.js when it opened the panel).
const LOCKED_TAB = Number(new URLSearchParams(location.search).get("tabId")) || null;
let tabId = LOCKED_TAB;
let tabUrl = "";
let agent = { running: false, stop: false };
// The conversation continues across messages on the same tab until "New chat".
let conversation = { sessionId: null, tabId: null };

// ---------- plumbing ----------
async function backendUrl() {
  const { backendUrl } = await chrome.storage.local.get({ backendUrl: "http://127.0.0.1:7777" });
  return backendUrl;
}

async function api(path, method = "GET", body) {
  const resp = await fetch((await backendUrl()) + path, {
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

// Messages to the background worker (tab state, scripts, profile).
function bg(msg) {
  return new Promise((resolve, reject) => {
    chrome.runtime.sendMessage({ ...msg, tabId }, (resp) => {
      if (chrome.runtime.lastError) return reject(new Error(chrome.runtime.lastError.message));
      if (!resp) return reject(new Error("no response from background"));
      if (resp.ok === false) return reject(new Error(resp.error));
      resolve(resp.data !== undefined ? resp.data : resp);
    });
  });
}

function toTab(msg, id = tabId) {
  return new Promise((resolve) => {
    chrome.tabs.sendMessage(id, msg, (resp) => {
      void chrome.runtime.lastError;
      resolve(resp);
    });
  });
}

// Pages opened before the extension loaded have no content script yet.
async function ensureContent(id = tabId) {
  const pong = await toTab({ type: "snapshot" }, id);
  if (pong) return true;
  try {
    await chrome.scripting.executeScript({ target: { tabId: id }, files: ["shared/netwatch.js"], world: "MAIN" });
    await chrome.scripting.executeScript({ target: { tabId: id }, files: ["shared/fingerprint.js", "shared/cursor.js", "content.js"] });
    return true;
  } catch (e) {
    throw new Error("Can't read this page (" + (e.message || e) + "). Chrome blocks extensions on chrome:// and store pages.");
  }
}

function host() {
  try { return new URL(tabUrl).hostname.replace(/^www\./, ""); } catch (_) { return ""; }
}

function el(tag, attrs = {}, children = []) {
  const n = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs)) {
    if (k === "class") n.className = v;
    else if (k === "text") n.textContent = v;
    else if (k.startsWith("on")) n.addEventListener(k.slice(2), v);
    else if (v !== undefined && v !== null) n.setAttribute(k, v);
  }
  for (const c of children) if (c) n.append(c);
  return n;
}

// Errors stay until the next action, page change, or 20 s, whichever comes first.
let errorTimer = null;
function showError(e) {
  clearTimeout(errorTimer);
  const text = e ? String(e.message || e) : "";
  $("error").textContent = text ? `${new Date().toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })} · ${text}` : "";
  if (text) errorTimer = setTimeout(() => ($("error").textContent = ""), 20000);
}

// ---------- tabs ----------
function selectTab(name) {
  for (const b of document.querySelectorAll(".tabs button")) b.classList.toggle("active", b.dataset.tab === name);
  for (const s of document.querySelectorAll(".tab")) s.classList.toggle("active", s.id === "tab-" + name);
  if (name === "context") loadDocs();
  if (name === "memory") refreshPage();
  if (name === "profile") loadProfile();
}

// ---------- page card + memory ----------
async function refreshPage() {
  if (tabId == null) return;
  let state, cfg;
  try {
    ({ state, cfg } = await bg({ type: "getTabState" }));
  } catch (e) {
    return showError(e);
  }
  $("mode").value = cfg.mode;
  $("backendUrl").value = cfg.backendUrl;
  const r = state.result;
  const badge = $("matchBadge");
  if (!r) {
    badge.textContent = "";
    badge.className = "badge";
    $("pageTitle").textContent = tabUrl ? new URL(tabUrl).hostname || "This page" : "No page";
    $("pageMeta").textContent = state.page ? "waiting for backend" : "no form fields seen";
    $("scripts").replaceChildren();
    $("scriptCount").textContent = "";
    return;
  }
  const o = r.observation;
  badge.textContent = r.matched ? "known" : "new";
  badge.className = "badge " + (r.matched ? "matched" : "new");
  $("pageTitle").textContent = o.title || o.host + o.url_pattern;
  $("pageMeta").textContent = `${o.fields.length} fields · seen ${o.seen_count}× · ${r.scripts.length} saved script${r.scripts.length === 1 ? "" : "s"}`;
  $("scriptCount").textContent = r.scripts.length || "";

  const list = $("scripts");
  list.replaceChildren();
  for (const s of r.scripts) {
    const pct = Math.round(s.confidence * 100);
    const last = state.lastRun && state.lastRun.scriptId === s.id ? state.lastRun.outcome : null;
    const steps = s.kind === "steps" ? `${s.body.length} steps` : "javascript";
    const item = el("li", {}, [
      el("div", { class: "name", text: s.name }),
      el("div", { class: "meta", text: `${steps} · ${s.origin} · ${pct}% (${s.successes} ok, ${s.failures} failed)` }),
      el("div", { class: "bar" }, [el("div", { style: `width:${pct}%` })]),
      el("div", { class: "buttons" }, [
        el("button", { text: "Run", class: "primary", onclick: () => act(() => bg({ type: "run", scriptId: s.id })) }),
        el("button", { text: "Delete", class: "danger", onclick: () => act(() => bg({ type: "deleteScript", scriptId: s.id })) }),
      ]),
    ]);
    if (last) {
      const msg = last.ok ? `Last run: ${last.ran} step(s) ok` + (last.skipped && last.skipped.length ? `, ${last.skipped.length} skipped` : "") : `Last run failed at step ${last.failed_step + 1}: ${last.error}`;
      item.append(el("div", { class: "outcome " + (last.ok ? "ok" : "bad"), text: msg }));
    }
    list.append(item);
  }
  if (!r.scripts.length) list.append(el("li", { class: "muted", text: "Nothing saved for this page yet. Ask the agent, or switch to Record and fill it yourself." }));
  $("next").textContent = r.next && r.next.length ? "Leads to: " + r.next.map((t) => `${t.to.slice(0, 8)} (${t.hits}×)`).join(", ") : "";
}

async function act(fn) {
  showError(null);
  try { await fn(); } catch (e) { showError(e); }
  await refreshPage();
}

// ---------- context documents ----------
function readBase64(file) {
  return new Promise((resolve, reject) => {
    const r = new FileReader();
    r.onload = () => resolve(String(r.result).split(",", 2)[1] || "");
    r.onerror = () => reject(r.error);
    r.readAsDataURL(file);
  });
}

function guessMime(file) {
  if (file.type) return file.type;
  const n = file.name.toLowerCase();
  if (n.endsWith(".pdf")) return "application/pdf";
  if (n.endsWith(".md") || n.endsWith(".txt")) return "text/plain";
  if (n.endsWith(".csv")) return "text/csv";
  return "application/octet-stream";
}

async function uploadFiles(files, scopeChoice) {
  const scope = scopeChoice === "host" ? host() || "*" : "*";
  const uploaded = [];
  for (const file of files) {
    if (file.size > 20 * 1024 * 1024) throw new Error(`${file.name} is larger than 20 MB`);
    const chip = el("span", { class: "chip busy", text: `Reading ${file.name}…` });
    $("chips").append(chip);
    try {
      const doc = await api("/v1/context", "POST", { name: file.name, mime: guessMime(file), scope, data_base64: await readBase64(file) });
      uploaded.push(doc);
    } finally {
      chip.remove();
    }
  }
  await loadDocs();
  return uploaded;
}

async function loadDocs() {
  let docs = [];
  try { docs = await api("/v1/context"); } catch (e) { return showError(e); }
  const h = host();
  const relevant = docs.filter((d) => d.scope === "*" || d.scope === h);
  $("ctxCount").textContent = relevant.length || "";
  // Chips above the prompt show what the agent will read on this site.
  const chips = $("chips");
  for (const c of [...chips.querySelectorAll(".chip:not(.busy)")]) c.remove();
  for (const d of relevant) chips.append(el("span", { class: "chip", title: d.preview, text: "📄 " + d.name }));

  const list = $("docs");
  list.replaceChildren();
  for (const d of docs) {
    const where = d.scope === "*" ? "all sites" : d.scope;
    list.append(
      el("li", {}, [
        el("div", { class: "name", text: d.name }),
        el("div", { class: "meta", text: `${where} · ${d.chars.toLocaleString()} characters` + (d.scope === "*" || d.scope === h ? " · used here" : "") }),
        el("div", { class: "preview", text: d.preview }),
        el("div", { class: "buttons" }, [
          el("button", { text: "View text", onclick: async () => {
            const full = await api(`/v1/context/${d.id}`);
            const w = window.open("", "_blank");
            if (w) { w.document.title = d.name; w.document.body.style.whiteSpace = "pre-wrap"; w.document.body.style.font = "13px ui-monospace, monospace"; w.document.body.textContent = full.text; }
          } }),
          el("button", { text: "Delete", class: "danger", onclick: async () => { await api(`/v1/context/${d.id}`, "DELETE"); loadDocs(); } }),
        ]),
      ])
    );
  }
  if (!docs.length) list.append(el("li", { class: "muted", text: "No documents yet." }));
}

// ---------- agent chat ----------
function chatAdd(node) {
  $("chatEmpty") && $("chatEmpty").remove();
  $("chat").append(node);
  node.scrollIntoView({ block: "end" });
  return node;
}

function describe(step) {
  const v = step.value && (step.value.literal ?? (step.value.profile ? "{" + step.value.profile + "}" : ""));
  switch (step.op) {
    case "click": return `click ${step.selector}`;
    case "waitFor": return `wait for ${step.selector}`;
    default: return `${step.op} ${step.selector} = ${JSON.stringify(v)}`;
  }
}

// Snapshot the page, tolerating navigations in progress.
async function snapshotWithRetry() {
  for (let i = 0; i < 20; i++) {
    const tab = await chrome.tabs.get(tabId).catch(() => null);
    if (!tab) throw new Error("the tab was closed");
    if (tab.status === "complete") {
      await ensureContent().catch(() => {});
      const snap = await toTab({ type: "agentSnapshot" });
      if (snap) return snap;
    }
    await sleep(400);
  }
  throw new Error("the page did not finish loading");
}

function setRunning(on) {
  agent.running = on;
  $("send").disabled = on;
  $("stop").hidden = !on;
  $("prompt").disabled = on;
}

async function runAgent(prompt) {
  showError(null);
  setRunning(true);
  agent.stop = false;
  const runTab = tabId;
  chatAdd(el("div", { class: "msg user", text: prompt }));
  const thinking = chatAdd(el("div", { class: "msg system", text: "Reading the page…" }));
  let sessionId = conversation.tabId === runTab ? conversation.sessionId : null;
  let pendingPrompt = prompt;
  let results = [];
  let events = [];
  let interrupted = null;
  let planEl = null;
  try {
    await ensureContent(runTab);
    for (;;) {
      if (agent.stop) {
        if (sessionId) {
          const saved = await api(`/v1/agent/${sessionId}/finish`, "POST", { results });
          reportSaved(saved, "Stopped.");
        } else chatAdd(el("div", { class: "msg system", text: "Stopped." }));
        break;
      }
      thinking.textContent = "Thinking…";
      $("chat").append(thinking);
      const page = await snapshotWithRetry();
      let resp;
      // A slow turn usually means OpenAI is rate-limiting; the backend waits and retries.
      const slowTimer = setTimeout(() => (thinking.textContent = "Still thinking… the model provider may be rate-limiting; retrying automatically."), 12000);
      try {
        resp = await api("/v1/agent/step", "POST", { session_id: sessionId, prompt: pendingPrompt, page, results, events, interrupted }).finally(() => clearTimeout(slowTimer));
      } catch (e) {
        if (sessionId && /session .* not found/.test(String(e.message))) {
          // Backend restarted: start over with the same request.
          sessionId = null;
          resp = await api("/v1/agent/step", "POST", { session_id: null, prompt, page, results: [] });
        } else throw e;
      }
      pendingPrompt = null;
      sessionId = resp.session_id;
      conversation = { sessionId, tabId: runTab };
      planEl = renderPlan(resp.plan, resp.background, planEl, resp.usage);

      const bubble = chatAdd(el("div", { class: "msg agent" }, [el("div", { text: resp.message || (resp.done ? "Done." : "Working…") })]));
      if (resp.done) {
        thinking.remove();
        reportSaved(resp.saved, null);
        break;
      }
      if (!resp.actions.length) { results = []; events = []; interrupted = null; await sleep(500); continue; }

      const rows = resp.actions.map((a) => el("li", { class: "pending", text: "• " + describe(a) }));
      bubble.append(el("ul", { class: "actions" }, rows));
      thinking.textContent = `Doing ${resp.actions.length} action${resp.actions.length === 1 ? "" : "s"}…`;
      $("chat").append(thinking);

      const before = (await chrome.tabs.get(runTab)).url;
      const { pace } = await chrome.storage.local.get({ pace: "normal" });
      thinking.textContent = resp.background ? `Acting · background: ${resp.background}` : `Doing ${resp.actions.length} action${resp.actions.length === 1 ? "" : "s"}…`;
      const out = await toTab({ type: "execute", steps: resp.actions, profile: {}, continueOnError: true, pace }, runTab);
      events = [];
      interrupted = null;
      if (out && out.results) {
        results = out.results;
        events = out.events || [];
        interrupted = out.interrupted || null;
      } else {
        // No reply usually means a click navigated away mid-batch.
        const after = (await chrome.tabs.get(runTab)).url;
        const navigated = after !== before;
        results = resp.actions.map(() => (navigated ? { ok: true } : { ok: false, error: "page did not respond" }));
      }
      results.forEach((r, i) => {
        if (!rows[i]) return;
        const notRun = r.skipped && /^not run/.test(r.error || "");
        rows[i].className = r.ok ? "ok" : notRun ? "pending" : "bad";
        rows[i].textContent = (r.ok ? "✓ " : notRun ? "· " : "✗ ") + describe(resp.actions[i]) + (r.ok ? "" : ` (${r.error || "skipped"})`);
      });
      if (interrupted) bubble.append(el("div", { class: "interrupt", text: `⚡ Page interrupted: ${interrupted}` }));
    }
  } catch (e) {
    showError(e);
    if (sessionId) {
      try { reportSaved(await api(`/v1/agent/${sessionId}/finish`, "POST", { results }), "Stopped on an error."); } catch (_) {}
    }
  } finally {
    thinking.remove();
    setRunning(false);
    refreshPage();
  }
}

// Live checklist of the background model's plan, updated every micro-turn.
function renderPlan(plan, background, existing, usage) {
  if (!plan && !background) return existing;
  const box = existing || el("div", { class: "msg plan" });
  box.replaceChildren();
  chatAdd(box); // re-appending moves it to the bottom: a live status card
  const icon = { pending: "☐", doing: "◐", done: "☑", skipped: "⊘" };
  if (plan) {
    const done = plan.items.filter((i) => i.status === "done").length;
    box.append(el("div", { class: "plan-head", text: `Plan · ${done}/${plan.items.length} done` }));
    if (plan.summary) box.append(el("div", { class: "muted small", text: plan.summary }));
    const ul = el("ul", { class: "plan-items" });
    for (const it of plan.items) ul.append(el("li", { class: "st-" + it.status, title: it.values.map((v) => `${v.field}: ${v.value}`).join("\n"), text: `${icon[it.status] || "☐"} ${it.title}${it.note ? " — " + it.note : ""}` }));
    box.append(ul);
    if (plan.questions && plan.questions.length) box.append(el("div", { class: "small", text: "Open questions: " + plan.questions.join(" · ") }));
  }
  if (background) box.append(el("div", { class: "muted small", text: "Background: " + background + "…" }));
  if (usage) {
    const t = ["interaction", "background"].reduce((a, k) => {
      const u = usage[k] || {};
      return { tokens: a.tokens + (u.prompt_tokens || 0) + (u.completion_tokens || 0), calls: a.calls + (u.calls || 0), cost: a.cost + (u.cost_usd || 0) };
    }, { tokens: 0, calls: 0, cost: 0 });
    box.append(el("div", { class: "muted small", text: `${t.calls} model calls · ${t.tokens.toLocaleString()} tokens${t.cost ? ` · $${t.cost.toFixed(4)}` : ""}` }));
  }
  return box;
}

// A quiet footnote: the agent's own message already says what happened on the page.
function reportSaved(saved, prefix) {
  const n = saved ? saved.length : 0;
  const steps = n ? saved.reduce((a, s) => a + (Array.isArray(s.body) ? s.body.length : 1), 0) : 0;
  const note = n ? `💾 Remembered ${steps} step${steps === 1 ? "" : "s"} for next time.` : "";
  const text = [prefix, note].filter(Boolean).join(" ");
  if (text) chatAdd(el("div", { class: "msg system", text }));
}

async function send() {
  const prompt = $("prompt").value.trim();
  if (!prompt || agent.running) return;
  if (tabId == null) return showError("No active tab.");
  $("prompt").value = "";
  await runAgent(prompt);
}

// ---------- profile ----------
async function loadProfile() {
  const table = $("profile");
  table.replaceChildren();
  try {
    const profile = await bg({ type: "getProfile" });
    for (const [k, v] of Object.entries(profile)) addFactRow(k, v);
  } catch (e) {
    showError(e);
  }
  if (!table.children.length) for (const k of ["firstName", "lastName", "email", "phone"]) addFactRow(k, "");
}

function addFactRow(k = "", v = "") {
  const tr = el("tr", {}, [
    el("td", {}, [el("input", { type: "text", placeholder: "key", value: k, class: "k" })]),
    el("td", {}, [el("input", { type: "text", placeholder: "value", value: v, class: "v" })]),
    el("td", {}, [el("button", { text: "×", class: "danger", onclick: () => { tr.dataset.removed = "1"; tr.style.display = "none"; } })]),
  ]);
  tr.dataset.originalKey = k;
  $("profile").append(tr);
}

async function saveProfile() {
  const payload = {};
  for (const tr of $("profile").querySelectorAll("tr")) {
    const k = tr.querySelector(".k").value.trim();
    const v = tr.querySelector(".v").value;
    if (tr.dataset.removed === "1") { if (tr.dataset.originalKey) payload[tr.dataset.originalKey] = null; continue; }
    if (!k) continue;
    if (tr.dataset.originalKey && tr.dataset.originalKey !== k) payload[tr.dataset.originalKey] = null;
    payload[k] = v;
  }
  await bg({ type: "putProfile", profile: payload });
  await loadProfile();
}

async function health() {
  try {
    const h = await api("/v1/health");
    $("health").className = "dot ok";
    $("health").title = `Backend ok · ${h.store} · ${h.llm ? `${h.provider} ${h.fast_model}` : "no model configured"}`;
  } catch (e) {
    $("health").className = "dot bad";
    $("health").title = "Backend unreachable: " + (e.message || e);
    showError("Backend not reachable at " + (await backendUrl()) + ". Start it with `cargo run`.");
  }
}

// ---------- locked tab tracking ----------
// The panel follows its own tab (navigations, reloads), never the active tab.
async function syncLockedTab() {
  if (tabId == null) {
    // Opened some other way (e.g. an old global panel): lock to the tab active now.
    const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
    tabId = tab ? tab.id : null;
  }
  const tab = tabId == null ? null : await chrome.tabs.get(tabId).catch(() => null);
  const url = tab ? tab.url || "" : "";
  const changed = url !== tabUrl;
  tabUrl = url;
  if (changed) {
    if (!agent.running) showError(null);
    await refreshPage();
    await loadDocs();
  }
}

chrome.tabs.onUpdated.addListener((id, info) => { if (id === tabId && (info.url || info.status === "complete")) syncLockedTab(); });
chrome.tabs.onRemoved.addListener((id) => { if (id === tabId) window.close(); });
chrome.runtime.onMessage.addListener((msg) => { if (msg.type === "tabStateChanged" && msg.tabId === tabId) refreshPage(); });

document.addEventListener("DOMContentLoaded", async () => {
  for (const b of document.querySelectorAll(".tabs button")) b.addEventListener("click", () => selectTab(b.dataset.tab));
  for (const b of document.querySelectorAll(".suggestion")) b.addEventListener("click", () => { $("prompt").value = b.textContent; $("prompt").focus(); });
  $("send").addEventListener("click", send);
  $("newChat").addEventListener("click", () => {
    if (agent.running) return;
    conversation = { sessionId: null, tabId: null };
    $("chat").replaceChildren();
    chatAdd(el("div", { class: "msg system", text: "New conversation." }));
  });
  $("stop").addEventListener("click", () => { agent.stop = true; $("stop").disabled = true; setTimeout(() => ($("stop").disabled = false), 1500); });
  $("prompt").addEventListener("keydown", (e) => { if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); send(); } });
  $("attach").addEventListener("change", async (e) => {
    const files = [...e.target.files];
    e.target.value = "";
    showError(null);
    try {
      const docs = await uploadFiles(files, $("attachScope").value);
      chatAdd(el("div", { class: "msg system", text: `Added ${docs.map((d) => d.name).join(", ")} as context.` }));
    } catch (err) { showError(err); }
  });
  $("ctxFile").addEventListener("change", async (e) => {
    const files = [...e.target.files];
    e.target.value = "";
    showError(null);
    try { await uploadFiles(files, $("ctxScope").value); } catch (err) { showError(err); }
  });
  $("pasteSave").addEventListener("click", async () => {
    const text = $("pasteText").value.trim();
    if (!text) return;
    const scope = $("ctxScope").value === "host" ? host() || "*" : "*";
    try {
      await api("/v1/context", "POST", { name: $("pasteName").value.trim() || "Pasted notes", mime: "text/plain", scope, text });
      $("pasteText").value = ""; $("pasteName").value = "";
      await loadDocs();
    } catch (err) { showError(err); }
  });
  chrome.storage.local.get({ pace: "normal" }).then(({ pace }) => ($("pace").value = pace));
  $("pace").addEventListener("change", () => chrome.storage.local.set({ pace: $("pace").value }));
  $("mode").addEventListener("change", () => act(() => bg({ type: "setMode", mode: $("mode").value })));
  $("rescan").addEventListener("click", () => act(async () => { await ensureContent(); await bg({ type: "reobserve" }); await sleep(600); }));
  $("learn").addEventListener("click", () => act(() => bg({ type: "learnNow" })));
  $("addFact").addEventListener("click", () => addFactRow());
  $("saveProfile").addEventListener("click", () => act(saveProfile));
  $("saveSettings").addEventListener("click", () => act(async () => { await bg({ type: "setBackendUrl", backendUrl: $("backendUrl").value }); await health(); }));
  await health();
  await syncLockedTab();
});
