// node --test scripts/test-terminal-assets.cjs (no npm dependencies).
const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const vm = require("node:vm");
const ui = `${__dirname}/../crates/c6-ui`;
const source = fs.readFileSync(`${ui}/js/terminal.js`, "utf8");

function fixture() {
  const nodes = [];
  const disposed = [];
  let failOpen = false;
  class Terminal {
    constructor() { this.cols = 80; this.rows = 24; }
    loadAddon() {}
    open() { if (failOpen) throw new Error("open failed"); }
    onData(cb) { this.data = cb; }
    onResize(cb) { this.resize = cb; }
    focus() {}
    write(bytes) { this.bytes = bytes; }
    writeln() {}
    dispose() { disposed.push("terminal"); }
  }
  const context = vm.createContext({
    document: {
      createElement(tag) { return { tag, remove() { this.removed = true; } }; },
      head: { appendChild(node) { nodes.push(node); } },
    },
    ResizeObserver: class { observe() {} disconnect() { disposed.push("observer"); } },
  });
  context.window = context;
  context.c6Theme = { isDark: () => true };
  vm.runInContext(source, context);
  const url = (node) => node.src || node.href;
  const pending = (path) => nodes.filter((n) => url(n) === path && !n.removed).at(-1);
  const success = (path) => {
    if (path.endsWith("xterm.js")) context.Terminal = Terminal;
    if (path.endsWith("addon-fit.js")) context.FitAddon = { FitAddon: class { fit() {} } };
    pending(path).onload();
  };
  return { nodes, disposed, bridge: context.c6Terminal, success, pending, failOpen: () => { failOpen = true; } };
}
const tick = () => new Promise((resolve) => setImmediate(resolve));
async function finish(f) {
  f.success("/vendor/xterm.js");
  await tick();
  f.success("/vendor/addon-fit.js");
  f.success("/vendor/xterm.css");
}

test("startup is inert; concurrent and reopened terminals share one vendor load", async () => {
  const f = fixture();
  assert.equal(f.nodes.length, 0);
  const one = f.bridge.load();
  assert.equal(one, f.bridge.load());
  assert.equal(f.nodes.length, 2); // CSS + xterm, not addon before its dependency.
  await finish(f);
  await one;
  await f.bridge.load();
  assert.equal(f.nodes.length, 3);
  const sizes = [];
  const handle = f.bridge.create({}, () => {}, (...size) => sizes.push(size));
  assert.deepEqual(sizes, [[80, 24]]);
  handle.dispose();
  assert.deepEqual(f.disposed, ["observer", "terminal"]);
  await f.bridge.load();
  assert.equal(f.nodes.length, 3);
});

test("asset errors are visible and retry only failed assets, including overlapping loads", async () => {
  for (const failed of ["/vendor/xterm.css", "/vendor/xterm.js", "/vendor/addon-fit.js"]) {
    const f = fixture();
    const result = assert.rejects(f.bridge.load(), new RegExp(failed));
    if (failed === "/vendor/xterm.js") {
      f.success("/vendor/xterm.css");
    } else {
      f.success("/vendor/xterm.js");
      await tick();
      if (failed === "/vendor/xterm.css") f.success("/vendor/addon-fit.js");
      else f.success("/vendor/xterm.css");
    }
    f.pending(failed).onerror();
    await result;
    const retry = f.bridge.load();
    await tick();
    f.success(failed);
    if (failed === "/vendor/xterm.js") { await tick(); f.success("/vendor/addon-fit.js"); }
    await retry;
    assert.equal(f.nodes.length, 4);
  }
  const f = fixture();
  const first = assert.rejects(f.bridge.load(), /xterm.css/);
  f.pending("/vendor/xterm.css").onerror();
  await first;
  const retry = f.bridge.load(); // xterm JS is still loading: share it.
  assert.equal(f.nodes.filter((n) => n.src === "/vendor/xterm.js").length, 1);
  await finish(f);
  await retry;
  assert.equal(f.nodes.length, 4);
});

test("a script that loads without its global can be retried", async () => {
  const f = fixture();
  const result = assert.rejects(f.bridge.load(), /xterm.js/);
  f.success("/vendor/xterm.css");
  f.pending("/vendor/xterm.js").onload();
  await result;
  const retry = f.bridge.load();
  f.success("/vendor/xterm.js");
  await tick();
  f.success("/vendor/addon-fit.js");
  await retry;
  assert.equal(f.nodes.length, 4);
});

test("initialization errors dispose partially opened terminals", async () => {
  const f = fixture();
  const ready = f.bridge.load();
  await finish(f);
  await ready;
  f.failOpen();
  assert.throws(() => f.bridge.create({}, () => {}, () => {}), /open failed/);
  assert.deepEqual(f.disposed, ["terminal"]);
});

test("entrypoint defers helpers, does not eagerly reference vendor terminal assets", () => {
  const html = fs.readFileSync(`${ui}/index.html`, "utf8");
  assert.doesNotMatch(html, /(?:src|href)="\/vendor\/(?:xterm|addon-fit)/);
  for (const tag of html.matchAll(/<script\b[^>]*src=[^>]*>/g)) assert.match(tag[0], /\bdefer\b/);
});
