#!/usr/bin/env node
// End-to-end check for "Learn from user".
//
// Runs the real background.js (in Node, behind a small fake `chrome.*` API), the
// real content scripts (in headless Chrome), and the real backend (in-memory
// store, port 7797). A simulated person types and clicks with trusted DevTools
// input across form.html -> form2.html; the test then stops learning and checks
// what landed in the graph.
//
// Usage: cargo build && node eval/learn-from-user.mjs [--headed]
// No model key needed.

import { spawn } from "node:child_process";
import assert from "node:assert/strict";
import fs from "node:fs";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, "..");
const EXT = path.join(ROOT, "autofill_extension");
const BACKEND = "http://127.0.0.1:7797";
const SITE = "http://127.0.0.1:7798";
const TAB = 1;
const HEADED = process.argv.includes("--headed");
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const log = (...a) => console.log(new Date().toISOString().slice(11, 19), ...a);
const read = (p) => fs.readFileSync(path.join(EXT, p), "utf8");

// ------------------------------------------------------------ processes
function startSite() {
  const dir = path.join(EXT, "test");
  const server = http.createServer((req, res) => {
    const file = path.normalize(path.join(dir, new URL(req.url, SITE).pathname.replace(/^\/test\//, "")));
    if (!file.startsWith(dir)) return res.writeHead(404).end();
    fs.readFile(file, (err, data) => (err ? res.writeHead(404).end() : res.writeHead(200, { "content-type": "text/html" }).end(data)));
  });
  return new Promise((r) => server.listen(7798, "127.0.0.1", () => r(server)));
}

async function startBackend() {
  const bin = path.join(ROOT, "target/debug/phalaenopsis");
  if (!fs.existsSync(bin)) throw new Error("run `cargo build` first");
  const proc = spawn(bin, [], {
    cwd: ROOT,
    // No model: learning must work without one.
    env: { ...process.env, PHALAENOPSIS_STORE: "memory", PHALAENOPSIS_ADDR: "127.0.0.1:7797", OPENROUTER_KEY: "", OPENAI_KEY: "", RUST_LOG: "warn" },
    stdio: "ignore",
  });
  for (let i = 0; i < 80; i++) {
    try { if ((await fetch(BACKEND + "/v1/health")).ok) return proc; } catch (_) {}
    await sleep(250);
  }
  proc.kill();
  throw new Error("backend did not start");
}

async function startChrome() {
  const profile = fs.mkdtempSync(path.join(os.tmpdir(), "phal-learn-chrome-"));
  const proc = spawn(process.env.CHROME || "google-chrome", [
    `--user-data-dir=${profile}`, "--remote-debugging-port=0", "--no-first-run", "--no-default-browser-check",
    "--disable-extensions", "--window-size=1100,900", ...(HEADED ? [] : ["--headless=new"]), "about:blank",
  ], { stdio: "ignore" });
  const portFile = path.join(profile, "DevToolsActivePort");
  for (let i = 0; i < 100 && !fs.existsSync(portFile); i++) await sleep(100);
  return { proc, port: fs.readFileSync(portFile, "utf8").split("\n")[0].trim(), profile };
}

// ------------------------------------------------------------ CDP
class CDP {
  static async open(port) {
    const target = await (await fetch(`http://127.0.0.1:${port}/json/new?about:blank`, { method: "PUT" })).json();
    const cdp = new CDP(target.webSocketDebuggerUrl);
    await new Promise((res, rej) => { cdp.ws.addEventListener("open", res, { once: true }); cdp.ws.addEventListener("error", rej, { once: true }); });
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
      } else if (msg.method) for (const fn of this.listeners.get(msg.method) || []) fn(msg.params);
    });
  }
  send(method, params = {}) {
    const id = ++this.id;
    this.ws.send(JSON.stringify({ id, method, params }));
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      setTimeout(() => this.pending.has(id) && (this.pending.delete(id), reject(new Error(`CDP timeout: ${method}`))), 30000);
    });
  }
  on(method, fn) { (this.listeners.get(method) || this.listeners.set(method, []).get(method)).push(fn); }
}

async function evaluate(cdp, expression) {
  const r = await cdp.send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
  if (r.exceptionDetails) throw new Error(r.exceptionDetails.exception?.description || r.exceptionDetails.text);
  return r.result.value;
}

// Content scripts in every document; chrome.runtime.sendMessage goes to Node.
function bootScript() {
  const bundle = [read("shared/fingerprint.js"), read("shared/cursor.js"), read("content.js")].join("\n;\n");
  return `(() => {
  if (window.__phalBoot) return;
  window.__phalBoot = true;
  window.__bgCb = {};
  let seq = 0;
  window.chrome = window.chrome || {};
  window.chrome.runtime = {
    lastError: undefined,
    sendMessage: (m, cb) => { const id = ++seq; if (cb) window.__bgCb[id] = cb; window.__toBg(JSON.stringify({ id, msg: m })); },
    onMessage: { addListener: (fn) => { window.__phalListener = fn; } },
  };
  ${read("shared/netwatch.js")}
  const start = () => {
    (0, eval)(${JSON.stringify(bundle)});
    window.__phal = { call: (msg) => new Promise((res) => window.__phalListener(msg, {}, res)) };
  };
  if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", start, { once: true });
  else start();
})();`;
}

// ------------------------------------------------------------ fake chrome for background.js
function fakeChrome(cdp) {
  const local = { backendUrl: BACKEND, mode: "off" }; // learning must work with the mode Off
  const session = {};
  const pick = (store, keys) => {
    if (keys == null) return { ...store };
    if (typeof keys === "string") return keys in store ? { [keys]: store[keys] } : {};
    if (Array.isArray(keys)) return Object.fromEntries(keys.filter((k) => k in store).map((k) => [k, store[k]]));
    return Object.fromEntries(Object.entries(keys).map(([k, d]) => [k, k in store ? store[k] : d]));
  };
  const area = (store) => ({
    get: async (k) => structuredClone(pick(store, k)),
    set: async (o) => Object.assign(store, structuredClone(o)),
    remove: async (k) => [].concat(k).forEach((x) => delete store[x]),
  });
  const ev = () => { const fns = []; return { addListener: (f) => fns.push(f), fns }; };
  const badges = [];
  const panelMessages = [];
  const chrome = {
    storage: { local: area(local), session: area(session) },
    runtime: { onInstalled: ev(), onStartup: ev(), onMessage: ev(), sendMessage: (m) => { panelMessages.push(m); return Promise.resolve(); } },
    sidePanel: { setPanelBehavior: async () => {}, setOptions: async () => {}, open: async () => {} },
    action: { onClicked: ev(), setBadgeText: async ({ text }) => badges.push(text), setBadgeBackgroundColor: async () => {} },
    tabs: {
      onRemoved: ev(),
      onUpdated: ev(),
      query: async () => [{ id: TAB }],
      // background -> content script, via the page's registered listener.
      sendMessage: (tabId, msg, cb) => {
        evaluate(cdp, `window.__phal ? window.__phal.call(${JSON.stringify(msg)}) : undefined`)
          .then((r) => cb(r), () => cb(undefined));
      },
    },
    scripting: {},
  };
  return { chrome, badges, panelMessages, session };
}

function loadBackground(chrome) {
  new Function("chrome", read("background.js"))(chrome);
  const [listener] = chrome.runtime.onMessage.fns;
  // What the side panel does: message the worker and wait for the reply.
  const panel = (msg) => new Promise((resolve, reject) => {
    const keep = listener({ ...msg, tabId: TAB }, {}, (resp) => (resp && resp.ok === false ? reject(new Error(resp.error)) : resolve(resp && "data" in resp ? resp.data : resp)));
    if (keep !== true) resolve(undefined);
  });
  return { listener, panel };
}

// ------------------------------------------------------------ trusted input
async function center(cdp, selector) {
  const r = await evaluate(cdp, `(() => { const el = document.querySelector(${JSON.stringify(selector)}); el.scrollIntoView({block:"center"}); const b = el.getBoundingClientRect(); return { x: b.x + b.width / 2, y: b.y + b.height / 2 }; })()`);
  return r;
}
async function click(cdp, selector) {
  const { x, y } = await center(cdp, selector);
  for (const type of ["mouseMoved", "mousePressed", "mouseReleased"]) {
    await cdp.send("Input.dispatchMouseEvent", { type, x, y, button: "left", clickCount: 1 });
  }
  await sleep(150);
}
async function type(cdp, selector, text) {
  await click(cdp, selector);
  for (const ch of text) { await cdp.send("Input.insertText", { text: ch }); await sleep(30); }
  await sleep(100);
}
async function waitReady(cdp, urlPart) {
  for (let i = 0; i < 100; i++) {
    try { if (await evaluate(cdp, `location.href.includes(${JSON.stringify(urlPart)}) && document.readyState === "complete" && !!window.__phal`)) return; } catch (_) {}
    await sleep(150);
  }
  throw new Error(`never reached ${urlPart}`);
}

// ------------------------------------------------------------ test
async function main() {
  const site = await startSite();
  const backend = await startBackend();
  const chromeProc = await startChrome();
  let failed = false;
  try {
    const cdp = await CDP.open(chromeProc.port);
    const fake = fakeChrome(cdp);
    const { listener, panel } = loadBackground(fake.chrome);
    await cdp.send("Runtime.enable");
    await cdp.send("Page.enable");
    await cdp.send("Runtime.addBinding", { name: "__toBg" });
    // content script -> background.js, reply via the page callback.
    cdp.on("Runtime.bindingCalled", ({ name, payload }) => {
      if (name !== "__toBg") return;
      const { id, msg } = JSON.parse(payload);
      listener(msg, { tab: { id: TAB } }, (resp) => {
        evaluate(cdp, `window.__bgCb[${id}] && window.__bgCb[${id}](${JSON.stringify(resp ?? null)})`).catch(() => {});
      });
    });
    await cdp.send("Page.addScriptToEvaluateOnNewDocument", { source: bootScript() });
    await backendProfile({ firstName: "Ada" });

    log("open form.html");
    await cdp.send("Page.navigate", { url: `${SITE}/test/form.html` });
    await waitReady(cdp, "form.html");
    // Mode is Off, so nothing is observed until learning starts.
    assert.equal((await panel({ type: "learnStatus" })).on, false);

    log("start learning");
    await panel({ type: "learnStart" });
    await sleep(800); // reobserve -> backend node
    assert.equal((await panel({ type: "learnStatus" })).on, true);
    assert.ok(fake.badges.includes("REC"), "REC badge shown");

    // A password field the person types into must never be stored.
    await evaluate(cdp, `document.querySelector("#profileForm").insertAdjacentHTML("afterbegin", '<input id="pw" type="password">')`);
    await sleep(600);
    await type(cdp, "#pw", "hunter2");
    await type(cdp, "#firstName", "Ada");
    await type(cdp, "#lastName", "Lovelace");
    await click(cdp, "#firstGen");
    // The agent acting during a demonstration must not be learned as the person.
    await evaluate(cdp, `window.__phal.call({ type: "execute", steps: [{ op: "fill", selector: "#phone", value: { literal: "555-0000" } }], profile: {}, pace: "fast", cursor: false })`);

    const mid = await panel({ type: "learnStatus" });
    log(`mid-demo: ${mid.moves} moves on ${mid.pages} page(s)`);
    assert.ok(mid.moves >= 3, "moves stream to the worker while recording");

    log("submit -> form2.html");
    await click(cdp, "#next");
    await waitReady(cdp, "form2.html");
    await sleep(900); // new content script asks getMode -> learning, page observed
    await type(cdp, "#parent1Name", "Byron");
    await sleep(700); // typing flush is debounced

    const status = await panel({ type: "learnStatus" });
    log(`before save: ${status.moves} moves on ${status.pages} page(s)`);
    assert.equal(status.pages, 2, "moves split across both pages");

    log("stop & save");
    const { saved } = await panel({ type: "learnStop", save: true });
    assert.equal((await panel({ type: "learnStatus" })).on, false);
    assert.equal(saved.pages, 2);
    assert.equal(saved.scripts.length, 2);
    assert.equal(saved.transitions.length, 1);
    const [first, second] = saved.scripts;
    const describe = (s) => s.body.map((st) => `${st.op} ${st.selector}${st.value ? " = " + JSON.stringify(st.value) : ""}`);
    log("page 1 script:", describe(first));
    log("page 2 script:", describe(second));
    const steps1 = first.body;
    assert.ok(steps1.some((s) => s.op === "fill" && s.selector === "#firstName" && s.value.profile === "firstName"), "typed profile value generalized to {firstName}");
    assert.ok(steps1.some((s) => s.op === "fill" && s.selector === "#lastName" && s.value.literal === "Lovelace"));
    assert.ok(steps1.some((s) => s.op === "check" && s.selector === "#firstGen" && s.value.literal === "true"));
    assert.ok(steps1.some((s) => s.op === "click" && s.selector === "#next"), "navigating click kept");
    assert.ok(!steps1.some((s) => s.selector === "#phone"), "agent's own fill not learned");
    assert.ok(!JSON.stringify(saved).includes("hunter2") && !steps1.some((s) => s.selector === "#pw"), "password never stored");
    assert.ok(second.body.some((s) => s.op === "fill" && s.selector === "#parent1Name" && s.value.literal === "Byron"));
    assert.ok(saved.scripts.every((s) => s.origin === "recorded"));

    const graph = await (await fetch(BACKEND + "/v1/graph")).json();
    const t = graph.transitions.find((x) => x.via === first.id);
    assert.ok(t && t.from === first.observation_id && t.to === second.observation_id, "transition page1 -> page2 via the learned script");
    assert.equal(graph.scripts.filter((s) => s.origin === "recorded").length, 2);
    log(`graph: ${graph.observations.length} nodes, ${graph.scripts.length} scripts, ${graph.transitions.length} transitions`);

    // Discard path: nothing new is stored.
    await panel({ type: "learnStart" });
    await sleep(500);
    await type(cdp, "#parent1Occupation", "Poet");
    const discarded = await panel({ type: "learnStop", save: false });
    assert.equal(discarded.saved, null);
    const after = await (await fetch(BACKEND + "/v1/graph")).json();
    assert.equal(after.scripts.length, graph.scripts.length, "discard stores nothing");
    log("PASS");
  } catch (e) {
    failed = true;
    console.error("FAIL:", e.stack || e);
  } finally {
    const exited = new Promise((r) => chromeProc.proc.once("exit", r));
    chromeProc.proc.kill();
    backend.kill();
    site.close();
    await Promise.race([exited, sleep(3000)]);
    try { fs.rmSync(chromeProc.profile, { recursive: true, force: true, maxRetries: 3 }); } catch (_) {}
  }
  process.exit(failed ? 1 : 0);
}

async function backendProfile(profile) {
  await fetch(BACKEND + "/v1/profile", { method: "PUT", headers: { "content-type": "application/json" }, body: JSON.stringify(profile) });
}

main();
