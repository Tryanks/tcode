// Console capture from the page's own script world, for engines with no
// native source. `emit` takes one entry. Entries are `source: script`: they
// miss what runs before document start and anything a page does to undo the
// hooks.
(function (emit) {
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
})
