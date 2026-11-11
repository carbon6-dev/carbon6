// Only this small bridge loads at startup. Successful vendor loads are shared
// by all terminals for the page's lifetime; failed assets can be retried.
window.c6Terminal = (() => {
  const assets = new Map();
  let ready;
  const asset = (url, css, available) => {
    if (assets.has(url)) return assets.get(url);
    const node = document.createElement(css ? "link" : "script");
    const promise = new Promise((resolve, reject) => {
      if (css) { node.rel = "stylesheet"; node.href = url; }
      else { node.src = url; }
      const fail = () => {
        node.onload = node.onerror = null;
        node.remove();
        assets.delete(url);
        reject(new Error(`Could not load ${url}`));
      };
      node.onload = () => {
        if (available && !available()) { fail(); return; }
        node.onload = node.onerror = null;
        resolve();
      };
      node.onerror = fail;
    });
    assets.set(url, promise);
    try { document.head.appendChild(node); } catch (_) { node.onerror(); }
    return promise;
  };
  return {
    load() {
      if (!ready) {
        ready = Promise.all([
          asset("/vendor/xterm.css", true),
          asset("/vendor/xterm.js", false, () => typeof window.Terminal === "function")
            .then(() => asset("/vendor/addon-fit.js", false, () => typeof window.FitAddon?.FitAddon === "function")),
        ]).catch((error) => { ready = undefined; throw error; });
      }
      return ready;
    },
    create(el, onData, onResize) {
      const dark = window.c6Theme.isDark();
      const term = new Terminal({
        cursorBlink: true,
        fontFamily: "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace",
        fontSize: 13,
        scrollback: 10000,
        theme: dark
          ? { background: "#09090b", foreground: "#e4e4e7", cursor: "#e4e4e7", selectionBackground: "#3f3f46" }
          : { background: "#fafafa", foreground: "#18181b", cursor: "#18181b", selectionBackground: "#d4d4d8" },
      });
      let ro;
      try {
        const fit = new FitAddon.FitAddon();
        term.loadAddon(fit);
        term.open(el);
        const refit = () => { try { fit.fit(); } catch (_) {} };
        refit();
        term.onData((d) => onData(d));
        term.onResize(({ cols, rows }) => onResize(cols, rows));
        ro = new ResizeObserver(refit);
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
      } catch (error) {
        ro?.disconnect();
        term.dispose();
        throw error;
      }
    },
  };
})();
