// Structured-data controls re-use the Rust renderer; no content is executable.
(() => {
  const states = new WeakMap();
  function input(root) {
    const source = root.querySelector(":scope > .fv-source");
    if (!states.has(root)) states.set(root, {
      path: root.dataset.path || "", url: root.dataset.url || "", mode: root.dataset.mode || "full",
      line: Number(root.dataset.line || 0), text: source?.dataset?.encoding === "json" ? JSON.parse(source.textContent) : source?.textContent || "",
      language: root.dataset.language || "", limit: 0,
      mime: root.dataset.mime || "",
    });
    return states.get(root);
  }
  async function complete(root, forCopy = false) {
    const state = input(root);
    if (root.dataset.sourceTruncated === "true" && state.url && !state.complete) {
      try { state.text = await window.c6Viewers.text(state.url, root, forCopy ? Infinity : undefined); state.complete = true; }
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
    const copy = ev.target.closest?.(".c6-copy");
    if (copy) {
      ev.preventDefault(); ev.stopPropagation();
      const status = copy.querySelector('[role="status"]');
      if (status) status.textContent = "Copying…";
      try {
        let text;
        if (copy.hasAttribute("data-copy-viewer")) {
          const state = await complete(copy.closest(".fv"), true);
          if (!state) throw new Error("Could not load complete content");
          text = state.text;
        } else if (copy.dataset.copyUrl) {
          const response = await fetch(copy.dataset.copyUrl, { credentials: "same-origin" });
          if (!response.ok) throw new Error("Could not load content");
          text = await response.text();
        } else if (copy.dataset.copySelector) {
          const element = copy.parentElement.querySelector(copy.dataset.copySelector);
          if (!element) throw new Error("Content unavailable");
          text = element.matches("table")
            ? [...element.rows].map(row => [...row.cells].map(cell => cell.innerText).join("\t")).join("\n")
            : element.innerText;
        } else {
          text = JSON.parse(copy.dataset.copyJson);
        }
        await navigator.clipboard.writeText(text);
        copy.dataset.copied = "true";
        copy.title = copy.getAttribute?.("aria-label") || "Copy";
        if (status) status.textContent = "Copied";
      } catch (_) {
        delete copy.dataset.copied;
        copy.title = "Copy unavailable; select the text and copy manually";
        if (status) status.textContent = "Couldn't copy";
      }
      return;
    }
    const target = ev.target.closest?.("[data-fv-sort],[data-fv-action]");
    const root = target?.closest(".fv");
    if (!root) return;
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
  }, true); // Copy must not also select a doc block, navigate, or open a message.
  document.addEventListener("change", async (ev) => {
    if (!ev.target.matches?.("[data-fv-filter]")) return;
    const root = ev.target.closest(".fv"), state = await complete(root);
    if (!state) return;
    state.filter = ev.target.value; redraw(root, state);
    root.querySelector?.("[data-fv-filter]")?.focus();
  });
})();
