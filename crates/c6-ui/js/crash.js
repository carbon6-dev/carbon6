// When the app breaks, a small banner says so and offers a reload, instead
// of a page that no longer answers. A Rust panic calls it (its hook), and so
// does any trap in the WebAssembly (what follows a panic, or a stack or
// memory gone bad), whichever comes first, and so does an error from the
// bindings that leaves a handler stuck: an event handler entered again
// while it runs ("recursive use of an object…"), or one called after it was
// dropped. Those are plain Errors, not traps, and after one the page's
// shared key and click handlers can stay broken (Enter types a new line,
// Send does nothing) until a reload. Plain DOM and a plain listener: what
// broke can't draw it. What broke goes to the head's log too.
window.c6Crashed = (() => {
  let told = false;
  // Only enum labels and numeric layout keys, never page text, URLs, IDs or
  // saved layout. Keep this prefix below 500 chars: the head keeps 2000 of
  // stack, so both the mounted panes and useful frames survive its cap.
  const kinds = new Set([
    "sessions", "session", "session_changes", "session_terminal", "session_files", "session_outputs",
    "search", "workers", "worker", "settings", "inbox", "mission", "usage", "chats", "analytics", "activity",
    "memex", "mail", "chat", "conversation", "brief", "calendar", "contacts", "photos", "decisions", "work",
    "projects", "docs", "collections", "realms", "explore", "app",
  ]);
  const numeric = (key) => /^(0|[1-9][0-9]{0,19})$/.test(key || "");
  const mounted = () => {
    try {
      let out = "mounted=";
      const add = (entry) => {
        const next = out + (out === "mounted=" ? "" : ",") + entry;
        if (next.length > 297) return false;
        out = next;
        return true;
      };
      for (const tile of document.querySelectorAll("[data-tile-motion]")) {
        const t = tile.getAttribute("data-tile-motion");
        if (!numeric(t)) continue;
        const panes = tile.querySelectorAll("[data-pane-motion]");
        if (!panes.length && !add(`t${t}/-`)) return out + "...";
        for (const pane of panes) {
          const p = pane.getAttribute("data-pane-motion");
          if (!numeric(p)) continue;
          const marker = pane.querySelector("[data-pane-kind]");
          const kind = marker && marker.getAttribute("data-pane-kind");
          // A deferred pane's shell exists before its content is mounted.
          if (!add(`t${t}/p${p}:${marker ? (kinds.has(kind) ? kind : "unknown") : "pending"}`)) return out + "...";
        }
      }
      return out === "mounted=" ? out + "none" : out;
    } catch (_) {
      return "mounted=unavailable";
    }
  };
  const agent = () => {
    try {
      return "ua=" + String(navigator.userAgent || "").replace(/[\r\n]/g, " ").slice(0, 160);
    } catch (_) {
      return "ua=unavailable";
    }
  };
  const tell = (message, stack) => {
    if (told) return;
    told = true;
    try {
      fetch("/api/v1/ui/error", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "same-origin",
        body: JSON.stringify({ message: String(message || ""), stack: mounted() + "\n" + agent() + "\n" + String(stack || ""), page: location.pathname }),
      }).catch(() => {});
    } catch (_) {}
  };
  const show = (message, stack) => {
    // Rust's panic hook supplies only a message. Capture here, while its
    // app-level Wasm callers are still on the stack, not at the later trap.
    tell(message, stack || new Error("c6Crashed").stack);
    if (document.getElementById("c6-crashed")) return;
    const bar = document.createElement("div");
    bar.id = "c6-crashed";
    bar.setAttribute("role", "alert");
    bar.style.cssText =
      "position:fixed;top:12px;left:50%;transform:translateX(-50%);z-index:2147483647;display:flex;align-items:center;gap:12px;" +
      "padding:8px 8px 8px 14px;border-radius:8px;background:#b91c1c;color:#fff;font:500 14px/1.4 system-ui,sans-serif;" +
      "box-shadow:0 4px 16px rgba(0,0,0,.3)";
    const text = document.createElement("span");
    text.textContent = "Something broke, reload";
    const button = document.createElement("button");
    button.type = "button";
    button.textContent = "Reload";
    button.style.cssText =
      "padding:4px 12px;border:0;border-radius:6px;background:#fff;color:#b91c1c;font:inherit;font-weight:600;cursor:pointer";
    button.addEventListener("click", () => location.reload());
    bar.append(text, button);
    (document.body || document.documentElement).append(bar);
  };
  const stuck = /recursive use of an object detected|closure invoked (recursively or )?after being dropped|already (mutably )?borrowed/i;
  const breaks = (e) =>
    (typeof WebAssembly !== "undefined" && e instanceof WebAssembly.RuntimeError) || (e && stuck.test(String(e.message || e)));
  window.addEventListener("error", (ev) => {
    if (breaks(ev.error)) show(ev.error && ev.error.message, ev.error && ev.error.stack);
  });
  window.addEventListener("unhandledrejection", (ev) => {
    if (breaks(ev.reason)) show(ev.reason && ev.reason.message, ev.reason && ev.reason.stack);
  });
  return show;
})();
