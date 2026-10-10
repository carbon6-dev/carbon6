// Search highlighting with the CSS Custom Highlight API (no DOM changes, so
// Leptos's view of the DOM stays intact). Each workspace column (a "root",
// by element id) can have its own find bar:
// - "c6-search": every match in every root with a find bar open, kept
//   current as the page changes;
// - "c6-search-current": the match being shown.
(() => {
  const supported = () => !!(window.CSS && CSS.highlights && window.Highlight);
  // root id -> { terms, observer }
  const roots = new Map();
  let timer = null;

  const rootEl = (id) => (id ? document.getElementById(id) : null) || document.body;

  // Ranges of `words` in the text under `el`, skipping [data-c6-nohl].
  function ranges(el, words, limit) {
    const out = [];
    const walker = document.createTreeWalker(el, NodeFilter.SHOW_TEXT, {
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
    if (!supported()) return;
    const all = [];
    for (const [id, r] of roots) all.push(...ranges(rootEl(id), r.terms, 5000));
    if (all.length) CSS.highlights.set("c6-search", new Highlight(...all));
    else CSS.highlights.delete("c6-search");
  }

  const parse = (json) => JSON.parse(json).map((t) => t.toLowerCase()).filter((t) => t.length > 0);

  window.c6Search = {
    // Highlight every match under the root, and keep doing so as it changes
    // (streamed messages, expanded side chats) until clear(root).
    highlightAll(root, termsJson) {
      const prev = roots.get(root);
      if (prev && prev.observer) prev.observer.disconnect();
      let observer = null;
      if (supported()) {
        observer = new MutationObserver(() => {
          clearTimeout(timer);
          timer = setTimeout(paintAll, 200);
        });
        observer.observe(rootEl(root), { childList: true, subtree: true, characterData: true });
      }
      roots.set(root, { terms: parse(termsJson), observer });
      paintAll();
    },

    clear(root) {
      const r = roots.get(root);
      if (r && r.observer) r.observer.disconnect();
      roots.delete(root);
      if (supported()) {
        CSS.highlights.delete("c6-search-current");
        paintAll();
      }
    },

    // Reveal a match under the root: open collapsed blocks around it,
    // scroll to it, flash it, and mark its terms as the current match.
    reveal(root, id, termsJson) {
      const el = rootEl(root).querySelector("#" + CSS.escape(id));
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
      }
      return true;
    },
  };
})();
