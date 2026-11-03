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
  const tell = (message, stack) => {
    if (told) return;
    told = true;
    try {
      fetch("/api/v1/ui/error", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        credentials: "same-origin",
        body: JSON.stringify({ message: String(message || ""), stack: String(stack || ""), page: location.pathname }),
      }).catch(() => {});
    } catch (_) {}
  };
  const show = (message, stack) => {
    tell(message, stack);
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
