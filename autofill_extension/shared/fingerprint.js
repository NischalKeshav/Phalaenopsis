// Page fingerprinting in the DOM. Mirrors src/fingerprint.rs: the backend
// hashes the field *signatures* we produce here, so they must be stable across
// visits (prefer ids/names over positional selectors).
var PhalFP = (() => {
  const FIELD_SELECTOR = "input, select, textarea";
  const SKIP_TYPES = new Set(["hidden", "submit", "button", "reset", "image", "file"]);

  function esc(s) {
    return typeof CSS !== "undefined" && CSS.escape ? CSS.escape(s) : s.replace(/([^\w-])/g, "\\$1");
  }

  function isVisible(el) {
    if (el.disabled) return false;
    if (el.type === "hidden") return false;
    const style = window.getComputedStyle(el);
    if (style.display === "none" || style.visibility === "hidden") return false;
    // Custom-styled checkboxes/radios are often visually hidden themselves but
    // shown through their label; inside a hidden container both have no boxes.
    if (el.type === "checkbox" || el.type === "radio") {
      if (el.getClientRects().length) return true;
      const lab = el.labels && el.labels[0];
      return !!(lab && lab.getClientRects().length);
    }
    const rects = el.getClientRects();
    return rects.length > 0 && (rects[0].width > 0 || rects[0].height > 0);
  }

  function unique(selector, el) {
    try {
      const found = document.querySelectorAll(selector);
      return found.length === 1 && found[0] === el;
    } catch (_) {
      return false;
    }
  }

  // Stable selector preference: #id > tag[name] (+[value] for radios) > [data-testid] > nth-of-type path.
  function selectorFor(el) {
    const tag = el.tagName.toLowerCase();
    if (el.id && !/^\d/.test(el.id) && !/\d{4,}/.test(el.id)) {
      const s = "#" + esc(el.id);
      if (unique(s, el)) return s;
    }
    const name = el.getAttribute("name");
    if (name) {
      let s = `${tag}[name="${esc(name)}"]`;
      if (el.type === "radio" || el.type === "checkbox") {
        const v = el.getAttribute("value");
        if (v != null) s += `[value="${esc(v)}"]`;
      }
      if (unique(s, el)) return s;
    }
    for (const attr of ["data-testid", "data-test", "data-qa", "aria-label"]) {
      const v = el.getAttribute(attr);
      if (v) {
        const s = `${tag}[${attr}="${esc(v)}"]`;
        if (unique(s, el)) return s;
      }
    }
    // Positional path up to the nearest ancestor with a stable id.
    const parts = [];
    let node = el;
    while (node && node.nodeType === 1 && node !== document.body) {
      const t = node.tagName.toLowerCase();
      if (node.id && !/\d{4,}/.test(node.id) && unique("#" + esc(node.id), node)) {
        parts.unshift("#" + esc(node.id));
        break;
      }
      let idx = 1;
      let sib = node;
      while ((sib = sib.previousElementSibling)) if (sib.tagName === node.tagName) idx++;
      parts.unshift(`${t}:nth-of-type(${idx})`);
      node = node.parentElement;
    }
    return parts.join(" > ");
  }

  function text(node) {
    return (node && node.textContent ? node.textContent : "").replace(/\s+/g, " ").trim().slice(0, 120);
  }

  function labelFor(el) {
    if (el.id) {
      const lab = document.querySelector(`label[for="${esc(el.id)}"]`);
      if (lab) return text(lab);
    }
    const aria = el.getAttribute("aria-label");
    if (aria) return aria.trim();
    const labelled = el.getAttribute("aria-labelledby");
    if (labelled) {
      const t = labelled
        .split(/\s+/)
        .map((id) => text(document.getElementById(id)))
        .filter(Boolean)
        .join(" ");
      if (t) return t;
    }
    const wrap = el.closest("label");
    if (wrap) return text(wrap);
    // Common pattern: <div><label>Text</label><input></div> without `for`.
    const parent = el.parentElement;
    if (parent) {
      const sibLabel = parent.querySelector("label");
      if (sibLabel && !sibLabel.htmlFor) return text(sibLabel);
    }
    return contextLabel(el);
  }

  // Text beside the field in its row/cell when there is no <label> at all
  // (grid or table layouts such as "<div>Level *</div><div><select>"):
  // climb a few levels and read siblings that do not contain other inputs.
  function contextLabel(el) {
    let inner = el;
    for (let i = 0; i < 4; i++) {
      const p = inner.parentElement;
      if (!p || p === document.body) break;
      const parts = [];
      for (const n of p.childNodes) {
        if (n === inner) continue;
        if (n.nodeType === 3) parts.push(n.textContent);
        else if (n.nodeType === 1 && !["SCRIPT", "STYLE", "OPTION"].includes(n.tagName) && !n.querySelector("input, select, textarea")) parts.push(n.innerText || "");
      }
      const t = parts.join(" ").replace(/\s+/g, " ").trim();
      if (t) return t.slice(0, 200);
      inner = p;
    }
    return "";
  }

  function fieldSig(el) {
    const tag = el.tagName.toLowerCase();
    return {
      selector: selectorFor(el),
      tag,
      input_type: tag === "input" ? (el.getAttribute("type") || "text").toLowerCase() : "",
      name: el.getAttribute("name") || "",
      id: /\d{4,}/.test(el.id || "") ? "" : el.id || "",
      label: labelFor(el),
      placeholder: el.getAttribute("placeholder") || "",
      autocomplete: el.getAttribute("autocomplete") || "",
    };
  }

  function collectFields() {
    const out = [];
    for (const el of document.querySelectorAll(FIELD_SELECTOR)) {
      const type = (el.getAttribute("type") || "").toLowerCase();
      if (SKIP_TYPES.has(type)) continue;
      if (!isVisible(el)) continue;
      out.push(fieldSig(el));
    }
    return out;
  }

  function snapshot() {
    return { url: location.href, title: document.title || "", fields: collectFields() };
  }

  // ---------- agent view: values, options, clickables, text ----------
  const CLICKABLE = "button, a[href], a[onclick], [role=button], input[type=submit], input[type=button]";

  function clickText(el) {
    const t = el.tagName.toLowerCase() === "input" ? el.value || "" : el.innerText || el.textContent || el.getAttribute("aria-label") || "";
    return t.replace(/\s+/g, " ").trim().slice(0, 80);
  }

  function visibleClickables() {
    return [...document.querySelectorAll(CLICKABLE)].filter((el) => {
      if (el.disabled) return false;
      const st = window.getComputedStyle(el);
      if (st.display === "none" || st.visibility === "hidden") return false;
      const r = el.getClientRects();
      return r.length > 0 && (r[0].width > 0 || r[0].height > 0);
    });
  }

  // Buttons and links: #id > [name] > text=<label> when unique > positional path.
  function actionSelector(el, all) {
    if (el.id && !/\d{4,}/.test(el.id) && unique("#" + esc(el.id), el)) return "#" + esc(el.id);
    const name = el.getAttribute("name");
    const tag = el.tagName.toLowerCase();
    if (name && unique(`${tag}[name="${esc(name)}"]`, el)) return `${tag}[name="${esc(name)}"]`;
    const t = clickText(el);
    if (t && all.filter((o) => clickText(o) === t).length === 1) return "text=" + t;
    return selectorFor(el);
  }

  // Question text for a radio group: legend / aria label, else text next to the
  // group's container (e.g. "Year taken *" in the cell beside the options).
  function groupQuestion(radios) {
    const first = radios[0];
    const rg = first.closest("[role=radiogroup]");
    if (rg) {
      const t = rg.getAttribute("aria-label") || (rg.getAttribute("aria-labelledby") || "").split(/\s+/).map((id) => text(document.getElementById(id))).join(" ").trim();
      if (t) return t;
    }
    const fs = first.closest("fieldset");
    if (fs && fs.querySelector("legend") && radios.every((r) => fs.contains(r))) return text(fs.querySelector("legend"));
    let common = first.parentElement;
    while (common && !radios.every((r) => common.contains(r))) common = common.parentElement;
    let inner = common;
    for (let i = 0; inner && i < 5; i++) {
      const parent = inner.parentElement;
      if (!parent) break;
      const extra = (parent.innerText || "").replace(inner.innerText || "", "").replace(/\s+/g, " ").trim();
      if (extra) return extra.slice(0, 200);
      inner = parent;
    }
    return "";
  }

  function isRequired(el, label) {
    return !!(el.required || el.getAttribute("aria-required") === "true" || /\*\s*$|\*\s/.test(label || ""));
  }

  function isInvalid(el) {
    if (el.getAttribute("aria-invalid") === "true") return true;
    try { if (el.matches(":user-invalid")) return true; } catch (_) {}
    let n = el;
    for (let i = 0; n && i < 4; i++, n = n.parentElement) {
      if (/(^|[\s_-])(error|invalid|has-error|is-invalid)([\s_-]|$)/i.test(n.className || "")) return true;
    }
    return false;
  }

  // Visible text lines from the previous agent snapshot, to report what is new.
  let lastLines = null;

  function visibleDialogs() {
    return [...document.querySelectorAll("dialog[open], [role=dialog], [role=alertdialog], [aria-modal=true], .modal, .ui-dialog")].filter((d) => {
      const r = d.getClientRects();
      return r.length && r[0].width > 0 && window.getComputedStyle(d).visibility !== "hidden" && window.getComputedStyle(d).display !== "none";
    });
  }

  function agentSnapshot() {
    const fields = [];
    const radioGroups = new Map();
    for (const el of document.querySelectorAll(FIELD_SELECTOR)) {
      const type = (el.getAttribute("type") || "").toLowerCase();
      if (SKIP_TYPES.has(type) || !isVisible(el)) continue;
      if (type === "radio" && el.name) {
        if (!radioGroups.has(el.name)) {
          radioGroups.set(el.name, []);
          fields.push({ __radioGroup: el.name }); // placeholder keeps page order
        }
        radioGroups.get(el.name).push(el);
        continue;
      }
      const f = fieldSig(el);
      f.value = el.type === "checkbox" ? String(el.checked) : String(el.value || "").slice(0, 300);
      f.options = el.tagName.toLowerCase() === "select" ? [...el.options].slice(0, 120).map((o) => ({ value: o.value, text: o.text.trim() })) : [];
      f.required = isRequired(el, f.label);
      f.invalid = isInvalid(el);
      fields.push(f);
    }
    for (let i = 0; i < fields.length; i++) {
      const name = fields[i].__radioGroup;
      if (!name) continue;
      const radios = radioGroups.get(name);
      const question = groupQuestion(radios);
      const checked = radios.find((r) => r.checked);
      const optionText = (r) => labelFor(r).replace(/\s+/g, " ").trim() || r.value;
      fields[i] = {
        selector: `input[name="${esc(name)}"]`,
        tag: "radio-group",
        input_type: "radio",
        name,
        id: "",
        label: question,
        placeholder: "",
        autocomplete: "",
        value: checked ? optionText(checked) : "",
        options: radios.map((r) => ({ value: r.value, text: optionText(r) })),
        required: radios.some((r) => r.required) || /\*/.test(question),
        invalid: radios.some(isInvalid),
      };
    }
    const clickables = visibleClickables();
    const actions = clickables.slice(0, 120).map((el) => ({ selector: actionSelector(el, clickables), tag: el.tagName.toLowerCase(), text: clickText(el) }));

    const bodyText = document.body ? document.body.innerText : "";
    const lines = bodyText.split("\n").map((l) => l.replace(/\s+/g, " ").trim()).filter(Boolean);
    const newText = lastLines ? [...new Set(lines.filter((l) => !lastLines.has(l)))].slice(0, 25) : [];
    lastLines = new Set(lines);
    // Open dialogs first so their text (and any error inside) is never cut off.
    const dialogs = visibleDialogs().map((d) => (d.innerText || "").replace(/\n{2,}/g, "\n").trim()).filter(Boolean);
    const text = (dialogs.length ? "OPEN DIALOG:\n" + dialogs.join("\n---\n") + "\n\nPAGE:\n" : "") + bodyText.replace(/\n{3,}/g, "\n\n");
    return { url: location.href, title: document.title || "", fields, actions, text: text.slice(0, 12000), new_text: newText };
  }

  // Resolves CSS selectors and the `text=<label>` form used for buttons/links.
  function find(selector) {
    if (selector.startsWith("text=")) {
      const want = selector.slice(5).trim();
      return visibleClickables().find((el) => clickText(el) === want) || null;
    }
    try {
      return document.querySelector(selector);
    } catch (_) {
      return null;
    }
  }

  // Cheap identity for change detection between MutationObserver ticks.
  function fieldsKey(fields) {
    return fields.map((f) => `${f.tag}|${f.input_type}|${f.name}|${f.id}|${f.label}`).sort().join("\n");
  }

    function invalidCount() {
    let n = 0;
    for (const el of document.querySelectorAll(FIELD_SELECTOR)) if (isVisible(el) && isInvalid(el)) n++;
    // Rows/cells the page highlights as errors without marking the input itself.
    return n;
  }

  return { snapshot, agentSnapshot, find, collectFields, selectorFor, fieldSig, fieldsKey, visibleDialogs, invalidCount };
})();
