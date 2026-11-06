// "system" (default) follows the OS; "light", "dark", or the softer dark
// "soft" is kept in this browser. The head loads this before WASM starts.
// Dark keeps the former Dim greys; Soft only repoints greys (style/input.css).
window.c6Theme = (() => {
  const KEY = "c6.theme";
  const media = window.matchMedia("(prefers-color-scheme: dark)");
  const get = () => {
    let choice = "system";
    try { choice = localStorage.getItem(KEY) || "system"; } catch (_) {}
    if (choice === "dim") return "dark";
    return ["light", "dark", "soft"].includes(choice) ? choice : "system";
  };
  const apply = () => {
    const choice = get();
    const soft = choice === "soft";
    const dark = choice === "dark" || soft || (choice === "system" && media.matches);
    const root = document.documentElement;
    root.classList.toggle("dark", dark);
    root.classList.toggle("soft", soft);
    root.style.colorScheme = dark ? "dark" : "light";
    const meta = document.querySelector('meta[name="theme-color"]');
    if (meta) meta.setAttribute("content", soft ? "#1c1d21" : dark ? "#141417" : "#fafafa");
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
