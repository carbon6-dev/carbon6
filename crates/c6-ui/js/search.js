// Search highlighting with the CSS Custom Highlight API (no DOM changes, so
// Leptos's view of the DOM stays intact):
// - "c6-search": every match on the page, kept current as the page changes;
// - "c6-search-current": the match being shown.
(() => {
  const supported = () => !!(window.CSS && CSS.highlights && window.Highlight);
  let terms = [];
  let observer = null;
  let timer = null;

  // Ranges of `terms` in the text under `root`, skipping [data-c6-nohl].
  function ranges(root, words, limit) {
    const out = [];
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT, {
      acceptNode: (n) => (n.parentElement && n.parentElement.closest("[data-c6-nohl]") ? NodeFilter.FILTER_REJECT : NodeFilter.FILTER_ACCEPT),
    });
    for (let node = walker.nextNode(); node && out.length < limit; node = walker.nextNode()) {
      const text = node.data.toLowerCase();
      for (const t of words) {
        for (let i = text.indexOf(t); i >= 0 && out.length < limit; i = text.indexOf(t, i + t.length)) {
          const r = new Range();
          r.setStart(node, i);
          r.setEnd(node, i + t.length);
          out.push(r);
        }
      }
    }
    return out;
  }

  function paintAll() {
    const root = document.querySelector("main");
    if (!supported() || !root || !terms.length) return;
    CSS.highlights.set("c6-search", new Highlight(...ranges(root, terms, 5000)));
  }

  const parse = (json) => JSON.parse(json).map((t) => t.toLowerCase()).filter((t) => t.length > 0);

  window.c6Search = {
    // Highlight every match on the page, and keep doing so as it changes
    // (streamed messages, expanded side chats) until clear().
    highlightAll(termsJson) {
      terms = parse(termsJson);
      paintAll();
      const root = document.querySelector("main");
      if (!observer && root && supported()) {
        observer = new MutationObserver(() => {
          clearTimeout(timer);
          timer = setTimeout(paintAll, 200);
        });
        observer.observe(root, { childList: true, subtree: true, characterData: true });
      }
    },

    clear() {
      terms = [];
      if (observer) observer.disconnect();
      observer = null;
      clearTimeout(timer);
      if (supported()) {
        CSS.highlights.delete("c6-search");
        CSS.highlights.delete("c6-search-current");
      }
    },

    // Reveal a match: open collapsed blocks around it, scroll to it, flash
    // it, and mark its terms as the current match.
    reveal(id, termsJson) {
      const el = document.getElementById(id);
      if (!el) return false;
      for (let p = el; p; p = p.parentElement) if (p.tagName === "DETAILS") p.open = true;
      el.querySelectorAll("details").forEach((d) => (d.open = true));
      el.scrollIntoView({ block: "center" });
      el.classList.remove("c6-flash");
      void el.offsetWidth; // restart the animation
      el.classList.add("c6-flash");
      const words = parse(termsJson);
      if (supported() && words.length) {
        const current = new Highlight(...ranges(el, words, 1000));
        current.priority = 1;
        CSS.highlights.set("c6-search-current", current);
        paintAll();
      }
      return true;
    },
  };
})();
