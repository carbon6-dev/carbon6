// Light or dark: "system" (default) follows the OS; "light" or "dark" is
// kept in this browser. Loaded before the page renders, so it never flashes.
window.c6Theme = (() => {
  const KEY = "c6.theme";
  const media = window.matchMedia("(prefers-color-scheme: dark)");
  const get = () => {
    try { return localStorage.getItem(KEY) || "system"; } catch (_) { return "system"; }
  };
  const apply = () => {
    const choice = get();
    const dark = choice === "dark" || (choice === "system" && media.matches);
    const root = document.documentElement;
    root.classList.toggle("dark", dark);
    root.style.colorScheme = dark ? "dark" : "light";
    const meta = document.querySelector('meta[name="theme-color"]');
    if (meta) meta.setAttribute("content", dark ? "#09090b" : "#fafafa");
  };
  media.addEventListener("change", apply);
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
