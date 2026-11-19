// Structured-data controls re-use the Rust renderer; no content is executable.
(() => {
  const states = new WeakMap();
  function input(root) {
    if (!states.has(root)) states.set(root, {
      path: root.dataset.path || "", url: root.dataset.url || "", mode: root.dataset.mode || "full",
      line: Number(root.dataset.line || 0), text: root.querySelector(":scope > .fv-source")?.textContent || "",
      language: root.dataset.language || "", limit: 0,
      mime: root.dataset.mime || "",
    });
    return states.get(root);
  }
  async function complete(root) {
    const state = input(root);
    if (root.dataset.sourceTruncated === "true" && state.url && !state.complete) {
      try { state.text = await window.c6Viewers.text(state.url, root); state.complete = true; }
      catch (_) {
        if (!root.isConnected) return null;
        const error = document.createElement("p"); error.setAttribute("role", "alert");
        error.textContent = "Could not load the complete file; use Download or retry."; root.append(error);
        return null;
      }
    }
    return state;
  }
  function redraw(root, state) {
    const source = root.querySelector(":scope > .fv-source-view")?.hidden === false;
    const split = root.querySelector('[data-fv-action="split"]')?.getAttribute("aria-pressed") === "true";
    const scroll = root.querySelector(".fv-code")?.scrollTop || 0;
    const wrapper = document.createElement("div");
    wrapper.innerHTML = window.c6Viewers.render(state);
    const next = wrapper.firstElementChild;
    if (!next) return;
    states.set(next, state);
    root.replaceWith(next);
    if (source) {
      const src = next.querySelector(":scope > .fv-source-view"), rendered = next.querySelector(":scope > .fv-rendered");
      if (src && rendered) { src.hidden = false; rendered.hidden = true; }
      next.querySelectorAll('[data-fv-action="source"]').forEach(button => {
        if (button.closest(".fv") !== next) return;
        button.setAttribute("aria-pressed", String(button.dataset.fvView !== "pretty"));
      });
    }
    if (split) {
      next.querySelectorAll(".fv-diff-unified").forEach(el => { el.hidden = true; });
      next.querySelectorAll(".fv-diff-split").forEach(el => { el.hidden = false; });
      next.querySelector('[data-fv-action="split"]')?.setAttribute("aria-pressed", "true");
    }
    const selected = next.querySelector(".fv-selected"), box = selected?.closest(".fv-code");
    if (box) {
      box.scrollTop = selected.offsetTop - box.offsetTop - box.clientHeight / 2;
      if (selected.hasAttribute("data-fv-error")) selected.focus({ preventScroll: true });
    }
    else if (next.querySelector(".fv-code")) next.querySelector(".fv-code").scrollTop = scroll;
  }
  document.addEventListener("click", async (ev) => {
    const target = ev.target.closest?.("[data-fv-copy],[data-fv-copy-json],[data-fv-sort],[data-fv-action]");
    const root = target?.closest(".fv");
    if (!root) return;
    if (target.hasAttribute("data-fv-copy") || target.hasAttribute("data-fv-copy-json")) {
      ev.preventDefault(); ev.stopPropagation();
      let message;
      try {
        await navigator.clipboard.writeText(target.hasAttribute("data-fv-copy-json") ? JSON.parse(target.dataset.fvCopyJson) : target.dataset.fvCopy);
        message = "Copied"; target.dataset.copied = "true";
      } catch (_) { message = "Copy unavailable"; }
      target.title = message;
      let status = root.querySelector(".fv-copy-status");
      if (!status) {
        status = document.createElement("span"); status.className = "fv-copy-status";
        status.setAttribute("role", "status"); root.append(status);
      }
      status.textContent = message;
      return;
    }
    if (target.hasAttribute("data-fv-sort")) {
      const state = await complete(root), sort = Number(target.dataset.fvSort);
      if (!state) return;
      state.descending = state.sort === sort ? !state.descending : false;
      state.sort = sort; redraw(root, state);
    } else if (target.dataset.fvAction === "split") {
      const split = target.getAttribute("aria-pressed") !== "true";
      target.setAttribute("aria-pressed", String(split));
      root.querySelectorAll(".fv-diff-unified").forEach(el => { el.hidden = split; });
      root.querySelectorAll(".fv-diff-split").forEach(el => { el.hidden = !split; });
    } else if (["first-error", "prev-error", "next-error"].includes(target.dataset.fvAction)) {
      const numbers = (root.querySelector("[data-fv-errors]")?.dataset.fvErrors || "").split(",").map(Number).filter(n => n > 0);
      if (!numbers.length) return;
      const state = await complete(root);
      if (!state) return;
      const current = numbers.indexOf(state.line);
      const index = target.dataset.fvAction === "first-error" ? 0 : target.dataset.fvAction === "prev-error"
        ? (current <= 0 ? numbers.length - 1 : current - 1) : (current + 1) % numbers.length;
      state.line = numbers[index];
      const selected = root.querySelector(`[data-fv-error][data-line="${state.line}"]`);
      if (!selected) { redraw(root, state); return; }
      root.querySelectorAll(".fv-selected").forEach(el => el.classList.remove("fv-selected"));
      const line = selected, box = line.closest(".fv-code");
      line.classList.add("fv-selected");
      box.scrollTop += line.getBoundingClientRect().top - box.getBoundingClientRect().top - box.clientHeight / 2;
      line.focus({ preventScroll: true });
    } else if (target.dataset.fvAction === "more") {
      const state = await complete(root);
      if (!state) return;
      const step = ["csv", "tsv"].includes(root.dataset.kind) ? (state.mode === "inline" ? 20 : 10000) : 2000;
      state.limit = (state.limit || step) + step;
      redraw(root, state);
    }
  });
  document.addEventListener("change", async (ev) => {
    if (!ev.target.matches?.("[data-fv-filter]")) return;
    const root = ev.target.closest(".fv"), state = await complete(root);
    if (!state) return;
    state.filter = ev.target.value; redraw(root, state);
    root.querySelector?.("[data-fv-filter]")?.focus();
  });
})();
