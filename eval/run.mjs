#!/usr/bin/env node
// Phalaenopsis eval harness.
//
// Runs the real extension scripts (shared/*.js, content.js) inside headless Chrome
// on real web forms, drives them with the same agent loop as the side panel, and
// optionally lets a simulated human use the page at the same time (trusted mouse
// and keyboard input over the DevTools protocol). Records correctness, micro-turns,
// tokens and cost, and screenshots showing the virtual cursor.
//
// Usage:
//   node eval/run.mjs                      # every scenario, calm + busy
//   node eval/run.mjs --only=signup,wizard --modes=busy
//   node eval/run.mjs --headed             # watch it in a real window
//
// Isolation: starts its own backend with an in-memory store on port 7787, so
// your real graph, profile and documents are never touched. Needs
// OPENROUTER_KEY (or OPENAI_KEY) in the repo's .env, and `cargo build`.

import { spawn } from "node:child_process";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { SCENARIOS } from "./scenarios.mjs";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, "..");
const EXT = path.join(ROOT, "autofill_extension");
const BACKEND_ADDR = "127.0.0.1:7787";
const BACKEND = `http://${BACKEND_ADDR}`;
const SITE_PORT = 7790;
const SITE = `http://127.0.0.1:${SITE_PORT}`;

const args = Object.fromEntries(
  process.argv.slice(2).map((a) => {
    const [k, v] = a.replace(/^--/, "").split("=");
    return [k, v === undefined ? true : v];
  }),
);
const ONLY = args.only ? String(args.only).split(",") : null;
const MODES = String(args.modes || "calm,busy").split(",");
const PACE = String(args.pace || "normal");
const HEADED = !!args.headed;
const MAX_TURNS = Number(args.maxTurns || 60);
const MAX_SECONDS = Number(args.maxSeconds || 420);

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const rand = (a, b) => a + Math.random() * (b - a);
const log = (...a) => console.log(new Date().toISOString().slice(11, 19), ...a);

// ---------------------------------------------------------------- processes

function startStaticServer() {
  const types = { ".html": "text/html", ".htm": "text/html", ".js": "text/javascript", ".css": "text/css", ".png": "image/png", ".gif": "image/gif", ".jpg": "image/jpeg", ".svg": "image/svg+xml", ".json": "application/json", ".ico": "image/x-icon", ".woff2": "font/woff2" };
  const roots = { "/sites/": path.join(HERE, "sites"), "/test/": path.join(EXT, "test") };
  const server = http.createServer((req, res) => {
    const url = new URL(req.url, SITE);
    // jquery-validation's Milk demo posts its remote checks here; mockjax answers in-page.
    for (const [prefix, dir] of Object.entries(roots)) {
      if (!url.pathname.startsWith(prefix)) continue;
      const file = path.normalize(path.join(dir, decodeURIComponent(url.pathname.slice(prefix.length))));
      if (!file.startsWith(dir)) break;
      fs.readFile(file, (err, data) => {
        if (err) {
          res.writeHead(404).end("not found");
          return;
        }
        res.writeHead(200, { "content-type": types[path.extname(file).toLowerCase()] || "application/octet-stream" }).end(data);
      });
      return;
    }
    res.writeHead(404).end("not found");
  });
  return new Promise((resolve) => server.listen(SITE_PORT, "127.0.0.1", () => resolve(server)));
}

async function startBackend() {
  const bin = path.join(ROOT, "target", "debug", "phalaenopsis");
  if (!fs.existsSync(bin)) throw new Error("backend binary missing: run `cargo build` first");
  const out = fs.openSync(path.join(OUT_DIR, "backend.log"), "w");
  const proc = spawn(bin, [], {
    cwd: ROOT, // loads .env (API keys) and serves /test from here
    env: { ...process.env, PHALAENOPSIS_STORE: "memory", PHALAENOPSIS_ADDR: BACKEND_ADDR, RUST_LOG: "info" },
    stdio: ["ignore", out, out],
  });
  for (let i = 0; i < 80; i++) {
    try {
      const h = await (await fetch(`${BACKEND}/v1/health`)).json();
      if (!h.llm) throw new Error("backend has no model configured (set OPENROUTER_KEY or OPENAI_KEY in .env)");
      return { proc, health: h };
    } catch (e) {
      if (String(e.message).includes("no model")) throw e;
      await sleep(250);
    }
  }
  proc.kill();
  throw new Error("backend did not start; see backend.log");
}

async function startChrome() {
  const profile = fs.mkdtempSync(path.join(os.tmpdir(), "phal-eval-chrome-"));
  const chromeArgs = [
    `--user-data-dir=${profile}`,
    "--remote-debugging-port=0",
    "--no-first-run",
    "--no-default-browser-check",
    "--disable-extensions",
    "--window-size=1280,900",
    ...(HEADED ? [] : ["--headless=new"]),
    "about:blank",
  ];
  const proc = spawn(process.env.CHROME || "google-chrome", chromeArgs, { stdio: "ignore" });
  const portFile = path.join(profile, "DevToolsActivePort");
  for (let i = 0; i < 100 && !fs.existsSync(portFile); i++) await sleep(100);
  const port = fs.readFileSync(portFile, "utf8").split("\n")[0].trim();
  return { proc, port, profile };
}

// ---------------------------------------------------------------- CDP

class CDP {
  static async open(port) {
    const target = await (await fetch(`http://127.0.0.1:${port}/json/new?about:blank`, { method: "PUT" })).json();
    const cdp = new CDP(target.webSocketDebuggerUrl);
    cdp.targetId = target.id;
    cdp.port = port;
    await new Promise((res, rej) => {
      cdp.ws.addEventListener("open", res, { once: true });
      cdp.ws.addEventListener("error", rej, { once: true });
    });
    return cdp;
  }

  constructor(url) {
    this.ws = new WebSocket(url);
    this.id = 0;
    this.pending = new Map();
    this.listeners = new Map();
    this.ws.addEventListener("message", (ev) => {
      const msg = JSON.parse(ev.data);
      if (msg.id && this.pending.has(msg.id)) {
        const { resolve, reject } = this.pending.get(msg.id);
        this.pending.delete(msg.id);
        msg.error ? reject(new Error(msg.error.message)) : resolve(msg.result);
      } else if (msg.method) {
        for (const fn of this.listeners.get(msg.method) || []) fn(msg.params);
      }
    });
  }

  send(method, params = {}, timeoutMs = 60000) {
    const id = ++this.id;
    this.ws.send(JSON.stringify({ id, method, params }));
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      setTimeout(() => {
        if (this.pending.has(id)) {
          this.pending.delete(id);
          reject(new Error(`CDP timeout: ${method}`));
        }
      }, timeoutMs);
    });
  }

  on(method, fn) {
    if (!this.listeners.has(method)) this.listeners.set(method, []);
    this.listeners.get(method).push(fn);
  }

  async close() {
    try {
      await fetch(`http://127.0.0.1:${this.port}/json/close/${this.targetId}`);
    } catch (_) {}
    this.ws.close();
  }
}

// ---------------------------------------------------------------- page bootstrap

const read = (p) => fs.readFileSync(path.join(EXT, p), "utf8");

// Runs at document start in every document of the tab (survives navigations).
function bootScript() {
  const contentBundle = [read("shared/fingerprint.js"), read("shared/cursor.js"), read("content.js")].join("\n;\n");
  return `(() => {
  if (window.__phalBoot) return;
  window.__phalBoot = true;
  // Minimal stand-in for the extension runtime the content script talks to.
  window.chrome = window.chrome || {};
  window.chrome.runtime = {
    lastError: undefined,
    sendMessage: (m, cb) => { if (cb) cb({ mode: "off" }); },
    onMessage: { addListener: (fn) => { window.__phalListener = fn; } },
  };
  ${read("shared/netwatch.js")}
  const start = () => {
    (0, eval)(${JSON.stringify(contentBundle)});
    window.__phal = { call: (msg) => new Promise((res) => window.__phalListener(msg, {}, res)) };
    // Harness only: a blue dot that follows the (simulated) human's real mouse,
    // so screenshots show both the human and the agent's orange cursor.
    const dot = document.createElement("phal-human");
    dot.style.cssText = "position:fixed;left:0;top:0;width:14px;height:14px;margin:-7px 0 0 -7px;border-radius:50%;background:rgba(37,99,235,.85);border:2px solid #fff;box-shadow:0 0 0 1px rgba(0,0,0,.3);pointer-events:none;z-index:2147483646;transition:transform .08s linear;display:none";
    document.documentElement.appendChild(dot);
    addEventListener("mousemove", (e) => { if (e.isTrusted) { dot.style.display = "block"; dot.style.transform = "translate(" + e.clientX + "px," + e.clientY + "px)"; } }, true);
    addEventListener("mousedown", (e) => { if (e.isTrusted) { dot.style.background = "rgba(220,38,38,.9)"; setTimeout(() => (dot.style.background = "rgba(37,99,235,.85)"), 250); } }, true);
  };
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", start, { once: true });
  else start();
})();`;
}

async function evaluate(cdp, expression, timeoutMs = 60000) {
  const r = await cdp.send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true }, timeoutMs);
  if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description || r.exceptionDetails.text);
  return r.result.value;
}

async function waitReady(cdp, timeoutMs = 20000) {
  const end = Date.now() + timeoutMs;
  while (Date.now() < end) {
    try {
      if (await evaluate(cdp, `document.readyState === "complete" && !!window.__phal`, 3000)) return true;
    } catch (_) {}
    await sleep(200);
  }
  throw new Error("page never became ready");
}

/** Message the content script; null if the page navigated away mid-call. */
async function pageCall(cdp, msg, timeoutMs = 120000) {
  try {
    return await evaluate(cdp, `window.__phal.call(${JSON.stringify(msg)})`, timeoutMs);
  } catch (e) {
    if (/context|navigat|destroyed|Cannot find|timeout/i.test(e.message)) return null;
    throw e;
  }
}

async function screenshot(cdp, file) {
  try {
    const { data } = await cdp.send("Page.captureScreenshot", { format: "jpeg", quality: 70 }, 15000);
    fs.writeFileSync(file, Buffer.from(data, "base64"));
    return path.relative(OUT_DIR, file);
  } catch (_) {
    return null;
  }
}

// ---------------------------------------------------------------- simulated human

/**
 * A person using the same tab while the agent works: moves the mouse around,
 * scrolls, clicks on empty space, now and then edits a field the agent already
 * filled, or presses Escape. All input is trusted (Input.dispatch*), exactly
 * like a real mouse and keyboard.
 */
class Human {
  constructor(cdp) {
    this.cdp = cdp;
    this.x = 640;
    this.y = 450;
    this.actions = [];
    this.running = false;
    this.edits = 0;
  }

  async move(x, y, steps = 14) {
    const sx = this.x, sy = this.y;
    // Slight curve, like a hand.
    const cx = (sx + x) / 2 + rand(-80, 80), cy = (sy + y) / 2 + rand(-60, 60);
    for (let i = 1; i <= steps; i++) {
      const t = i / steps;
      const px = (1 - t) * (1 - t) * sx + 2 * (1 - t) * t * cx + t * t * x;
      const py = (1 - t) * (1 - t) * sy + 2 * (1 - t) * t * cy + t * t * y;
      await this.cdp.send("Input.dispatchMouseEvent", { type: "mouseMoved", x: px, y: py });
      await sleep(rand(12, 28));
    }
    this.x = x;
    this.y = y;
  }

  async click(x, y) {
    await this.move(x, y);
    await this.cdp.send("Input.dispatchMouseEvent", { type: "mousePressed", x, y, button: "left", clickCount: 1 });
    await sleep(rand(50, 110));
    await this.cdp.send("Input.dispatchMouseEvent", { type: "mouseReleased", x, y, button: "left", clickCount: 1 });
  }

  async key(key, text) {
    const code = key.length === 1 ? `Key${key.toUpperCase()}` : key;
    await this.cdp.send("Input.dispatchKeyEvent", { type: text ? "keyDown" : "rawKeyDown", key, code, text, windowsVirtualKeyCode: key === "Escape" ? 27 : key === "End" ? 35 : 0 });
    await this.cdp.send("Input.dispatchKeyEvent", { type: "keyUp", key, code });
  }

  note(kind, detail, t0) {
    this.actions.push({ t: Math.round((Date.now() - t0) / 100) / 10, kind, detail });
  }

  async act(t0) {
    const r = Math.random();
    if (r < 0.32) {
      const x = rand(60, 1220), y = rand(60, 840);
      await this.move(x, y, 18);
      this.note("move", `${Math.round(x)},${Math.round(y)}`, t0);
    } else if (r < 0.55) {
      const dy = Math.round(rand(-450, 450));
      await this.cdp.send("Input.dispatchMouseEvent", { type: "mouseWheel", x: this.x, y: this.y, deltaX: 0, deltaY: dy });
      this.note("scroll", String(dy), t0);
    } else if (r < 0.8) {
      // Click somewhere harmless: not a link, button, field or label.
      const p = await evaluate(this.cdp, `(() => {
        for (let i = 0; i < 40; i++) {
          const x = 40 + Math.random() * (innerWidth - 80), y = 40 + Math.random() * (innerHeight - 80);
          const el = document.elementFromPoint(x, y);
          if (el && !el.closest("a, button, input, select, textarea, label, [onclick], [role=button], summary, .ui-accordion-header, .ui-accordion-link")) return { x, y, tag: el.tagName };
        }
        return null;
      })()`, 5000).catch(() => null);
      if (p) {
        await this.click(p.x, p.y);
        this.note("click empty space", p.tag, t0);
      }
    } else if (r < 0.92 && this.edits < 1) {
      // Edit a field the agent already filled (people do fix things themselves).
      const f = await evaluate(this.cdp, `(() => {
        const els = [...document.querySelectorAll("input[type=text], input:not([type]), input[type=email], textarea")]
          .filter((e) => e.value && e.offsetParent && e.getBoundingClientRect().top > 0 && e.getBoundingClientRect().bottom < innerHeight);
        const e = els[Math.floor(Math.random() * els.length)];
        if (!e) return null;
        const r = e.getBoundingClientRect();
        return { x: r.left + Math.min(r.width - 6, 30), y: r.top + r.height / 2, name: e.name || e.id, before: e.value };
      })()`, 5000).catch(() => null);
      if (f) {
        this.edits++;
        await this.click(f.x, f.y);
        await this.key("End");
        for (const ch of " x") await this.key(ch, ch);
        await this.key("Tab");
        // Record what the field really holds now (End in a textarea only goes to
        // the end of the line) and mark the element to find it again later.
        const after = await evaluate(this.cdp, `(() => { const f = document.querySelector(${JSON.stringify(`[name="${f.name}"], #${f.name}`)}); if (!f) return null; f.setAttribute("data-phal-human-edited", "1"); return f.value; })()`, 5000).catch(() => null);
        this.note("EDITED a filled field", `${f.name}: "${f.before}" -> "${after}"`, t0);
        this.edited = { name: f.name, after, t: (Date.now() - t0) / 1000 };
      }
    } else {
      await this.key("Escape");
      this.note("press Escape", "", t0);
    }
  }

  start(t0) {
    this.running = true;
    this.loop = (async () => {
      await sleep(rand(1500, 3000));
      while (this.running) {
        try {
          await this.act(t0);
        } catch (_) {
          /* page navigating; try again later */
        }
        await sleep(rand(1200, 3500));
      }
    })();
  }

  async stop() {
    this.running = false;
    await this.loop;
  }
}

// ---------------------------------------------------------------- agent loop (mirrors sidepanel.js)

async function api(p, method = "GET", body) {
  const r = await fetch(BACKEND + p, { method, headers: body ? { "content-type": "application/json" } : {}, body: body ? JSON.stringify(body) : undefined });
  const t = await r.text();
  const data = t ? JSON.parse(t) : null;
  if (!r.ok) throw new Error((data && data.error) || `${r.status}`);
  return data;
}

async function runOne(browser, scenario, mode, index) {
  const dir = path.join(OUT_DIR, `${String(index).padStart(2, "0")}-${scenario.id}-${mode}`);
  fs.mkdirSync(dir, { recursive: true });

  // Fresh context: only this scenario's document, scoped to the test site.
  for (const d of await api("/v1/context")) await api(`/v1/context/${d.id}`, "DELETE");
  await api("/v1/context", "POST", { name: `${scenario.id}-context.txt`, mime: "text/plain", scope: "127.0.0.1", text: scenario.doc });

  const cdp = await CDP.open(browser.port);
  await cdp.send("Page.enable");
  // A blocking alert/confirm would freeze every later evaluate: dismiss and log it.
  const dialogs = [];
  cdp.on("Page.javascriptDialogOpening", (p) => {
    dialogs.push({ type: p.type, message: String(p.message).slice(0, 120) });
    cdp.send("Page.handleJavaScriptDialog", { accept: p.type !== "beforeunload" }).catch(() => {});
  });
  await cdp.send("Runtime.enable");
  await cdp.send("Emulation.setDeviceMetricsOverride", { width: 1280, height: 900, deviceScaleFactor: 1, mobile: false });
  await cdp.send("Page.addScriptToEvaluateOnNewDocument", { source: bootScript() });
  await cdp.send("Page.navigate", { url: SITE + scenario.path });
  await waitReady(cdp);

  const human = mode === "busy" ? new Human(cdp) : null;
  const t0 = Date.now();
  const turns = [];
  const shots = [];
  let sessionId = null, results = [], events = [], interrupted = null, pending = scenario.prompt, last = null, error = null;
  let lastSnapshotAt = 0;
  if (human) human.start(t0);

  try {
    for (let i = 0; i < MAX_TURNS; i++) {
      if ((Date.now() - t0) / 1000 > MAX_SECONDS) throw new Error(`time limit ${MAX_SECONDS}s`);
      await waitReady(cdp);
      lastSnapshotAt = (Date.now() - t0) / 1000;
      const page = await pageCall(cdp, { type: "agentSnapshot" });
      if (!page) continue;
      const tm = Date.now();
      const resp = await api("/v1/agent/step", "POST", { session_id: sessionId, prompt: pending, page, results, events, interrupted });
      pending = null;
      sessionId = resp.session_id;
      last = resp;
      const describe = (a) => `${a.op} ${a.selector}${a.value && a.value.literal !== undefined ? ` = ${JSON.stringify(a.value.literal).slice(0, 40)}` : ""}`;
      const turn = { n: resp.turn, at_s: +((Date.now() - t0) / 1000).toFixed(1), model_s: +((Date.now() - tm) / 1000).toFixed(1), message: resp.message, actions: resp.actions.length, did: resp.actions.map(describe), done: resp.done, background: resp.background || null };
      turns.push(turn);
      log(`  ${scenario.id}/${mode} t${resp.turn} ${turn.model_s}s, ${resp.actions.length} actions${resp.done ? ", done" : ""}: ${resp.message.slice(0, 90)}`);
      if (resp.done) break;
      if (!resp.actions.length) {
        results = []; events = []; interrupted = null;
        await sleep(500);
        continue;
      }
      // Mid-batch screenshot catches the virtual cursor in motion.
      const mid = sleep(900).then(() => screenshot(cdp, path.join(dir, `t${String(resp.turn).padStart(2, "0")}-a.jpg`)));
      const beforeUrl = (await cdp.send("Runtime.evaluate", { expression: "location.href", returnByValue: true }).catch(() => ({ result: {} }))).result.value;
      const out = await pageCall(cdp, { type: "execute", steps: resp.actions, profile: {}, continueOnError: true, pace: PACE, cursor: true });
      const a = await mid;
      const b = await screenshot(cdp, path.join(dir, `t${String(resp.turn).padStart(2, "0")}-b.jpg`));
      shots.push(...[a, b].filter(Boolean));
      if (out) {
        results = out.results;
        events = out.events || [];
        interrupted = out.interrupted || null;
      } else {
        // Navigated mid-batch (form submit, link): same rule as the side panel.
        await waitReady(cdp).catch(() => {});
        const afterUrl = await evaluate(cdp, "location.href", 5000).catch(() => null);
        results = resp.actions.map(() => (afterUrl !== beforeUrl ? { ok: true } : { ok: false, error: "page did not respond" }));
        events = [];
        interrupted = afterUrl !== beforeUrl ? "the page navigated" : null;
      }
      turn.ran = results.filter((r) => r.ok).length;
      turn.interrupted = interrupted;
    }
  } catch (e) {
    error = String(e.message || e);
    log(`  ${scenario.id}/${mode} ERROR ${error}`);
    if (sessionId) await api(`/v1/agent/${sessionId}/finish`, "POST", { results }).catch(() => {});
  }
  if (human) await human.stop();
  const wall = (Date.now() - t0) / 1000;
  await sleep(300);
  shots.push(await screenshot(cdp, path.join(dir, "final.jpg")));

  let verdict;
  try {
    verdict = await evaluate(cdp, `(() => { ${scenario.verify} })()`, 10000);
  } catch (e) {
    verdict = { pass: false, checks: [{ name: "verify crashed", ok: false, got: String(e.message) }] };
  }
  // The simulated person edited a field: the agent should keep their edit and say so,
  // rather than overwrite it. Judge that instead of the original exact-value check.
  if (human && human.edited) {
    const { name, after } = human.edited;
    // Only the same element counts: dialogs rebuilt per row reuse field names.
    const now = await evaluate(cdp, `(() => { const e = document.querySelector("[data-phal-human-edited]"); return e ? e.value : "__gone__"; })()`, 5000).catch(() => "__gone__");
    verdict.checks = verdict.checks.filter((c) => !(c.got && String(c.got).includes(" x")));
    if (now !== "__gone__") verdict.checks.push({ name: `person's edit to ${name} kept`, ok: now === after, got: now });
    // Only fair to expect a mention if the agent looked at the page after the edit.
    if (human.edited.t < lastSnapshotAt) verdict.checks.push({ name: `person's edit to ${name} mentioned`, ok: /(you|your|person|user)\b.{0,60}(chang|edit|typ|add|modif|renam)|(chang|edit|modif).{0,40}\b(by you|yourself)/i.test(last ? last.message : ""), got: (last ? last.message : "").slice(0, 80) });
    verdict.pass = verdict.checks.every((c) => c.ok);
  }
  if (scenario.judge) {
    verdict.checks.push(...scenario.judge(last ? last.message : ""));
    verdict.pass = verdict.checks.every((c) => c.ok);
  }
  await cdp.close();

  const usage = last ? last.usage : { interaction: {}, background: {} };
  const sum = (k) => (usage.interaction?.[k] || 0) + (usage.background?.[k] || 0);
  const result = {
    scenario: scenario.id,
    title: scenario.title,
    mode,
    pass: !!verdict.pass && !error,
    checks: verdict.checks,
    error,
    micro_turns: turns.length,
    interrupts: turns.filter((t) => t.interrupted).length,
    wall_s: +wall.toFixed(1),
    model_calls: sum("calls"),
    prompt_tokens: sum("prompt_tokens"),
    completion_tokens: sum("completion_tokens"),
    reasoning_tokens: sum("reasoning_tokens"),
    cost_usd: +sum("cost_usd").toFixed(6),
    usage,
    final_message: last ? last.message : null,
    page_dialogs: dialogs,
    plan: last && last.plan ? last.plan.items.map((i) => `${i.status}: ${i.title}`) : [],
    human_actions: human ? human.actions : [],
    turns,
    screenshots: shots.filter(Boolean),
  };
  fs.writeFileSync(path.join(dir, "result.json"), JSON.stringify(result, null, 2));
  const okChecks = verdict.checks.filter((c) => c.ok).length;
  log(`${result.pass ? "PASS" : "FAIL"} ${scenario.id}/${mode}: ${okChecks}/${verdict.checks.length} checks, ${result.micro_turns} turns, ${result.model_calls} calls, ${result.prompt_tokens + result.completion_tokens} tokens, $${result.cost_usd}, ${result.wall_s}s${human ? `, ${human.actions.length} human actions` : ""}`);
  return result;
}

// ---------------------------------------------------------------- report

function report(results, health) {
  const md = [];
  const fmt = (n) => Number(n || 0).toLocaleString("en-US");
  md.push(`# Phalaenopsis eval: ${new Date().toISOString().slice(0, 16).replace("T", " ")} UTC`, "");
  md.push(`Provider ${health.provider}, interaction model \`${health.fast_model}\`, background model \`${health.model}\`, pace ${PACE}.`, "");
  md.push("| Scenario | Mode | Result | Checks | Micro-turns | Interrupts | Model calls | Prompt tok | Completion tok (reasoning) | Cost | Time | Human actions |");
  md.push("|---|---|---|---|---|---|---|---|---|---|---|---|");
  for (const r of results) {
    const ok = r.checks.filter((c) => c.ok).length;
    md.push(`| ${r.scenario} | ${r.mode} | ${r.pass ? "✅ pass" : "❌ fail"} | ${ok}/${r.checks.length} | ${r.micro_turns} | ${r.interrupts} | ${r.model_calls} | ${fmt(r.prompt_tokens)} | ${fmt(r.completion_tokens)} (${fmt(r.reasoning_tokens)}) | $${r.cost_usd.toFixed(4)} | ${r.wall_s}s | ${r.human_actions.length} |`);
  }
  const tot = (k) => results.reduce((a, r) => a + (r[k] || 0), 0);
  md.push(`| **total** | | ${results.filter((r) => r.pass).length}/${results.length} | | ${tot("micro_turns")} | ${tot("interrupts")} | ${tot("model_calls")} | ${fmt(tot("prompt_tokens"))} | ${fmt(tot("completion_tokens"))} (${fmt(tot("reasoning_tokens"))}) | $${tot("cost_usd").toFixed(4)} | ${tot("wall_s").toFixed(0)}s | ${results.reduce((a, r) => a + r.human_actions.length, 0)} |`, "");
  for (const r of results) {
    md.push(`## ${r.scenario} (${r.mode}): ${r.pass ? "pass" : "FAIL"}`, "", `${r.title}`, "");
    if (r.error) md.push(`Error: \`${r.error}\``, "");
    const bad = r.checks.filter((c) => !c.ok);
    md.push(bad.length ? `Failed checks: ${bad.map((c) => `${c.name} (got ${JSON.stringify(c.got)})`).join("; ")}` : "All checks passed.", "");
    md.push(`Agent's final message: ${r.final_message ? `"${r.final_message}"` : "none"}`, "");
    md.push(`Tokens: interaction ${fmt(r.usage.interaction?.prompt_tokens)} in / ${fmt(r.usage.interaction?.completion_tokens)} out over ${r.usage.interaction?.calls || 0} calls; background ${fmt(r.usage.background?.prompt_tokens)} in / ${fmt(r.usage.background?.completion_tokens)} out over ${r.usage.background?.calls || 0} calls.`, "");
    if (r.human_actions.length) md.push(`Human did: ${r.human_actions.map((a) => `${a.t}s ${a.kind}${a.detail ? ` (${a.detail})` : ""}`).join("; ")}`, "");
    md.push(`Screenshots: ${r.screenshots.slice(-4).map((s) => `[${path.basename(s)}](${s})`).join(" ")}`, "");
  }
  fs.writeFileSync(path.join(OUT_DIR, "report.md"), md.join("\n"));
  fs.writeFileSync(path.join(OUT_DIR, "results.json"), JSON.stringify({ health, pace: PACE, results }, null, 2));

  // Contact sheet to flip through screenshots.
  const html = `<!doctype html><meta charset=utf-8><title>Phalaenopsis eval</title>
<style>body{font:14px system-ui;margin:20px;background:#faf9f5}h2{margin:28px 0 6px}.g{display:flex;flex-wrap:wrap;gap:8px}figure{margin:0}img{width:420px;border:1px solid #ccc;border-radius:6px}figcaption{font-size:12px;color:#555}</style>
<h1>Phalaenopsis eval</h1><p>Orange arrow: the agent's virtual cursor. Blue dot: the simulated human's real mouse (red when clicking).</p>
${results.map((r) => `<h2>${r.scenario} · ${r.mode} · ${r.pass ? "pass" : "FAIL"}</h2><div class=g>${r.screenshots.map((s) => `<figure><img src="${s}" loading=lazy><figcaption>${path.basename(s)}</figcaption></figure>`).join("")}</div>`).join("")}`;
  fs.writeFileSync(path.join(OUT_DIR, "index.html"), html);
}

// ---------------------------------------------------------------- main

const STAMP = new Date().toISOString().replace(/[:.]/g, "-").slice(0, 19);
const OUT_DIR = path.join(HERE, "results", STAMP);
fs.mkdirSync(OUT_DIR, { recursive: true });

const chosen = SCENARIOS.filter((s) => !ONLY || ONLY.includes(s.id));
if (!fs.existsSync(path.join(HERE, "sites", "repos"))) {
  console.error("Test sites missing. Run: bash eval/fetch-sites.sh");
  process.exit(1);
}

const site = await startStaticServer();
const { proc: backend, health } = await startBackend();
const chrome = await startChrome();
log(`backend ${health.provider} ${health.fast_model} / ${health.model}; chrome on ${chrome.port}; ${chosen.length} scenarios x ${MODES.join("+")}; output ${path.relative(ROOT, OUT_DIR)}`);

const results = [];
let i = 0;
try {
  for (const s of chosen) {
    for (const mode of MODES) {
      i++;
      log(`== ${s.id} (${mode})`);
      results.push(await runOne(chrome, s, mode, i));
      report(results, health); // keep the report current if interrupted
    }
  }
} finally {
  chrome.proc.kill();
  backend.kill();
  site.close();
  fs.rmSync(chrome.profile, { recursive: true, force: true });
}
report(results, health);
log(`done: ${results.filter((r) => r.pass).length}/${results.length} passed. Report: ${path.relative(ROOT, path.join(OUT_DIR, "report.md"))}`);
