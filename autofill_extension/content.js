// Content script: observes the page, executes step scripts, records actions.
(() => {
  if (window.__phalaenopsisLoaded) return;
  window.__phalaenopsisLoaded = true;

  let lastKey = null;
  let recording = false;
  // "Learn from user": moves not yet handed to the background worker.
  let pending = [];
  let flushTimer = null;
  let observeTimer = null;
  let executing = false;

  function send(msg) {
    return new Promise((resolve) => {
      try {
        chrome.runtime.sendMessage(msg, (resp) => {
          void chrome.runtime.lastError;
          resolve(resp);
        });
      } catch (_) {
        resolve(undefined);
      }
    });
  }

  function observeNow(force) {
    const page = PhalFP.snapshot();
    const key = PhalFP.fieldsKey(page.fields) + "\n" + location.href.split("#")[0];
    if (!force && key === lastKey) return;
    lastKey = key;
    // Moves made before this page change belong to the previous page node.
    flushMoves();
    // Fieldless pages (e.g. a list with an "Add New" link) only match nodes
    // the agent already created; they never create new ones.
    send({ type: "observe", page, create: page.fields.length > 0 });
  }

  function scheduleObserve() {
    if (executing) return;
    clearTimeout(observeTimer);
    observeTimer = setTimeout(() => observeNow(false), 300);
  }

  // ---------- step execution ----------
  const SETTERS = {
    input: Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, "value").set,
    textarea: Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, "value").set,
    select: Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, "value").set,
  };

  function fire(el, type) {
    el.dispatchEvent(new Event(type, { bubbles: true, cancelable: true }));
  }

  const find = (selector) => PhalFP.find(selector);
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  // How long to give the page between actions. Real portals open dialogs,
  // load dependent dropdowns and validate fields asynchronously.
  const PACES = {
    fast: { step: 0, clickMin: 150, quiet: 150, settleMax: 1500, find: 3000, typeDelay: 0, cursor: 0.5 },
    normal: { step: 150, clickMin: 400, quiet: 300, settleMax: 4000, find: 6000, typeDelay: 0, cursor: 1 },
    careful: { step: 400, clickMin: 800, quiet: 600, settleMax: 8000, find: 10000, typeDelay: 35, cursor: 1.4 },
  };
  let pace = PACES.normal;

  // Requests the page itself has in flight (published by shared/netwatch.js).
  const inflight = () => Number(document.documentElement.getAttribute("data-phal-inflight") || 0);

  // Resolve once the page has no requests in flight and the DOM has gone
  // `quiet` ms without mutations, or after `max` ms regardless.
  function settle(quiet, max) {
    return new Promise((resolve) => {
      let timer = null;
      const start = Date.now();
      const arm = () => { clearTimeout(timer); timer = setTimeout(check, quiet); };
      const check = () => (inflight() > 0 ? arm() : done());
      const done = () => { mo.disconnect(); clearTimeout(timer); clearTimeout(cap); resolve(Date.now() - start); };
      const mo = new MutationObserver((records) => {
        // Our own inflight counter attribute is not page activity.
        if (records.every((r) => r.type === "attributes" && r.attributeName === "data-phal-inflight")) {
          if (inflight() === 0) arm();
          return;
        }
        arm();
      });
      mo.observe(document.documentElement, { childList: true, subtree: true, attributes: true, characterData: true });
      arm();
      const cap = setTimeout(done, max);
    });
  }

  function key(el, type, ch) {
    el.dispatchEvent(new KeyboardEvent(type, { key: ch, bubbles: true, cancelable: true }));
  }

  async function setNativeValue(el, value) {
    const tag = el.tagName.toLowerCase();
    const setter = SETTERS[tag] || ((v) => (el.value = v));
    el.scrollIntoView({ block: "center" });
    el.focus();
    fire(el, "focus");
    if (pace.typeDelay > 0 && value.length <= 200) {
      // Type character by character for typeahead/autocomplete widgets.
      setter.call(el, "");
      fire(el, "input");
      let typed = "";
      for (const ch of value) {
        typed += ch;
        key(el, "keydown", ch);
        setter.call(el, typed);
        el.dispatchEvent(new InputEvent("input", { bubbles: true, data: ch, inputType: "insertText" }));
        key(el, "keyup", ch);
        await sleep(pace.typeDelay);
      }
    } else {
      setter.call(el, value);
      el.dispatchEvent(new InputEvent("input", { bubbles: true, data: value, inputType: "insertText" }));
    }
    fire(el, "change");
    el.blur();
    fire(el, "focusout");
  }

  // Letters and digits only: compares what was meant with what the field shows,
  // ignoring formatting a mask adds ("(617) 555-0142" vs "6175550142").
  const alnum = (v) => String(v || "").replace(/[^0-9a-z]/gi, "").toLowerCase();
  const stuck = (el, value) => {
    const want = alnum(value), got = alnum(el.value);
    return want === "" ? true : got === want || (got.length >= want.length && got.includes(want));
  };

  // Emulate a keyboard: keydown, keypress (with legacy keyCode/charCode/which,
  // which input-mask plugins read), then insert the character only if the page
  // did not handle it itself (preventDefault), like the browser's default action.
  async function typeLikeKeyboard(el, value) {
    const setter = SETTERS[el.tagName.toLowerCase()] || ((v) => (el.value = v));
    el.scrollIntoView({ block: "center" });
    el.focus();
    fire(el, "focus");
    await sleep(60); // mask plugins move the caret on focus, asynchronously
    try { el.setSelectionRange(0, el.value.length); } catch (_) {}
    for (const ch of value) {
      const code = ch.charCodeAt(0);
      const opts = { key: ch, bubbles: true, cancelable: true, keyCode: code, charCode: code, which: code };
      el.dispatchEvent(new KeyboardEvent("keydown", { ...opts, charCode: 0 }));
      const pressed = el.dispatchEvent(new KeyboardEvent("keypress", opts));
      if (pressed) {
        let start = el.value.length, end = el.value.length;
        try { start = el.selectionStart; end = el.selectionEnd; } catch (_) {}
        const next = el.value.slice(0, start) + ch + el.value.slice(end);
        setter.call(el, next);
        try { el.setSelectionRange(start + 1, start + 1); } catch (_) {}
        el.dispatchEvent(new InputEvent("input", { bubbles: true, data: ch, inputType: "insertText" }));
      }
      el.dispatchEvent(new KeyboardEvent("keyup", { ...opts, charCode: 0 }));
      await sleep(Math.max(15, pace.typeDelay || 0));
    }
    fire(el, "change");
    el.blur();
    fire(el, "focusout");
  }

  // Fill, then check the page kept the value. If a script or input mask wiped or
  // rejected it, type it key by key instead.
  async function fillAndVerify(el, value) {
    await setNativeValue(el, value);
    await sleep(Math.max(80, pace.step));
    if (stuck(el, value)) return;
    await typeLikeKeyboard(el, value);
    await sleep(150);
    if (!stuck(el, value)) {
      throw new Error(`the field did not accept the value (it shows ${JSON.stringify(el.value)})`);
    }
  }

  function waitFor(selector, timeoutMs) {
    const deadline = Date.now() + (timeoutMs || 5000);
    return new Promise((resolve, reject) => {
      const tick = () => {
        const el = find(selector);
        if (el) return resolve(el);
        if (Date.now() > deadline) return reject(new Error(`timeout waiting for ${selector}`));
        setTimeout(tick, 100);
      };
      tick();
    });
  }

  // Returns {value} or {skip: reason}.
  function resolveValue(ref, profile) {
    if (ref && typeof ref === "object") {
      if ("profile" in ref) {
        const v = profile[ref.profile];
        if (v === undefined || v === null || v === "") return { skip: `profile key "${ref.profile}" not set` };
        return { value: String(v) };
      }
      if ("literal" in ref) return { value: String(ref.literal) };
    }
    if (typeof ref === "string") return { value: ref };
    return { skip: "no value" };
  }

  async function selectOption(el, value) {
    // Dependent dropdowns often fill their options a moment after the previous choice.
    const deadline = Date.now() + pace.find;
    while (el.options.length <= 1 && Date.now() < deadline) await sleep(150);
    el.scrollIntoView({ block: "center" });
    const want = value.trim().toLowerCase();
    let match = null;
    for (const opt of el.options) {
      if (opt.value === value) { match = opt; break; }
    }
    if (!match) {
      for (const opt of el.options) {
        if (opt.value.trim().toLowerCase() === want || opt.text.trim().toLowerCase() === want) { match = opt; break; }
      }
    }
    if (!match) {
      for (const opt of el.options) {
        if (opt.text.trim().toLowerCase().startsWith(want)) { match = opt; break; }
      }
    }
    if (!match) throw new Error(`no option matching "${value}"`);
    el.focus();
    SETTERS.select.call(el, match.value);
    match.selected = true;
    fire(el, "input");
    fire(el, "change");
    el.blur();
    fire(el, "focusout");
  }

  const norm = (t) => String(t || "").replace(/\s+/g, " ").trim().toLowerCase();

  // Choose the option of a radio group whose label or value matches `value`.
  async function chooseRadio(anyRadio, value) {
    const group = anyRadio.name
      ? [...document.querySelectorAll(`input[type=radio][name="${CSS.escape(anyRadio.name)}"]`)]
      : [anyRadio];
    const want = norm(value);
    const label = (r) => norm(PhalFP.fieldSig(r).label);
    const pick =
      group.find((r) => norm(r.value) === want || label(r) === want) ||
      group.find((r) => label(r).startsWith(want) || want.startsWith(label(r)) && label(r).length > 0) ||
      group.find((r) => label(r).includes(want));
    if (!pick) throw new Error(`no radio option matching "${value}" (options: ${group.map((r) => PhalFP.fieldSig(r).label || r.value).join(", ")})`);
    touch(pick);
    await point(pick.labels && pick.labels[0] ? pick.labels[0] : pick, `Choose ${JSON.stringify(PhalFP.fieldSig(pick).label || value)}`);
    await tap();
    if (!pick.checked) pick.click();
    await settle(pace.quiet, pace.settleMax);
    if (!pick.checked) throw new Error(`radio option "${value}" did not stay selected`);
  }

  // Set any kind of field to a value, choosing the right mechanism for its type.
  async function setField(el, value) {
    touch(el);
    el.dataset.phalSet = "1"; // see page_default in shared/fingerprint.js
    const tag = el.tagName.toLowerCase();
    if (tag === "select") return selectOption(el, value);
    if (el.type === "radio") return chooseRadio(el, value);
    if (el.type === "checkbox") {
      const want = ["true", "yes", "1", "on", "checked"].includes(norm(value));
      if (el.checked !== want) el.click();
      await settle(pace.quiet, pace.settleMax);
      return;
    }
    if (tag === "input" || tag === "textarea" || el.isContentEditable) return fillAndVerify(el, value);
    throw new Error(`cannot set a value on <${tag}>`);
  }

  // Show the virtual cursor on the element about to be used (see shared/cursor.js).
  const fieldName = (el) => (PhalFP.fieldSig(el).label || el.getAttribute("name") || el.id || "").replace(/\s*\*\s*$/, "").slice(0, 40);
  // Elements the agent just acted on. Clicking a checkbox/radio from script makes
  // the browser fire a change event that is marked trusted, so trust alone would
  // mistake the agent's own clicks for the person's edits.
  const agentTouched = new WeakMap();
  const touch = (el) => { if (el) agentTouched.set(el, Date.now()); };

  async function point(el, text) {
    touch(el);
    if (typeof PhalCursor === "undefined" || !el) return;
    el.scrollIntoView({ block: "center", behavior: "instant" });
    await PhalCursor.moveTo(el, text, pace.cursor || 1);
  }
  const tap = async () => { if (typeof PhalCursor !== "undefined") await PhalCursor.click(pace.cursor || 1); };

  async function runStep(step, profile) {
    // Fields often appear a moment after a click (new row, dialog), so wait briefly.
    const need = async (sel) => {
      try {
        return await waitFor(sel, pace.find);
      } catch (_) {
        throw new Error(`element not found: ${sel}`);
      }
    };
    switch (step.op) {
      case "fill":
      case "select": {
        const r = resolveValue(step.value, profile);
        if (r.skip) return { skipped: r.skip };
        const el = await need(step.selector);
        if (el.type !== "radio") {
          await point(el, `${step.op === "fill" ? "Type" : "Choose"} ${JSON.stringify(r.value.slice(0, 30))} · ${fieldName(el)}`);
          await tap();
        }
        await setField(el, r.value);
        return {};
      }
      case "check": {
        const r = step.value === undefined ? { value: String(step.checked !== false) } : resolveValue(step.value, profile);
        if (r.skip) return { skipped: r.skip };
        const el = await need(step.selector);
        // "check" on a radio group with an option label means "choose that option".
        if (el.type === "radio" && !["true", "false", "yes", "no", "1", "0", "on", "off", "checked"].includes(norm(r.value))) {
          await chooseRadio(el, r.value);
          return {};
        }
        const want = ["true", "yes", "1", "on", "checked"].includes(r.value.trim().toLowerCase());
        if (el.checked !== want) {
          await point(el, `${want ? "Check" : "Uncheck"} · ${fieldName(el)}`);
          await tap();
          el.click();
          await settle(pace.quiet, pace.settleMax);
        }
        return {};
      }
      case "click": {
        const el = await need(step.selector);
        const text = (el.innerText || el.value || el.getAttribute("aria-label") || "").replace(/\s+/g, " ").trim().slice(0, 40);
        await point(el, `Click ${text ? JSON.stringify(text) : ""}`.trim());
        await sleep(Math.min(pace.step, 200));
        await tap();
        el.click();
        // Give the page time to react: at least clickMin, then until the DOM goes quiet.
        await sleep(pace.clickMin);
        await settle(pace.quiet, pace.settleMax);
        return {};
      }
      case "waitFor": {
        await waitFor(step.selector, step.timeout_ms);
        return {};
      }
      case "submit": {
        const el = await need(step.selector);
        const form = el.tagName.toLowerCase() === "form" ? el : el.form || el.closest("form");
        if (form && form.requestSubmit) form.requestSubmit();
        else el.click();
        return {};
      }
      case "navigate":
        location.href = step.url;
        return {};
      default:
        throw new Error(`unknown op ${step.op}`);
    }
  }

  // ---------- micro-turn perception ----------
  // After every action the executor looks at the page again, like an
  // interaction model taking in a new input chunk. Surprises interrupt the
  // batch so the agent reacts to what the page did instead of acting blind.
  const ERRORISH = /\b(required|not completed|invalid|error|must be|must have|please (enter|select|choose|complete|provide)|failed|cannot be|can't be|is missing|not valid|incorrect)\b/i;

  function perceive() {
    const lines = new Set((document.body ? document.body.innerText : "").split("\n").map((l) => l.replace(/\s+/g, " ").trim()).filter(Boolean));
    return { lines, dialogs: PhalFP.visibleDialogs().length, invalid: PhalFP.invalidCount(), fields: PhalFP.collectFields().length, url: location.href.split("#")[0] };
  }

  // Element whose own text contains `line` (first match), for styling checks.
  function elementWithText(line) {
    const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
    for (let n = walker.nextNode(), i = 0; n && i < 20000; n = walker.nextNode(), i++) {
      const t = n.textContent.replace(/\s+/g, " ").trim();
      if (t && (t === line || (t.length > 8 && line.includes(t)) || t.includes(line))) return n.parentElement;
    }
    return null;
  }

  // Error wording alone is not enough ("please select no" is help text);
  // the page must also present it as an error: red text or an alert/error container.
  function looksLikeError(line) {
    if (line.length >= 300 || !ERRORISH.test(line)) return false;
    const el = elementWithText(line);
    if (!el) return false;
    if (el.closest("[role=alert], [aria-live=assertive], [aria-live=polite], [class*=error i], [class*=invalid i], [class*=alert i]")) return true;
    const m = window.getComputedStyle(el).color.match(/\d+/g);
    if (m) {
      const [r, g, b] = m.map(Number);
      if (r >= 150 && g <= 90 && b <= 90) return true;
    }
    return false;
  }

  const literalOf = (step) => (step && step.value && typeof step.value === "object" && "literal" in step.value ? String(step.value.literal) : "");

  // Compare two perceptions; returns events and, if the page objected, why.
  function delta(before, after, step, t0) {
    const t_ms = Math.round(performance.now() - t0);
    const typed = literalOf(step).trim();
    const appeared = [...after.lines].filter((l) => !before.lines.has(l) && l !== typed);
    const events = [];
    let interrupt = null;
    const errors = appeared.filter(looksLikeError);
    for (const l of errors.slice(0, 4)) events.push({ t_ms, kind: "text", detail: l });
    if (errors.length) interrupt = `page showed: ${errors[0]}`;
    const other = appeared.filter((l) => !errors.includes(l)).slice(0, 3);
    for (const l of other) events.push({ t_ms, kind: "text", detail: l.slice(0, 160) });
    if (after.fields > before.fields) events.push({ t_ms, kind: "fields_appeared", detail: `${after.fields - before.fields} new field(s)` });
    if (after.dialogs > before.dialogs) events.push({ t_ms, kind: "dialog_opened", detail: "" });
    if (after.dialogs < before.dialogs) events.push({ t_ms, kind: "dialog_closed", detail: "" });
    if (after.invalid > before.invalid) {
      events.push({ t_ms, kind: "field_error", detail: `${after.invalid - before.invalid} field(s) flagged` });
      interrupt = interrupt || "the page flagged a field as invalid";
    }
    if (after.url !== before.url) {
      events.push({ t_ms, kind: "navigated", detail: after.url });
      interrupt = interrupt || "the page navigated";
    }
    return { events, interrupt };
  }

  async function executeSteps(steps, profile, _continueOnError, paceName, cursorOn) {
    pace = PACES[paceName] || PACES.normal;
    if (typeof PhalCursor !== "undefined") PhalCursor.setEnabled(cursorOn !== false);
    executing = true;
    const t0 = performance.now();
    const result = { ok: true, ran: 0, skipped: [], failed_step: null, error: null, results: [], events: [], interrupted: null };
    const stopRest = (from, why) => {
      for (let j = from; j < steps.length; j++) result.results.push({ ok: false, skipped: true, error: `not run: ${why}` });
    };
    try {
      let before = perceive();
      for (let i = 0; i < steps.length; i++) {
        let failed = null;
        try {
          const r = await runStep(steps[i], profile || {});
          if (r && r.skipped) {
            result.skipped.push({ step: i, reason: r.skipped });
            result.results.push({ ok: false, skipped: true, error: r.skipped });
          } else {
            result.ran++;
            result.results.push({ ok: true });
          }
          if (pace.step) await sleep(pace.step);
        } catch (e) {
          failed = String(e && e.message ? e.message : e);
          result.results.push({ ok: false, error: failed });
          result.ok = false;
          result.failed_step = i;
          result.error = failed;
          result.events.push({ t_ms: Math.round(performance.now() - t0), kind: "step_failed", detail: failed });
        }
        const after = perceive();
        const d = delta(before, after, steps[i], t0);
        result.events.push(...d.events);
        before = after;
        const why = failed ? `step ${i + 1} failed` : d.interrupt;
        if (why && i < steps.length - 1) {
          result.interrupted = why;
          stopRest(i + 1, why);
          break;
        }
        if (why && !result.interrupted) result.interrupted = why;
      }
      // Let the last action's effects land before anyone snapshots the page.
      await settle(pace.quiet * 2, pace.settleMax);
      const tail = delta(before, perceive(), null, t0);
      result.events.push(...tail.events);
      if (tail.interrupt && !result.interrupted) result.interrupted = tail.interrupt;
    } finally {
      executing = false;
      if (typeof PhalCursor !== "undefined") PhalCursor.idle();
    }
    if (result.ok && result.ran === 0 && steps.length > 0) {
      result.ok = false;
      result.error = "every step was skipped (missing profile values?)";
    }
    // A page objection means the batch did not achieve its goal, even if each step "worked".
    if (result.interrupted && result.ok) {
      result.ok = false;
      result.error = result.error || result.interrupted;
    }
    return result;
  }

  // ---------- recording ("Learn from user") ----------
  // Only real, trusted events from the person count; the agent's own actions
  // dispatch untrusted events (or touch the element first, for toggles).
  function record(op, ev) {
    if (!recording || executing || !ev.isTrusted) return;
    const el = ev.target;
    if (!(el instanceof Element)) return;
    const touchedAt = agentTouched.get(el);
    if (touchedAt && Date.now() - touchedAt < 2500) return;
    const tag = el.tagName.toLowerCase();
    const isField = tag === "input" || tag === "select" || tag === "textarea";
    if (op === "click") {
      const clickable = el.closest("button, a, [role=button], input[type=submit], input[type=button], input[type=checkbox], input[type=radio], label");
      if (!clickable) return;
      const label = (clickable.innerText || clickable.value || clickable.getAttribute("aria-label") || "").replace(/\s+/g, " ").trim().slice(0, 60);
      pending.push({ op, selector: PhalFP.selectorFor(clickable), value: null, label, ts: Date.now() });
      flushMoves(); // a click may navigate away; hand it over now
      return;
    }
    if (!isField) return;
    if (el.type === "password") return; // never store passwords in the graph
    let value = el.value;
    if (el.type === "checkbox" || el.type === "radio") value = el.checked ? "true" : "false";
    const selector = PhalFP.selectorFor(el);
    const label = PhalFP.fieldSig(el).label || el.getAttribute("name") || "";
    const last = pending[pending.length - 1];
    // Typing fires one input event per key; keep only the latest value.
    if (op === "input" && last && last.op === "input" && last.selector === selector) pending.pop();
    pending.push({ op, selector, value, label: label.slice(0, 60), ts: Date.now() });
    clearTimeout(flushTimer);
    if (op === "change") flushMoves();
    else flushTimer = setTimeout(flushMoves, 500);
  }

  function flushMoves() {
    clearTimeout(flushTimer);
    if (!pending.length) return;
    send({ type: "learnActions", actions: pending.splice(0) });
  }

  // Edits the person makes themselves while the agent is working. Everything the
  // agent does dispatches untrusted events, so isTrusted alone tells them apart,
  // even mid-batch. Reported with the next snapshot so the agent respects them.
  const userEdits = new Map();
  const noteUserEdit = (e) => {
    if (!e.isTrusted) return;
    // Only checkbox/radio clicks from script produce trusted change events;
    // typed text from the agent never does, so real keystrokes always count.
    const toggle = e.target.type === "checkbox" || e.target.type === "radio";
    const touchedAt = agentTouched.get(e.target);
    if (toggle && touchedAt && Date.now() - touchedAt < 2500) return;
    const el = e.target;
    if (!(el instanceof Element) || !el.matches("input, select, textarea")) return;
    if (el.type === "password") return;
    const sig = PhalFP.fieldSig(el);
    const value = el.type === "checkbox" || el.type === "radio" ? String(el.checked) : String(el.value).slice(0, 200);
    userEdits.set(sig.selector, { selector: sig.selector, label: sig.label, value });
  };
  document.addEventListener("input", noteUserEdit, true);
  document.addEventListener("change", noteUserEdit, true);
  document.addEventListener("input", (e) => record("input", e), true);
  document.addEventListener("change", (e) => record("change", e), true);
  document.addEventListener("click", (e) => record("click", e), true);

  window.addEventListener("pagehide", () => { if (recording) flushMoves(); });

  // ---------- messages ----------
  chrome.runtime.onMessage.addListener((msg, _sender, respond) => {
    switch (msg.type) {
      case "execute":
        executeSteps(msg.steps || [], msg.profile || {}, !!msg.continueOnError, msg.pace, msg.cursor).then(respond);
        return true;
      case "agentSnapshot": {
        const snap = PhalFP.agentSnapshot();
        snap.user_edits = [...userEdits.values()];
        userEdits.clear();
        respond(snap);
        return false;
      }
      case "snapshot":
        respond(PhalFP.snapshot());
        return false;
      case "reobserve":
        observeNow(true);
        respond({ ok: true });
        return false;
      case "setRecording":
        recording = !!msg.on;
        if (!recording) { clearTimeout(flushTimer); pending = []; }
        respond({ ok: true });
        return false;
      case "takeRecording":
        // Hand back unsent moves directly so stopping never races a flush.
        clearTimeout(flushTimer);
        respond({ actions: pending.splice(0) });
        return false;
      default:
        return false;
    }
  });

  // ---------- boot ----------
  send({ type: "getMode" }).then((resp) => {
    recording = !!(resp && resp.learning);
    observeNow(true);
  });
  const mo = new MutationObserver(scheduleObserve);
  mo.observe(document.documentElement, { childList: true, subtree: true, attributes: true, attributeFilter: ["style", "class", "hidden", "disabled"] });
  window.addEventListener("popstate", () => setTimeout(() => observeNow(true), 50));
  window.addEventListener("hashchange", () => setTimeout(() => observeNow(true), 50));
})();
