// Console and network capture from the page's own script world, for engines
// with no native source of either. `emit` takes one entry; `capture` says
// which of { console, network } to hook. Entries are `source: script`: they
// miss what runs before document start, service-worker traffic, and anything
// a page does to undo the hooks.
(function (emit, capture) {
  "use strict";
  const now = () => (typeof performance !== "undefined" ? performance.now() : Date.now());
  const location = (frame) => (frame && frame.url ? frame.url + ":" + frame.line : undefined);
  const text = (value) => {
    if (typeof value === "string") return value;
    if (value instanceof Error) return value.stack || String(value);
    try {
      const json = JSON.stringify(value);
      return json === undefined ? String(value) : json;
    } catch (_) {
      return String(value);
    }
  };
  // Where a console call came from: the first stack frame with a URL. This
  // script was injected and has none.
  const caller = () => {
    for (const entry of String(new Error().stack || "").split("\n")) {
      const match = /(\S+?):(\d+):\d+\)?$/.exec(entry.trim());
      if (!match) continue;
      const url = match[1].replace(/^.*?@/, "").replace(/^\(/, "");
      if (/^[a-z][\w+.-]*:\/\//i.test(url)) return { url, line: match[2] };
    }
    return null;
  };

  if (capture.console) {
    for (const level of ["log", "info", "warn", "error", "debug"]) {
      const original = console[level];
      if (typeof original !== "function") continue;
      console[level] = function () {
        try {
          emit({ t: "console", level, text: Array.from(arguments).map(text).join(" "), location: location(caller()) });
        } catch (_) {}
        return original.apply(this, arguments);
      };
    }
    window.addEventListener("error", (event) => {
      if (event.target !== window && event.target && event.target.nodeType === 1) return;
      emit({
        t: "console",
        level: "error",
        text: event.error ? text(event.error) : event.message,
        location: event.filename ? event.filename + ":" + event.lineno : undefined,
      });
    });
    window.addEventListener("unhandledrejection", (event) => {
      emit({ t: "console", level: "error", text: "Unhandled promise rejection: " + text(event.reason) });
    });
  }

  if (capture.network) {
    const absolute = (url) => {
      try {
        return new URL(String(url), document.baseURI).href;
      } catch (_) {
        return String(url);
      }
    };
    const fetch = window.fetch;
    if (typeof fetch === "function") {
      window.fetch = function (input, init) {
        const started = now();
        const method = String((init && init.method) || (input && input.method) || "GET").toUpperCase();
        const url = absolute(input && input.url ? input.url : input);
        return fetch.apply(this, arguments).then(
          (response) => {
            emit({ t: "network", method, url: response.url || url, status: response.status, kind: "fetch", ms: now() - started });
            return response;
          },
          (error) => {
            emit({ t: "network", method, url, kind: "fetch", ms: now() - started, failed: String((error && error.message) || error) });
            throw error;
          }
        );
      };
    }
    const XHR = window.XMLHttpRequest;
    if (XHR) {
      const open = XHR.prototype.open;
      const send = XHR.prototype.send;
      XHR.prototype.open = function (method, url) {
        this.__tcodeRequest = { method: String(method || "GET").toUpperCase(), url: absolute(url) };
        return open.apply(this, arguments);
      };
      XHR.prototype.send = function () {
        const request = this.__tcodeRequest;
        if (request) {
          const started = now();
          this.addEventListener("loadend", () => {
            const entry = { t: "network", method: request.method, url: this.responseURL || request.url, kind: "xhr", ms: now() - started };
            if (this.status) entry.status = this.status;
            else entry.failed = "request failed";
            emit(entry);
          });
        }
        return send.apply(this, arguments);
      };
    }
    // Everything else the document loads: scripts, styles, images, frames.
    if (typeof PerformanceObserver === "function") {
      try {
        new PerformanceObserver((list) => {
          for (const entry of list.getEntries()) {
            if (entry.initiatorType === "fetch" || entry.initiatorType === "xmlhttprequest") continue;
            const record = { t: "network", method: "GET", url: entry.name, kind: entry.initiatorType, ms: entry.duration };
            if (entry.responseStatus) record.status = entry.responseStatus;
            if (entry.transferSize) record.bytes = entry.transferSize;
            emit(record);
          }
        }).observe({ type: "resource", buffered: true });
      } catch (_) {}
    }
    window.addEventListener(
      "error",
      (event) => {
        const target = event.target;
        if (!target || target === window || target.nodeType !== 1) return;
        const url = target.currentSrc || target.src || target.href;
        if (url) emit({ t: "network", method: "GET", url: String(url), kind: target.localName, failed: "failed to load" });
      },
      true
    );
  }
})
