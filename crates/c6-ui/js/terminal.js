// Thin glue between the Rust UI and xterm.js (vendored, loaded as globals).
window.c6Terminal = {
  create(el, onData, onResize) {
    const dark = window.matchMedia("(prefers-color-scheme: dark)").matches;
    const term = new Terminal({
      cursorBlink: true,
      fontFamily: "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace",
      fontSize: 13,
      scrollback: 10000,
      theme: dark
        ? { background: "#09090b", foreground: "#e4e4e7", cursor: "#e4e4e7", selectionBackground: "#3f3f46" }
        : { background: "#fafafa", foreground: "#18181b", cursor: "#18181b", selectionBackground: "#d4d4d8" },
    });
    const fit = new FitAddon.FitAddon();
    term.loadAddon(fit);
    term.open(el);
    const refit = () => { try { fit.fit(); } catch (_) {} };
    refit();
    term.onData((d) => onData(d));
    term.onResize(({ cols, rows }) => onResize(cols, rows));
    const ro = new ResizeObserver(refit);
    ro.observe(el);
    onResize(term.cols, term.rows);
    term.focus();
    return {
      // `bytes` is a view into wasm memory that is reused after this call,
      // and xterm parses asynchronously: copy it.
      write: (bytes) => term.write(bytes.slice()),
      writeln: (s) => term.writeln(s),
      dispose: () => { ro.disconnect(); term.dispose(); },
    };
  },
};
