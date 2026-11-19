// Structured-data controls re-use the Rust renderer; no content is executable.
(() => {
  const states = new WeakMap();
  function input(root) {
    if (!states.has(root)) states.set(root, {
      path: root.dataset.path || "", url: root.dataset.url || "", mode: root.dataset.mode || "full",
      line: Number(root.dataset.line || 0), text: root.querySelector(":scope > .fv-source")?.textContent || "",
      language: root.dataset.language || "", limit: 0,
    });
    return states.get(root);
  }
  function redraw(root, state) {
    const wrapper = document.createElement("div");
    wrapper.innerHTML = window.c6Viewers.render(state);
    const next = wrapper.firstElementChild;
    if (!next) return;
    states.set(next, state);
    root.replaceWith(next);
  }
  document.addEventListener("click", async (ev) => {
    const target = ev.target.closest?.("[data-fv-copy],[data-fv-sort],[data-fv-action]");
    const root = target?.closest(".fv");
    if (!root) return;
    if (target.hasAttribute("data-fv-copy")) {
      ev.preventDefault(); ev.stopPropagation();
      try { await navigator.clipboard.writeText(target.dataset.fvCopy); target.textContent = "Copied"; }
      catch (_) { target.textContent = "Copy unavailable"; }
      return;
    }
    if (target.hasAttribute("data-fv-sort")) {
      const state = input(root), sort = Number(target.dataset.fvSort);
      state.descending = state.sort === sort ? !state.descending : false;
      state.sort = sort; redraw(root, state);
    } else if (target.dataset.fvAction === "more") {
      const state = input(root);
      const step = ["csv", "tsv"].includes(root.dataset.kind) ? (state.mode === "inline" ? 20 : 10000) : 2000;
      state.limit = (state.limit || step) + step;
      redraw(root, state);
    }
  });
  document.addEventListener("change", (ev) => {
    if (!ev.target.matches?.("[data-fv-filter]")) return;
    const root = ev.target.closest(".fv"), state = input(root);
    state.filter = ev.target.value; redraw(root, state);
    root.querySelector?.("[data-fv-filter]")?.focus();
  });
})();
