// Reveal a search hit: open collapsed blocks around it, scroll to it, flash
// it, and highlight the search terms in it with the CSS Custom Highlight API
// (no DOM changes, so Leptos's view of the DOM stays intact).
window.c6Search = {
  clear() {
    if (window.CSS && CSS.highlights) CSS.highlights.delete("c6-search");
  },
  reveal(id, termsJson) {
    const el = document.getElementById(id);
    if (!el) return false;
    for (let p = el; p; p = p.parentElement) if (p.tagName === "DETAILS") p.open = true;
    el.querySelectorAll("details").forEach((d) => (d.open = true));
    el.scrollIntoView({ block: "center" });
    el.classList.remove("c6-flash");
    void el.offsetWidth; // restart the animation
    el.classList.add("c6-flash");
    const terms = JSON.parse(termsJson).map((t) => t.toLowerCase()).filter((t) => t.length > 0);
    if (window.CSS && CSS.highlights && window.Highlight && terms.length) {
      const ranges = [];
      const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
      for (let node = walker.nextNode(); node; node = walker.nextNode()) {
        const text = node.data.toLowerCase();
        for (const t of terms) {
          for (let i = text.indexOf(t); i >= 0; i = text.indexOf(t, i + t.length)) {
            const r = new Range();
            r.setStart(node, i);
            r.setEnd(node, i + t.length);
            ranges.push(r);
          }
        }
      }
      CSS.highlights.set("c6-search", new Highlight(...ranges));
    }
    return true;
  },
};
