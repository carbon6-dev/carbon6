// Light or dark: "system" (default) follows the OS; "light" or "dark" is
// kept in this browser. Loaded before the page renders, so it never flashes.
// "dim" and "soft", tried for a while, are dark now (dark took dim's greys:
// style/input.css).
window.c6Theme = (() => {
  const KEY = "c6.theme";
  const media = window.matchMedia("(prefers-color-scheme: dark)");
  const get = () => {
    let choice = "system";
    try { choice = localStorage.getItem(KEY) || "system"; } catch (_) {}
    return choice === "dim" || choice === "soft" ? "dark" : choice;
  };
  const apply = () => {
    const choice = get();
    const dark = choice === "dark" || (choice === "system" && media.matches);
    const root = document.documentElement;
    root.classList.toggle("dark", dark);
    root.style.colorScheme = dark ? "dark" : "light";
    const meta = document.querySelector('meta[name="theme-color"]');
    if (meta) meta.setAttribute("content", dark ? "#141417" : "#fafafa");
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
