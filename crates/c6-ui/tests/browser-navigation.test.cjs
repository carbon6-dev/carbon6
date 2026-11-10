'use strict';
const assert = require('node:assert/strict');
const { test } = require('node:test');
const { EventEmitter } = require('node:events');
const fs = require('node:fs');
const os = require('node:os');
const path = require('node:path');
const { diagnostic, exactDiagnostic, navigationObserver } = require('./browser-navigation.cjs');
const { fixture, within } = require('./motion.cjs');
const origin = 'http://127.0.0.1:37655';
// Literal stack/name/message from rawPageErrors[0] of the earlier FAILED WebKit run:
// ~/.cache/c6-helper/browser-fast/evidence/cold-native-scope-repair-integration-20261110T100100Z-g0huHO/
// runs/full-2026-11-10T10-01-40-724Z-2390590/pass-1/webkit/resource-lifecycle/shard-1/webkit-light-projects/evidence.json
// Source SHA-256: 184855fa130b62c16a6d1f5dd9abe1277b9e209eaeb67171c465a9e1f89087a3
// Replayed policy input only; this does not reclassify or pass that historical run.
const savedError = {
  stack: `Fetch API cannot load http://127.0.0.1:37655/api/v1/attention due to access control checks.
    at __wbg_fetch_9b478faef8cda538 (http://127.0.0.1:37655/c6-ui-e40c7bd8a090685e.js:549:30)
    at .wasm-function[5864] ([wasm code]:0:0)
    at .wasm-function[5985] ([wasm code]:0:0)
    at .wasm-function[38938] ([wasm code]:0:0)
    at .wasm-function[3083] ([wasm code]:0:0)
    at .wasm-function[1029] ([wasm code]:0:0)
    at .wasm-function[34295] ([wasm code]:0:0)
    at .wasm-function[5592] ([wasm code]:0:0)
    at .wasm-function[15770] ([wasm code]:0:0)
    at .wasm-function[24176] ([wasm code]:0:0)
    at wasm_bindgen_542dc48063100ff5___convert__closures_____invoke___wasm_bindgen_542dc48063100ff5___JsValue__core_9b3796e30d99ddb7___result__Result_____wasm_bindgen_542dc48063100ff5___JsError___true_ (http://127.0.0.1:37655/c6-ui-e40c7bd8a090685e.js:1967:216)
    at real (http://127.0.0.1:37655/c6-ui-e40c7bd8a090685e.js:2176:21)`,
  name: 'Fetch API cannot load http',
  message: '/127.0.0.1:37655/api/v1/attention due to access control checks.',
};
const error = text => text === undefined ? { ...savedError } :
  { ...savedError, stack: text + savedError.stack.slice(savedError.stack.indexOf('\n')), message: text };

async function harness(engine = 'webkit') {
  const fatal = [], calls = [], rawLog = [], response = {};
  const page = Object.assign(new EventEmitter(), {
    url: () => origin + '/initial',
    async goto(destination, options) { calls.push({ operation: 'goto', destination, options }); return this.action(); },
    async reload(options) { calls.push({ operation: 'reload', options }); return this.action(); },
    async waitForLoadState(state) { calls.push({ waitForLoadState: state }); },
    async evaluate() { calls.push({ drain: true }); },
    async action() { page.emit('load'); return response; },
  });
  const observer = await navigationObserver({ page, engine, origin, fatal: value => fatal.push(value),
    log: (...args) => rawLog.push(args) });
  return { page, observer, fatal, calls, response, rawLog };
}

test('exact native predicate uses the complete original line and exact endpoint', () => {
  assert(exactDiagnostic(error(), origin));
  for (const text of [diagnostic(origin) + ' extra', 'Error: ' + diagnostic(origin),
    diagnostic(origin).replace('/attention', '/attention?extra=1'), diagnostic(origin).replace(':37655', ':4321')]) {
    assert(!exactDiagnostic(error(text), origin));
  }
  assert(!exactDiagnostic({ message: diagnostic(origin) }, origin));
});

test('same message outside the window stays fatal, including immediately after destination load', async () => {
  const h = await harness(); h.observer.pageError(error());
  h.page.action = async () => { h.page.emit('load'); h.observer.pageError(error()); return h.response; };
  await h.observer.reload({ waitUntil: 'domcontentloaded' });
  h.observer.pageError(error());
  assert.equal(h.fatal.length, 3); assert.equal(h.observer.rawPageErrors.length, 3);
  assert(!h.observer.ledger.some(row => row.kind === 'classified-native'));
});

for (const text of [diagnostic(origin) + ' extra', diagnostic(origin).replace('/attention', '/not-attention')]) {
  test(`different message/URL inside the window stays fatal: ${text}`, async () => {
    const h = await harness();
    h.page.action = async () => { h.observer.pageError(error(text)); h.page.emit('load'); return h.response; };
    await h.observer.reload();
    assert.equal(h.fatal.length, 1); assert.equal(h.observer.rawPageErrors.length, 1);
    assert(!h.observer.ledger.some(row => row.kind === 'classified-native'));
  });
}

for (const engine of ['webkit', 'chromium']) for (const operation of ['navigate', 'reload']) {
  test(`${engine} ${operation}: only WebKit's exact diagnostic is tolerated before load; options/response/raw logs survive`, async () => {
    const h = await harness(engine), options = { waitUntil: 'domcontentloaded', timeout: 123 };
    h.page.action = async () => { h.observer.pageError(error()); h.page.emit('load'); return h.response; };
    assert.equal(await (operation === 'reload' ? h.observer.reload(options) : h.observer.navigate(origin + '/destination', options)), h.response);
    assert.equal(h.calls[0].operation, operation === 'reload' ? 'reload' : 'goto');
    assert.equal(h.calls[0].options, options);
    assert.deepEqual(h.calls.slice(1), [{ waitForLoadState: 'load' }, { drain: true }]);
    assert.equal(h.fatal.length, engine === 'webkit' ? 0 : 1);
    assert.equal(h.observer.ledger.filter(row => row.kind === 'classified-native').length, engine === 'webkit' ? 1 : 0);
    assert(h.rawLog.some(row => row[0] === 'PAGEERROR'));
    assert.equal(h.observer.rawPageErrors.length, 1);
    assert.equal(h.observer.rawPageErrors[0].stack, savedError.stack);
    assert.equal(h.observer.rawPageErrors[0].name, savedError.name);
    assert.equal(h.observer.rawPageErrors[0].message, savedError.message);
  });
}

test('a rejected navigation or page close ends tolerance; failures are propagated without retry', async () => {
  const h = await harness();
  h.page.action = async () => { throw new Error('intentional navigation failure'); };
  await assert.rejects(h.observer.reload(), /intentional navigation failure/);
  h.observer.pageError(error());
  h.page.action = async () => { h.page.emit('close'); h.observer.pageError(error()); return h.response; };
  await h.observer.reload();
  assert.equal(h.fatal.length, 2); assert.equal(h.calls.filter(row => row.operation).length, 2);
});

test('overlapping navigation cannot extend a window or replace its owner', async () => {
  const h = await harness(); let release;
  h.page.action = () => new Promise(resolve => { release = () => { h.page.emit('load'); resolve(h.response); }; });
  const first = h.observer.reload();
  await assert.rejects(h.observer.reload(), /overlapping/);
  release(); await first;
  h.observer.pageError(error()); assert.equal(h.fatal.length, 1);
});

test('browser control matrix is exactly the four requested controls on either engine', () => {
  const { CELLS, cellsFor } = require('./navigation-errors.cjs');
  assert.deepEqual(CELLS, ['same-message-outside-window', 'different-message-inside-window',
    'different-url-inside-window', 'cold-reload-attention-in-flight']);
  for (const engine of ['webkit', 'chromium']) assert.deepEqual(cellsFor(engine), CELLS);
});

test('within retains caller-owned evidence, timing schema, results and rejection', async () => {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), 'navigation-within-'));
  try {
    const first = path.join(directory, 'first'), second = path.join(directory, 'second');
    fs.mkdirSync(first); fs.mkdirSync(second);
    assert.equal(await within('first', async () => 123, { evidenceDirectory: first, timings: true }), 123);
    await assert.rejects(within('second', async () => { throw new Error('intentional case failure'); },
      { evidenceDirectory: second, timings: true }), /intentional case failure/);
    const read = target => fs.readFileSync(path.join(target, 'cases.jsonl'), 'utf8').trim().split('\n').map(JSON.parse);
    assert.equal(read(first)[0].passed, true); assert.equal(read(second)[0].passed, false);
    await within('disabled', async () => {}, { evidenceDirectory: first, timings: false });
    assert.equal(read(first).length, 1);
  } finally { fs.rmSync(directory, { recursive: true }); }
});

test('real fixture primary and peer fatal diagnostics retain injected logger and crash handling (no browser)', async () => {
  const logs = [], pages = [];
  const context = {
    setDefaultTimeout() {}, setDefaultNavigationTimeout() {}, async route() {}, async routeWebSocket() {},
    async newPage() {
      const locator = { first() { return this; }, async waitFor() {}, async getAttribute() { return 'off'; } };
      const page = Object.assign(new EventEmitter(), {
        setDefaultTimeout() {}, setDefaultNavigationTimeout() {}, async route() {}, async addInitScript() {},
        async evaluate() {}, async waitForFunction() {}, locator: () => locator,
        async goto() { this.emit('load'); },
      });
      pages.push(page); return page;
    },
    async close() { for (const page of pages) page.emit('close'); },
  };
  const browser = { async newContext() { return context; }, browserType: () => ({ name: () => 'webkit' }) };
  const log = { log(...args) { assert.equal(this, log); logs.push(args); },
    error(...args) { assert.equal(this, log); logs.push(args); } };
  const f = await fixture(browser, origin, { width: 1440, height: 1000 }, 'dark',
    { choice: 'off', effective: 'off', os: 'no-preference' }, () => {}, {}, { log, instrumentation: false });
  try {
    f.page.emit('pageerror', new Error('intentional primary error'));
    await assert.rejects(f.crash, /intentional primary error/);
    assert(logs.some(row => row[0] === 'PAGEERROR')); assert(logs.some(row => row[0] === 'CRASH DIAGNOSTIC'));
    const peer = await context.newPage(), observer = await f.observePage(peer);
    peer.emit('pageerror', new Error('intentional peer error'));
    assert.equal(f.errors.length, 2); assert.equal(observer.rawPageErrors.length, 1);
  } finally { await context.close(); }
});

test('pre-navigation narrow setup still rejects external origins and falls back for foreign requests', async () => {
  const { install } = require('./narrow-fixtures.cjs'); let handler;
  const context = { pages: () => [], async route(_pattern, value) { handler = value; } };
  await install(context, origin); let fallback = false;
  await handler({ request: () => ({ url: () => 'http://example.invalid/api/v1/docs' }), fallback() { fallback = true; } });
  assert(fallback);
  await assert.rejects(install(context, 'https://example.invalid'), /loopback motion fixture/);
  await assert.rejects(install(context), /loopback motion fixture/);
});
