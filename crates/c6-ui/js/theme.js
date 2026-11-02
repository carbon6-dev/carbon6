// Light or dark: "system" (default) follows the OS; "light", "dark", or
// the softer darks "dim" and "soft" (dark, with lighter backgrounds:
// style/input.css), is kept in this browser. Loaded before the page
// renders, so it never flashes.
window.c6Theme = (() => {
  const KEY = "c6.theme";
  const media = window.matchMedia("(prefers-color-scheme: dark)");
  const get = () => {
    try { return localStorage.getItem(KEY) || "system"; } catch (_) { return "system"; }
  };
  const apply = () => {
    const choice = get();
    const softer = choice === "dim" || choice === "soft";
    const dark = choice === "dark" || softer || (choice === "system" && media.matches);
    const root = document.documentElement;
    root.classList.toggle("dark", dark);
    root.classList.toggle("dim", choice === "dim");
    root.classList.toggle("soft", choice === "soft");
    root.style.colorScheme = dark ? "dark" : "light";
    const meta = document.querySelector('meta[name="theme-color"]');
    const page = { dim: "#141417", soft: "#1c1d21" }[choice] || (dark ? "#09090b" : "#fafafa");
    if (meta) meta.setAttribute("content", page);
    window.dispatchEvent(new Event("c6-theme"));
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
