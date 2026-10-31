// Runs in the page's MAIN world at document_start. Counts in-flight fetch/XHR
// requests and exposes the count on <html data-phal-inflight>, so the isolated
// content script can wait for the page's own server round trips to finish.
(() => {
  if (window.__phalNetwatch) return;
  window.__phalNetwatch = true;
  let inflight = 0;
  const root = () => document.documentElement;
  const publish = () => { const r = root(); if (r) r.setAttribute("data-phal-inflight", String(inflight)); };
  const begin = () => { inflight++; publish(); };
  const end = () => { inflight = Math.max(0, inflight - 1); publish(); };

  const origFetch = window.fetch;
  if (origFetch) {
    window.fetch = function (...args) {
      begin();
      let p;
      try { p = origFetch.apply(this, args); } catch (e) { end(); throw e; }
      // Count until the body is read too would be ideal; headers arriving is close enough.
      return p.finally(end);
    };
  }
  const origSend = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.send = function (...args) {
    begin();
    let done = false;
    const finish = () => { if (!done) { done = true; end(); } };
    this.addEventListener("loadend", finish);
    try { return origSend.apply(this, args); } catch (e) { finish(); throw e; }
  };
  publish();
})();
