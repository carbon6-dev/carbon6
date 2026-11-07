// "system" (default) follows the OS; "light" or "dark" is kept in this
// browser. Resolve legacy choices before CSS can paint or WASM starts.
window.c6Theme = (() => {
  const KEY = "c6.theme";
  const media = window.matchMedia("(prefers-color-scheme: dark)");
  const get = () => {
    let choice = "system";
    try { choice = localStorage.getItem(KEY) || "system"; } catch (_) {}
    if (choice === "dim" || choice === "soft") {
      try { localStorage.setItem(KEY, "dark"); } catch (_) {}
      return "dark";
    }
    return ["light", "dark"].includes(choice) ? choice : "system";
  };
  const apply = () => {
    const choice = get();
    const dark = choice === "dark" || (choice === "system" && media.matches);
    const root = document.documentElement;
    root.classList.toggle("dark", dark);
    root.classList.remove("soft");
    root.style.colorScheme = dark ? "dark" : "light";
    const meta = document.querySelector('meta[name="theme-color"]');
    if (meta) meta.setAttribute("content", dark ? "#1c1d21" : "#fafafa");
    window.dispatchEvent(new Event("c6-theme"));
  };
  media.addEventListener("change", apply);
  window.addEventListener("storage", event => {
    if (event.key === KEY || event.key === null) apply();
  });
  apply();
  return {
    get,
    set(choice) {
      try { localStorage.setItem(KEY, choice); } catch (_) {}
      apply();
    },
    isDark: () => document.documentElement.classList.contains("dark"),
  };
})();
