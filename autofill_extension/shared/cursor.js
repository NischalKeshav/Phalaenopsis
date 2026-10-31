// Virtual cursor: shows where the agent is about to act, like Claude in Chrome.
// It lives in a closed shadow root on <html> with pointer-events: none, so the
// page cannot see or click it, and it stays out of document.body.innerText
// (which the agent's perception reads). The label text is drawn with CSS
// `content: attr(...)`, which never shows up in innerText either.
var PhalCursor = (() => {
  let host = null;
  let root = null;
  let pos = null;
  let hideTimer = null;
  let enabled = true;

  const CSS = `
    :host { all: initial; }
    .layer { position: fixed; inset: 0; pointer-events: none; z-index: 2147483647; }
    .cursor {
      position: fixed; left: 0; top: 0; width: 22px; height: 22px; opacity: 0;
      transition-property: transform, opacity; transition-timing-function: cubic-bezier(.2,.7,.2,1);
      filter: drop-shadow(0 1px 2px rgba(0,0,0,.45)); will-change: transform;
    }
    .cursor.on { opacity: 1; }
    .label {
      position: absolute; left: 20px; top: 18px; white-space: nowrap;
      font: 600 11px/1.2 system-ui, sans-serif; color: #fff; background: #c96442;
      padding: 3px 7px; border-radius: 6px; max-width: 320px; overflow: hidden; text-overflow: ellipsis;
    }
    .label::after { content: attr(data-text); }
    .label:not([data-text]), .label[data-text=""] { display: none; }
    .ring {
      position: fixed; width: 34px; height: 34px; margin: -17px 0 0 -17px; border-radius: 50%;
      border: 2px solid #c96442; opacity: 0; transform: scale(.4); pointer-events: none;
    }
    .ring.go { animation: ripple .45s ease-out; }
    @keyframes ripple { 0% { opacity: .9; transform: scale(.4); } 100% { opacity: 0; transform: scale(1.6); } }
    .box {
      position: fixed; border: 2px solid rgba(201,100,66,.85); border-radius: 6px; opacity: 0;
      box-shadow: 0 0 0 4px rgba(201,100,66,.18); transition: all .18s ease-out; pointer-events: none;
    }
    .box.on { opacity: 1; }
  `;
  const ARROW = `<svg width="22" height="22" viewBox="0 0 22 22"><path d="M3 2 L3 18 L7.5 14 L10.5 20.5 L13.2 19.3 L10.3 13 L16.5 13 Z" fill="#111" stroke="#fff" stroke-width="1.6" stroke-linejoin="round"/></svg>`;

  function ensure() {
    if (host && host.isConnected) return;
    host = document.createElement("phal-cursor");
    root = host.attachShadow({ mode: "closed" });
    root.innerHTML = `<style>${CSS}</style><div class="layer"><div class="box"></div><div class="ring"></div><div class="cursor">${ARROW}<div class="label"></div></div></div>`;
    document.documentElement.appendChild(host);
    if (!pos) pos = { x: window.innerWidth * 0.5, y: window.innerHeight * 0.35 };
    place(pos.x, pos.y, 0);
  }

  const $ = (sel) => root.querySelector(sel);
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

  function place(x, y, ms) {
    const c = $(".cursor");
    c.style.transitionDuration = `${ms}ms, 150ms`;
    c.style.transform = `translate(${x - 3}px, ${y - 2}px)`;
    pos = { x, y };
  }

  /** Glide to an element, outline it and show what is about to happen. */
  async function moveTo(el, text, speed = 1) {
    if (!enabled || !el || !el.getBoundingClientRect) return;
    ensure();
    clearTimeout(hideTimer);
    const r = el.getBoundingClientRect();
    // Aim a little inside the element, like a person would.
    const x = r.left + Math.min(r.width * 0.5, 40);
    const y = r.top + Math.min(r.height * 0.5, 18);
    const dist = Math.hypot(x - pos.x, y - pos.y);
    const ms = Math.round(Math.min(520, Math.max(140, dist * 0.6)) * speed);
    const box = $(".box");
    box.style.left = `${r.left - 4}px`;
    box.style.top = `${r.top - 4}px`;
    box.style.width = `${r.width + 8}px`;
    box.style.height = `${r.height + 8}px`;
    box.classList.add("on");
    $(".label").setAttribute("data-text", text || "");
    $(".cursor").classList.add("on");
    place(x, y, ms);
    await sleep(ms);
  }

  /** Click ripple at the current position. */
  async function click(speed = 1) {
    if (!enabled || !root) return;
    const ring = $(".ring");
    ring.style.left = `${pos.x}px`;
    ring.style.top = `${pos.y}px`;
    ring.classList.remove("go");
    void ring.offsetWidth; // restart the animation
    ring.classList.add("go");
    await sleep(Math.round(110 * speed));
  }

  /** Fade out after a pause; called when a batch ends. */
  function idle(delay = 1200) {
    if (!root) return;
    clearTimeout(hideTimer);
    hideTimer = setTimeout(() => {
      if (!root) return;
      $(".cursor").classList.remove("on");
      $(".box").classList.remove("on");
      $(".label").setAttribute("data-text", "");
    }, delay);
  }

  function setEnabled(on) {
    enabled = on !== false;
    if (!enabled && host) host.remove();
  }

  return { moveTo, click, idle, setEnabled };
})();
