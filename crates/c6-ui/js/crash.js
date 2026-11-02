// When the app breaks, a small banner says so and offers a reload, instead
// of a page that no longer answers. A Rust panic calls it (its hook), and so
// does any trap in the WebAssembly (what follows a panic, or a stack or
// memory gone bad), whichever comes first. Plain DOM and a plain listener:
// what broke can't draw it.
window.c6Crashed = (() => {
  const show = () => {
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
  const trapped = (e) => typeof WebAssembly !== "undefined" && e instanceof WebAssembly.RuntimeError;
  window.addEventListener("error", (ev) => {
    if (trapped(ev.error)) show();
  });
  window.addEventListener("unhandledrejection", (ev) => {
    if (trapped(ev.reason)) show();
  });
  return show;
})();
