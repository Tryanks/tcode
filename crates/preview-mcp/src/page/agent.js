// Tcode's page agent: installed at document start in the top frame, in a
// script world of Tcode's own where the engine has one. `post` sends one
// string to Tcode; `config` is { nonce, hooksEvent, hooks }.
//
// Every message carries the page's nonce and this document's id. Tcode calls
// `globalThis.__tcodePage.call(id, method, args)`; each call is answered once
// with a `reply` message, whenever its promise settles.
(function (post, config, hooks) {
  "use strict";
  if (window !== window.top) return;
  const NAME = "__tcodePage";
  if (Object.prototype.hasOwnProperty.call(globalThis, NAME)) return;

  const stringify = JSON.stringify;
  const doc = Math.random().toString(36).slice(2) + Date.now().toString(36);
  const send = (message) => {
    message.n = config.nonce;
    message.doc = doc;
    try {
      post(stringify(message));
    } catch (_) {}
  };
  const MAX_TEXT = 2000;
  const clip = (text, max) => {
    const value = String(text == null ? "" : text);
    return value.length > max ? value.slice(0, max) + "…" : value;
  };

  // Console and network entries the main-world hooks see.
  const relay = (entry) => {
    if (!entry || (entry.t !== "console" && entry.t !== "network")) return;
    const message = { t: entry.t };
    for (const key of ["level", "text", "location", "method", "url", "status", "kind", "ms", "bytes", "failed"]) {
      if (entry[key] != null) message[key] = typeof entry[key] === "string" ? clip(entry[key], MAX_TEXT) : entry[key];
    }
    send(message);
  };
  if (hooks) {
    hooks(relay, config.capture);
  } else if (config.hooksEvent) {
    window.addEventListener(config.hooksEvent, (event) => {
      try {
        relay(JSON.parse(event.detail));
      } catch (_) {}
    });
  }

  // The user's own input in the page: what the agent last read may no
  // longer be what is there. Synthetic events (the agent's) are untrusted.
  let generation = 0;
  let snapshotGeneration = 0;
  let reported = false;
  const userActed = (event) => {
    if (!event.isTrusted) return;
    generation += 1;
    if (!reported) {
      reported = true;
      send({ t: "input", generation });
    }
  };
  for (const type of ["pointerdown", "keydown", "paste", "drop", "touchstart"]) {
    window.addEventListener(type, userActed, { capture: true, passive: true });
  }

  // Same-document URL changes (history API, fragments), which no new
  // document announces.
  let href = location.href;
  const checkUrl = () => {
    if (location.href !== href) {
      href = location.href;
      send({ t: "url", url: href });
    }
  };
  window.addEventListener("popstate", checkUrl, true);
  window.addEventListener("hashchange", checkUrl, true);
  const watchUrl = () => new MutationObserver(checkUrl).observe(document, { childList: true, subtree: true });
  if (document.documentElement) watchUrl();
  else document.addEventListener("readystatechange", watchUrl, { once: true });

  const parsed = () =>
    document.readyState !== "loading"
      ? Promise.resolve()
      : new Promise((resolve) => document.addEventListener("DOMContentLoaded", resolve, { once: true }));

  // Refs: one per element for the life of the document.
  const hold = typeof WeakRef === "function" ? (el) => new WeakRef(el) : (el) => ({ deref: () => el });
  const refs = new Map();
  const ids = new WeakMap();
  let nextRef = 0;
  const refOf = (el) => {
    let ref = ids.get(el);
    if (!ref) {
      nextRef += 1;
      ref = "e" + nextRef;
      ids.set(el, ref);
      refs.set(ref, hold(el));
    }
    return ref;
  };

  const resolve = (args) => {
    if (args.ref) {
      const ref = String(args.ref).replace(/^@/, "");
      const held = refs.get(ref);
      const el = held && held.deref();
      if (!el || !el.isConnected) {
        throw new Error(
          "ref " + ref + " is not on the page any more (the page navigated or re-rendered since your snapshot); take a new snapshot"
        );
      }
      if (generation !== snapshotGeneration) {
        throw new Error("the user acted on the page since your snapshot; take a new snapshot before using its refs");
      }
      return el;
    }
    if (args.selector) {
      let el;
      try {
        el = document.querySelector(args.selector);
      } catch (error) {
        throw new Error("invalid selector: " + error.message);
      }
      if (!el) throw new Error("no element matches selector");
      return el;
    }
    throw new Error("give a ref from preview_snapshot or a CSS selector");
  };

  // --- The accessibility outline -------------------------------------------

  const SKIPPED = new Set(["script", "style", "noscript", "template", "head", "meta", "link", "title", "base"]);
  const INPUT_ROLES = {
    button: "button", submit: "button", reset: "button", image: "button", file: "button", color: "button",
    checkbox: "checkbox", radio: "radio", range: "slider", number: "spinbutton", search: "searchbox",
  };
  const LEAVES = new Set([
    "button", "link", "heading", "img", "textbox", "searchbox", "checkbox", "radio", "switch", "slider",
    "spinbutton", "combobox", "option", "menuitem", "menuitemcheckbox", "menuitemradio", "tab", "progressbar",
    "meter", "separator",
  ]);
  const NAMED_BY_CONTENT = new Set([
    "button", "link", "heading", "option", "tab", "menuitem", "menuitemcheckbox", "menuitemradio", "treeitem",
    "cell", "columnheader", "rowheader", "checkbox", "radio", "switch", "tooltip", "generic",
  ]);
  const collapse = (text) => String(text || "").replace(/\s+/g, " ").trim();
  const textOf = (el) => {
    const text = collapse(el.innerText != null ? el.innerText : el.textContent);
    if (text) return text;
    const image = el.querySelector && el.querySelector("img[alt],svg title");
    return image ? collapse(image.getAttribute("alt") || image.textContent) : "";
  };
  const inSection = (el) => !!(el.parentElement && el.parentElement.closest("article,aside,main,nav,section"));

  const roleOf = (el, style) => {
    const explicit = (el.getAttribute("role") || "").trim().split(/\s+/)[0];
    if (explicit === "presentation" || explicit === "none") return null;
    if (explicit && explicit !== "generic") return explicit;
    const tag = el.localName;
    switch (tag) {
      case "a":
      case "area":
        return el.hasAttribute("href") ? "link" : null;
      case "button":
      case "summary":
        return "button";
      case "input": {
        const type = (el.getAttribute("type") || "text").toLowerCase();
        if (type === "hidden") return null;
        return INPUT_ROLES[type] || (el.hasAttribute("list") ? "combobox" : "textbox");
      }
      case "textarea":
        return "textbox";
      case "select":
        return el.multiple || el.size > 1 ? "listbox" : "combobox";
      case "option":
        return "option";
      case "img":
        return el.getAttribute("alt") === "" ? null : "img";
      case "h1": case "h2": case "h3": case "h4": case "h5": case "h6":
        return "heading";
      case "ul": case "ol": case "menu":
        return "list";
      case "li":
        return "listitem";
      case "nav":
        return "navigation";
      case "main":
        return "main";
      case "header":
        return inSection(el) ? null : "banner";
      case "footer":
        return inSection(el) ? null : "contentinfo";
      case "aside":
        return "complementary";
      case "form":
        return "form";
      case "section":
        return el.hasAttribute("aria-label") || el.hasAttribute("aria-labelledby") ? "region" : null;
      case "article":
        return "article";
      case "dialog":
        return "dialog";
      case "table":
        return "table";
      case "tr":
        return "row";
      case "td":
        return "cell";
      case "th":
        return "columnheader";
      case "fieldset":
      case "details":
        return "group";
      case "progress":
        return "progressbar";
      case "meter":
        return "meter";
      case "hr":
        return "separator";
      case "iframe":
        return "iframe";
    }
    if (el.isContentEditable && !(el.parentElement && el.parentElement.isContentEditable)) return "textbox";
    if (el.hasAttribute("tabindex") && el.tabIndex >= 0) return "generic";
    if (el.hasAttribute("onclick")) return "generic";
    if (style.cursor === "pointer" && el.parentElement && getComputedStyle(el.parentElement).cursor !== "pointer") {
      return "generic";
    }
    return null;
  };

  const nameOf = (el, role) => {
    const labelledby = el.getAttribute("aria-labelledby");
    if (labelledby) {
      const text = collapse(
        labelledby
          .split(/\s+/)
          .map((id) => {
            const target = el.ownerDocument.getElementById(id);
            return target ? textOf(target) : "";
          })
          .join(" ")
      );
      if (text) return text;
    }
    const label = collapse(el.getAttribute("aria-label"));
    if (label) return label;
    const tag = el.localName;
    if (tag === "input" && /^(button|submit|reset)$/i.test(el.type)) {
      return el.value || (el.type === "submit" ? "Submit" : el.type === "reset" ? "Reset" : "");
    }
    if (el.labels && el.labels.length) {
      const text = collapse(Array.from(el.labels).map(textOf).join(" "));
      if (text) return text;
    }
    if (tag === "img" || tag === "area" || (tag === "input" && el.type === "image")) {
      const alt = collapse(el.getAttribute("alt"));
      if (alt) return alt;
    }
    if (tag === "fieldset") {
      const legend = el.querySelector("legend");
      if (legend) return textOf(legend);
    }
    if (tag === "table" && el.caption) return textOf(el.caption);
    if (NAMED_BY_CONTENT.has(role)) {
      const text = textOf(el);
      if (text) return text;
    }
    return collapse(el.getAttribute("title") || el.getAttribute("placeholder"));
  };

  const valueOf = (el, role) => {
    const tag = el.localName;
    if (tag === "select") {
      return Array.from(el.selectedOptions || [])
        .map((option) => collapse(option.label || option.textContent))
        .join(", ");
    }
    if (tag === "input" || tag === "textarea") {
      if (/^(checkbox|radio|button|submit|reset|image|file|color)$/i.test(el.type)) return "";
      if (el.type === "password") return el.value ? "•".repeat(Math.min(el.value.length, 12)) : "";
      return el.value;
    }
    if (role === "textbox" && el.isContentEditable) return collapse(el.textContent);
    if (role === "link") return el.getAttribute("href") || "";
    if (role === "slider" || role === "progressbar" || role === "meter" || role === "spinbutton") {
      return el.getAttribute("aria-valuetext") || el.getAttribute("aria-valuenow") || "";
    }
    return "";
  };

  const detailsOf = (el, role) => {
    const details = [];
    if (role === "heading") {
      const level = el.getAttribute("aria-level") || (/^h([1-6])$/.exec(el.localName) || [])[1];
      if (level) details.push("level=" + level);
    }
    const checked = el.getAttribute("aria-checked");
    if (checked != null) details.push(checked === "true" ? "checked" : checked === "mixed" ? "mixed" : "unchecked");
    else if (el.localName === "input" && /^(checkbox|radio)$/i.test(el.type)) details.push(el.checked ? "checked" : "unchecked");
    const expanded = el.getAttribute("aria-expanded");
    if (expanded != null) details.push(expanded === "true" ? "expanded" : "collapsed");
    else if (el.localName === "details") details.push(el.open ? "expanded" : "collapsed");
    if (el.getAttribute("aria-selected") === "true" || (el.localName === "option" && el.selected)) details.push("selected");
    if (el.disabled || el.getAttribute("aria-disabled") === "true") details.push("disabled");
    if (el.required || el.getAttribute("aria-required") === "true") details.push("required");
    if (el.readOnly || el.getAttribute("aria-readonly") === "true") details.push("readonly");
    if (el === el.ownerDocument.activeElement && el !== el.ownerDocument.body) details.push("focused");
    if (role === "generic") details.push("clickable");
    return details;
  };

  const quote = (text, max) =>
    '"' +
    clip(text, max)
      .replace(/\\/g, "\\\\")
      .replace(/"/g, '\\"')
      .replace(/\n/g, "\\n")
      .replace(/\r/g, "\\r")
      .replace(/\t/g, "\\t") +
    '"';

  const line = (depth, el, role, name, value, details) => {
    let text = "  ".repeat(depth) + "@" + refOf(el) + " " + role;
    if (name) text += " " + quote(name, 240);
    if (value && value !== name) text += " value=" + quote(value, 320);
    if (details && details.length) text += " " + details.join(" ");
    return text;
  };

  const OUTLINE_BUDGET = 20000;
  const outline = (root) => {
    const lines = [];
    let size = 0;
    let omitted = 0;
    const push = (text) => {
      if (size + text.length + 1 > OUTLINE_BUDGET) {
        omitted += 1;
        return;
      }
      lines.push(text);
      size += text.length + 1;
    };
    // The nodes a box's children render from: an open shadow root's, with
    // its slots' assigned nodes, else its light children.
    const childNodes = (node) => {
      const source = node.shadowRoot || node;
      const out = [];
      for (const child of Array.from(source.childNodes)) {
        if (child.nodeType === 1 && child.localName === "slot" && child.assignedNodes) {
          const assigned = child.assignedNodes({ flatten: true });
          out.push(...(assigned.length ? assigned : Array.from(child.childNodes)));
        } else out.push(child);
      }
      return out;
    };
    const directText = (nodes) => collapse(nodes.filter((node) => node.nodeType === 3).map((node) => node.data).join(" "));

    // Emit `el`'s subtree at `depth`; whether it emitted any line.
    const visit = (el, depth) => {
      if (SKIPPED.has(el.localName) || el.getAttribute("aria-hidden") === "true") return false;
      const style = getComputedStyle(el);
      if (style.display === "none") return false;
      const invisible = style.visibility === "hidden" || style.visibility === "collapse";
      const role = invisible ? null : roleOf(el, style);
      const nodes = childNodes(el);
      if (role === "iframe") {
        let inner = null;
        try {
          inner = el.contentDocument;
        } catch (_) {}
        const name = nameOf(el, role) || el.getAttribute("src") || "";
        if (!inner || !inner.body) {
          push(line(depth, el, role, name, "", ["cross-origin"]));
          return true;
        }
        push(line(depth, el, role, name, "", []));
        visit(inner.body, depth + 1);
        return true;
      }
      if (role && LEAVES.has(role)) {
        push(line(depth, el, role, nameOf(el, role), valueOf(el, role), detailsOf(el, role)));
        return true;
      }
      if (role) {
        const at = lines.length;
        push("");
        let any = false;
        for (const child of nodes) if (child.nodeType === 1 && visit(child, depth + 1)) any = true;
        const name = nameOf(el, role) || (any ? directText(nodes) : invisible ? "" : textOf(el));
        const text = line(depth, el, role, name, valueOf(el, role), detailsOf(el, role));
        if (lines[at] === "") {
          lines[at] = text;
          size += text.length;
        }
        return true;
      }
      // A box with no role of its own: its children stand in its place,
      // and its text is one line in its name.
      const at = lines.length;
      let any = false;
      for (const child of nodes) if (child.nodeType === 1 && visit(child, depth)) any = true;
      if (invisible) return any;
      // A label's text is its control's name.
      if (el.localName === "label" && el.control) return any;
      const text = any ? directText(nodes) : textOf(el);
      if (!text) return any;
      const entry = line(depth, el, "text", text, "", []);
      if (any) {
        lines.splice(at, 0, entry);
        size += entry.length + 1;
      } else push(entry);
      return true;
    };
    visit(root, 0);
    return { text: lines.filter((text) => text !== "").join("\n"), lines: lines.length, omitted };
  };

  const describe = (el) => {
    const role = roleOf(el, getComputedStyle(el)) || el.localName;
    return { ref: refOf(el), role, name: clip(nameOf(el, role), 240) };
  };

  // --- Synthetic input -------------------------------------------------------
  // Gap: no engine Tcode drives exposes trusted input to a page script, so
  // these are untrusted DOM events with the default actions done by hand.

  const pointer = (el, type, x, y) => {
    const init = { bubbles: true, cancelable: true, composed: true, view: window, clientX: x, clientY: y, button: 0 };
    const event =
      type.startsWith("pointer") && typeof PointerEvent === "function"
        ? new PointerEvent(type, Object.assign({ pointerId: 1, pointerType: "mouse", isPrimary: true }, init))
        : new MouseEvent(type, init);
    el.dispatchEvent(event);
  };

  const setValue = (el, value) => {
    const proto = el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype : HTMLInputElement.prototype;
    const setter = Object.getOwnPropertyDescriptor(proto, "value");
    if (setter && setter.set) setter.set.call(el, value);
    else el.value = value;
  };

  const methods = {
    status: () => ({
      url: location.href,
      title: document.title,
      loading: document.readyState !== "complete",
      generation,
    }),

    snapshot: () => {
      const result = outline(document.body || document.documentElement);
      snapshotGeneration = generation;
      reported = false;
      const snapshot = {
        url: location.href,
        title: document.title,
        loading: document.readyState !== "complete",
        generation,
        elements: result.lines,
        outline: result.text,
      };
      if (result.omitted) snapshot.omitted = result.omitted + " lines past the 20000-character budget";
      return snapshot;
    },

    click: (args) => {
      const el = resolve(args);
      el.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
      const rect = el.getBoundingClientRect();
      const x = rect.left + rect.width / 2;
      const y = rect.top + rect.height / 2;
      pointer(el, "pointerdown", x, y);
      pointer(el, "mousedown", x, y);
      if (typeof el.focus === "function") el.focus({ preventScroll: true });
      pointer(el, "pointerup", x, y);
      pointer(el, "mouseup", x, y);
      if (typeof el.click === "function") el.click();
      else pointer(el, "click", x, y);
      checkUrl();
      return { ok: true, target: describe(el) };
    },

    type: (args) => {
      const el = resolve(args);
      const text = String(args.text == null ? "" : args.text);
      el.focus();
      if (el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement) {
        setValue(el, text);
        el.dispatchEvent(new InputEvent("input", { bubbles: true, composed: true, data: text, inputType: "insertText" }));
        el.dispatchEvent(new Event("change", { bubbles: true }));
        return { ok: true, target: describe(el) };
      }
      if (el.isContentEditable) {
        el.textContent = text;
        el.dispatchEvent(new InputEvent("input", { bubbles: true, composed: true, data: text, inputType: "insertText" }));
        return { ok: true, target: describe(el) };
      }
      throw new Error("element is not editable");
    },

    press: (args) => {
      const key = String(args.key);
      const modifiers = args.modifiers || [];
      const target = document.activeElement || document.body;
      if (!target) throw new Error("document has no key target");
      const init = {
        key,
        bubbles: true,
        cancelable: true,
        composed: true,
        altKey: modifiers.includes("Alt"),
        ctrlKey: modifiers.includes("Control"),
        metaKey: modifiers.includes("Meta"),
        shiftKey: modifiers.includes("Shift"),
      };
      const proceed = target.dispatchEvent(new KeyboardEvent("keydown", init));
      if (proceed && Array.from(key).length === 1 && !init.ctrlKey && !init.metaKey) {
        if (target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement) {
          const start = target.selectionStart == null ? target.value.length : target.selectionStart;
          const end = target.selectionEnd == null ? start : target.selectionEnd;
          setValue(target, target.value.slice(0, start) + key + target.value.slice(end));
          try {
            target.setSelectionRange(start + key.length, start + key.length);
          } catch (_) {}
          target.dispatchEvent(new InputEvent("input", { bubbles: true, composed: true, data: key, inputType: "insertText" }));
        } else if (target.isContentEditable) {
          const selection = getSelection();
          let range = selection && selection.rangeCount ? selection.getRangeAt(0) : null;
          if (!range || !target.contains(range.commonAncestorContainer)) {
            range = document.createRange();
            range.selectNodeContents(target);
            range.collapse(false);
          }
          range.deleteContents();
          const node = document.createTextNode(key);
          range.insertNode(node);
          range.setStartAfter(node);
          range.collapse(true);
          if (selection) {
            selection.removeAllRanges();
            selection.addRange(range);
          }
          target.dispatchEvent(new InputEvent("input", { bubbles: true, composed: true, data: key, inputType: "insertText" }));
        }
      }
      if (proceed && key === "Enter" && target instanceof HTMLInputElement) {
        const form = target.closest("form");
        if (form && typeof form.requestSubmit === "function") form.requestSubmit();
      }
      target.dispatchEvent(new KeyboardEvent("keyup", init));
      checkUrl();
      return { ok: true, key, target: describe(target) };
    },

    scroll: (args) => {
      const left = Number(args.deltaX) || 0;
      const top = Number(args.deltaY) || 0;
      if (args.ref || args.selector) {
        const el = resolve(args);
        el.scrollBy({ left, top, behavior: "instant" });
        return { scrollLeft: el.scrollLeft, scrollTop: el.scrollTop, target: describe(el) };
      }
      window.scrollBy({ left, top, behavior: "instant" });
      return { x: window.scrollX, y: window.scrollY };
    },

    // An element's box in the viewport, for a picture of it: scrolled into
    // view first when it is not wholly in view.
    rect: (args) => {
      const el = resolve(args);
      let box = el.getBoundingClientRect();
      if (box.top < 0 || box.left < 0 || box.bottom > innerHeight || box.right > innerWidth) {
        el.scrollIntoView({ block: "center", inline: "center", behavior: "instant" });
        box = el.getBoundingClientRect();
      }
      return {
        x: box.left,
        y: box.top,
        width: box.width,
        height: box.height,
        scrollX: window.scrollX,
        scrollY: window.scrollY,
        dpr: window.devicePixelRatio || 1,
        viewportWidth: innerWidth,
        viewportHeight: innerHeight,
        target: describe(el),
      };
    },

    // Resolves once every DOM condition holds, or with what is still unmet
    // when `timeoutMs` runs out.
    wait: (args) =>
      new Promise((done) => {
        const unmet = () => {
          const pending = [];
          if (args.selector != null) {
            let found = null;
            try {
              found = document.querySelector(args.selector);
            } catch (_) {}
            if (!found) pending.push("selector");
          }
          if (args.text != null) {
            const body = document.body;
            const text = body ? body.innerText || body.textContent || "" : "";
            if (!text.includes(args.text)) pending.push("text");
          }
          return pending;
        };
        let observer = null;
        let timer = null;
        const settle = (pending) => {
          if (observer) observer.disconnect();
          if (timer) clearTimeout(timer);
          done({ matched: pending.length === 0, url: location.href, pending });
        };
        const now = unmet();
        if (!now.length) return settle(now);
        observer = new MutationObserver(() => {
          const pending = unmet();
          if (!pending.length) settle(pending);
        });
        observer.observe(document, { childList: true, subtree: true, characterData: true, attributes: true });
        timer = setTimeout(() => settle(unmet()), Math.max(0, Number(args.timeoutMs) || 0));
      }),
  };
  // Whether a method reads or acts on the document, so waits for it to be
  // parsed: the agent is ready at document start.
  const OF_DOCUMENT = new Set(["snapshot", "click", "type", "press", "scroll", "wait", "rect"]);

  const reply = (id, ok, payload) => send(ok ? { t: "reply", id, ok: true, value: payload } : { t: "reply", id, ok: false, error: payload });
  const call = (id, method, args) => {
    const run = Object.prototype.hasOwnProperty.call(methods, method) ? methods[method] : null;
    if (!run) return reply(id, false, "unknown page agent method " + method);
    (OF_DOCUMENT.has(method) ? parsed() : Promise.resolve())
      .then(() => run(args || {}))
      .then(
        (value) => reply(id, true, value === undefined ? null : value),
        (error) => reply(id, false, String((error && error.message) || error))
      );
  };

  Object.defineProperty(globalThis, NAME, {
    value: Object.freeze({ call }),
    enumerable: false,
    writable: false,
    configurable: false,
  });
  send({ t: "ready", url: location.href });
})
