#!/usr/bin/env node
'use strict';

// node --test crates/c6-ui/tests/crash.cjs
// VM tests need no build/browser. Opt into mounted production DOM coverage:
// CRASH_BROWSER=1 ROOT=/built/dist ENGINE=both node --test crates/c6-ui/tests/crash.cjs
const assert = require('node:assert/strict');
const { test } = require('node:test');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const source = fs.readFileSync(path.join(__dirname, '../js/crash.js'), 'utf8');
const PRIVATE = 'PRIVATE_SENTINEL_7c01';

class Element {
  constructor(tag = 'div', attrs = {}) {
    this.tag = tag;
    this.attrs = { ...attrs };
    this.children = [];
    this.listeners = {};
    this.style = {};
    this.id = this.title = this.textContent = PRIVATE;
    this.reads = [];
  }
  append(...children) { this.children.push(...children); }
  setAttribute(name, value) { this.attrs[name] = String(value); }
  getAttribute(name) { this.reads.push(name); return this.attrs[name] ?? null; }
  addEventListener(name, fn) { this.listeners[name] = fn; }
  querySelectorAll(selector) {
    assert.match(selector, /^\[data-(tile-motion|pane-motion|pane-kind)\]$/);
    const attr = selector.slice(1, -1);
    const found = [];
    for (const child of this.children) {
      if (Object.hasOwn(child.attrs, attr)) found.push(child);
      found.push(...child.querySelectorAll(selector));
    }
    return found;
  }
  querySelector(selector) { return this.querySelectorAll(selector)[0] || null; }
}

function pane(key, kind) {
  const shell = new Element('div', { 'data-pane-motion': key });
  if (kind !== undefined) shell.append(new Element('div', { 'data-pane-kind': kind }));
  return shell;
}

function tile(key, ...panes) {
  const el = new Element('div', { 'data-tile-motion': key });
  el.append(...panes);
  return el;
}

function fixture({ tiles = [], ua = 'Mozilla/5.0 Version/18.6 Safari/605.1.15', absent = false, fetchFails = false, noBody = false } = {}) {
  const root = new Element('html');
  const body = new Element('body');
  root.append(body);
  body.append(...tiles);
  const made = [];
  const document = {
    body: noBody ? null : body,
    documentElement: root,
    createElement(tag) { const el = new Element(tag); made.push(el); return el; },
    getElementById(id) { return made.find(el => el.id === id) || null; },
    querySelectorAll(selector) { return root.querySelectorAll(selector); },
  };
  const events = {};
  const window = { addEventListener(name, fn) { events[name] = fn; } };
  const requests = [];
  let reloads = 0;
  const location = { pathname: '/home', search: `?q=${PRIVATE}`, href: `https://example.test/home?q=${PRIVATE}`, reload() { reloads++; } };
  const globals = {
    window, document, location, WebAssembly,
    fetch(url, options) {
      requests.push({ url, options, report: JSON.parse(options.body) });
      return fetchFails ? Promise.reject(new Error('offline')) : Promise.resolve();
    },
  };
  Object.defineProperty(globals, 'localStorage', { get() { throw new Error('must not read saved layout'); } });
  if (absent) delete document.querySelectorAll;
  else globals.navigator = { userAgent: ua };
  const context = vm.createContext(globals);
  vm.runInContext(source, context, { filename: 'crash.js' });
  return {
    root, body, made, window, requests,
    emit(name, value) { events[name](value); },
    run(code) { return vm.runInContext(code, context, { filename: 'app-panic.js' }); },
    report() { assert.equal(requests.length, 1); return requests[0].report; },
    banner() { return made.filter(el => el.id === 'c6-crashed'); },
    reloads() { return reloads; },
  };
}

test('panic message captures its own JS Error stack, mounted types and UA; later trap/rejection cannot replace it', () => {
  const f = fixture({ tiles: [tile('2', pane('3', 'session'), pane('4', 'session_changes')), tile('5', pane('6', 'settings'))] });
  f.run('function appPanicFrame() { window.c6Crashed("panicked at workspace.rs:123"); } appPanicFrame();');
  const first = f.report();
  assert.equal(first.message, 'panicked at workspace.rs:123');
  assert.equal(first.page, '/home');
  assert.match(first.stack, /^mounted=t2\/p3:session,t2\/p4:session_changes,t5\/p6:settings\nua=Mozilla\/5\.0 Version\/18\.6 Safari\/605\.1\.15\nError: c6Crashed\n/);
  assert.match(first.stack, /appPanicFrame.*app-panic\.js/);
  assert.match(first.stack, /show.*crash\.js/);
  f.emit('error', { error: new WebAssembly.RuntimeError('unreachable') });
  f.emit('unhandledrejection', { reason: new Error('already borrowed') });
  f.window.c6Crashed('later panic', 'later stack');
  assert.deepEqual(f.report(), first);
  assert.equal(f.banner().length, 1);
  const request = f.requests[0];
  assert.equal(request.url, '/api/v1/ui/error');
  assert.equal(request.options.method, 'POST');
  assert.equal(request.options.credentials, 'same-origin');
  assert.equal(request.options.headers['Content-Type'], 'application/json');
  assert.deepEqual(Object.keys(first).sort(), ['message', 'page', 'stack']);
  const bar = f.banner()[0];
  assert.equal(bar.attrs.role, 'alert');
  assert.equal(bar.children[0].textContent, 'Something broke, reload');
  assert.equal(bar.children[1].textContent, 'Reload');
  bar.children[1].listeners.click();
  assert.equal(f.reloads(), 1);
});

test('Wasm error and stuck-binding rejection preserve the original Error stack', () => {
  for (const event of ['error', 'unhandledrejection']) {
    const f = fixture();
    const error = event === 'error' ? new WebAssembly.RuntimeError('unreachable') : new Error('recursive use of an object detected');
    error.stack = 'Error: original\n    at app_level_wasm (app.wasm:123:45)';
    f.emit(event, event === 'error' ? { error } : { reason: error });
    assert.equal(f.report().message, error.message);
    assert(f.report().stack.endsWith(error.stack));
    assert(!f.report().stack.includes('Error: c6Crashed'));
    f.window.c6Crashed('subsequent panic');
    assert.equal(f.report().message, error.message);
    assert.equal(f.banner().length, 1);
  }
});

test('only fatal traps and stuck-binding errors trigger reporting', () => {
  const f = fixture();
  for (const error of [new Error('ordinary network error'), new TypeError('bad response'), null, undefined]) {
    f.emit('error', { error });
    f.emit('unhandledrejection', { reason: error });
  }
  assert.equal(f.requests.length, 0);
  assert.equal(f.banner().length, 0);
  for (const message of ['closure invoked after being dropped', 'closure invoked recursively or after being dropped', 'already mutably borrowed', 'already borrowed']) {
    const f = fixture();
    f.emit('unhandledrejection', { reason: { message } });
    assert.equal(f.report().message, message);
    assert.match(f.report().stack, /Error: c6Crashed/);
  }
});

test('snapshot reads only numeric keys and allowlisted static types; private payloads never leak', () => {
  const p = pane('3', 'search');
  const t = tile('2', p, pane('4', PRIVATE), pane(PRIVATE, 'session'));
  const f = fixture({ tiles: [t, tile(PRIVATE, pane('99', 'docs')), tile('7', pane('8'))] });
  f.window.c6Crashed('panic', 'safe stack');
  assert.match(f.report().stack, /^mounted=t2\/p3:search,t2\/p4:unknown,t7\/p8:pending\n/);
  assert(!JSON.stringify(f.report()).includes(PRIVATE));
  const checkReads = el => {
    for (const attr of el.reads) assert(['data-tile-motion', 'data-pane-motion', 'data-pane-kind'].includes(attr));
    el.children.forEach(checkReads);
  };
  checkReads(t);
});

test('content marker, not a pane shell or desired layout, determines the mounted kind', () => {
  const p = pane('3');
  const f = fixture({ tiles: [tile('2', p)] });
  // A retained old content wrapper reports its old static kind, irrespective
  // of any navigation intent; deferred/unmounted content has no marker.
  p.append(new Element('div', { 'data-pane-kind': 'docs' }));
  f.window.c6Crashed('panic', 'stack');
  assert.match(f.report().stack, /^mounted=t2\/p3:docs\n/);
  const empty = fixture({ tiles: [tile('2', pane('3'))] });
  empty.window.c6Crashed('panic', 'stack');
  assert.match(empty.report().stack, /^mounted=t2\/p3:pending\n/);

  const rust = fs.readFileSync(path.join(__dirname, '../src/workspace.rs'), 'utf8');
  const column = rust.slice(rust.indexOf('fn ColumnView('), rust.indexOf('fn ColumnHeader('));
  assert.match(column, /<Show when=move \|\| mounted\.get\(\)>\s*<For[\s\S]*?let kind = content\.0\.crash_kind\(\); view! \{\s*<div data-pane-kind=kind[\s\S]*?<InitialView view=content\.0 col \/>\s*<\/div>\s*\}\}\s*<\/For>\s*<\/Show>/);
  assert.equal((column.match(/data-pane-kind=/g) || []).length, 1);
  const method = rust.slice(rust.indexOf('fn crash_kind('), rust.indexOf('pub fn session('));
  const rustKinds = [...method.matchAll(/=> "([a-z_]+)"/g)].map(m => m[1]).sort();
  const jsKinds = JSON.parse('[' + source.match(/const kinds = new Set\(\[([\s\S]*?)\]\)/)[1].replace(/,\s*$/, '') + ']').sort();
  const enumSource = rust.slice(rust.indexOf('pub enum View {')).split('\n}')[0];
  const variants = [...enumSource.matchAll(/^    ([A-Z][A-Za-z]+)(?:,| \{)/gm)].map(m => m[1]).sort();
  const matched = [...method.matchAll(/Self::([A-Z][A-Za-z]+)/g)].map(m => m[1]).sort();
  assert(variants.length > 0);
  assert.deepEqual(matched, variants, 'every enum variant has exactly one fixed crash kind');
  assert.deepEqual(rustKinds, jsKinds);
  assert.equal((column.match(/class="@container\/view /g) || []).length, 1, 'reuse the existing content wrapper; no extra DOM node');
});

test('bounded diagnostics precede useful frames within the head\'s 2000-character stack cap', () => {
  const tiles = Array.from({ length: 100 }, (_, i) => tile(String(i + 1), pane(String(i + 101), 'session_terminal')));
  const f = fixture({ tiles, ua: 'Safari\nVersion/18.6\r' + 'x'.repeat(1000) });
  f.window.c6Crashed('panic', 'Error: original\n    at first_useful_wasm_frame\n' + 'other frame\n'.repeat(500));
  const stack = f.report().stack;
  const lines = stack.split('\n');
  assert(lines[0].length <= 300);
  assert(lines[0].endsWith('...'));
  assert(lines[1].length <= 163);
  assert(lines[1].startsWith('ua=Safari Version/18.6 '));
  assert(lines[0].length + lines[1].length + 2 < 500);
  const logged = [...stack].slice(0, 2000).join('');
  assert(logged.startsWith('mounted='));
  assert(logged.includes('at first_useful_wasm_frame'));
});

test('missing diagnostics, missing body and failed reporting do not prevent the banner', async () => {
  const f = fixture({ absent: true, noBody: true, fetchFails: true });
  f.window.c6Crashed('panic');
  assert.match(f.report().stack, /^mounted=unavailable\nua=unavailable\nError: c6Crashed/);
  assert.equal(f.banner().length, 1);
  assert(f.root.children.includes(f.banner()[0]));
  f.window.c6Crashed('again');
  assert.equal(f.requests.length, 1);
  assert.equal(f.banner().length, 1);
  await Promise.resolve(); // the rejected fetch is consumed by production .catch
});

test('synchronous fetch failure still shows exactly one reload banner', () => {
  const f = fixture();
  f.run('fetch = () => { throw new Error("offline"); }; window.c6Crashed("panic");');
  f.window.c6Crashed('again');
  assert.equal(f.banner().length, 1);
});

const browserTest = process.env.CRASH_BROWSER === '1' ? test : test.skip;
browserTest('production mounted markers: pending, focus, single-copy dedupe and close', { timeout: Number(process.env.C6_TEST_DEADLINE_SECS || 300) * 1000 }, async () => {
  const { serve, fixture, settle, launchBrowser } = require('./motion.cjs');
  const playwright = require(process.env.PLAYWRIGHT_CORE || '/home/zheilbron/.bun/install/cache/playwright-core@1.50.1@@@1');
  const { server, origin } = await serve();
  const motion = { choice: 'off', effective: 'off', os: 'no-preference' };
  const layout = {
    tiles: [
      { key: 2, columns: [{ key: 3, view: { view: 'session', id: 'parent' } }], cursor: 0, active: 3, used: 3, back: [], forward: [], stack: 0, minimized: false },
      { key: 4, columns: [{ key: 5, view: { view: 'workers' } }], cursor: 0, active: 5, used: 5, back: [], forward: [], stack: 1, minimized: false },
    ], focus: 0, next: 6, full: false,
  };
  try {
    for (const engine of (process.env.ENGINE || 'both') === 'both' ? ['chromium', 'webkit'] : [process.env.ENGINE]) {
      const browser = await launchBrowser(playwright, engine);
      let f, release;
      const held = new Promise(resolve => { release = resolve; });
      const reports = [];
      try {
        f = await fixture(browser, origin, { width: 1800, height: 1000 }, 'light', motion, progress => { f = progress; }, {}, {
          layout, ready: '[data-pane-motion="5"]', instrumentation: false,
          setup: async ({ context }) => {
            await context.route('**/api/**', async route => {
              const pathname = new URL(route.request().url()).pathname;
              if (pathname.endsWith('/ui/error')) {
                reports.push(route.request().postDataJSON());
                return route.fulfill({ status: 204 });
              }
              if (pathname.endsWith('/sessions/parent')) await held;
              return route.fallback();
            });
          },
        });
        const { page } = f;
        const checks = async () => {
          const session = page.locator('[data-pane-motion="3"]');
          const workers = page.locator('[data-pane-motion="5"]');
          await session.locator('[data-pane-kind="session"]').waitFor({ state: 'attached' });
          assert.equal(await workers.locator('[data-pane-kind]').count(), 0, `${engine}: deferred shell must not claim desired workers content`);
          assert.equal(await page.locator('[data-pane-motion]').count(), 2);
          release();
          await workers.locator('[data-pane-kind="workers"]').waitFor({ state: 'attached' });
          await session.locator('textarea').fill(PRIVATE);
          await settle(page);
          assert(await page.evaluate(() => [...document.querySelectorAll('[data-pane-motion]')].every(pane => {
            const markers = pane.querySelectorAll('[data-pane-kind]');
            return markers.length === 1 && markers[0].parentElement === pane && markers[0].classList.contains('@container/view');
          })), `${engine}: the existing container is the marker, with no new intervening node`);
          await page.evaluate(() => {
            window.__crashWorkerMarker = document.querySelector('[data-pane-kind="workers"]');
            window.__crashSessionMarker = document.querySelector('[data-pane-kind="session"]');
          });
          // Real navigation focuses the already-mounted Workers destination,
          // rather than rebuilding either content wrapper or duplicating it.
          await session.click({ position: { x: 2, y: 2 } });
          const before = await page.evaluate(() => JSON.parse(localStorage.getItem('c6.home.workspace.v2')));
          await page.locator('header a[href$="/workers"]:visible').first().click();
          await settle(page);
          const after = await page.evaluate(() => JSON.parse(localStorage.getItem('c6.home.workspace.v2')));
          assert.equal(after.tiles[after.focus].key, 4);
          assert.deepEqual(after.tiles.map(t => t.columns), before.tiles.map(t => t.columns));
          assert.deepEqual(after.tiles.map(t => t.back), before.tiles.map(t => t.back));
          assert(await page.evaluate(() => document.querySelector('[data-pane-kind="workers"]') === __crashWorkerMarker &&
            document.querySelector('[data-pane-kind="session"]') === __crashSessionMarker));
          assert.equal(await session.locator('textarea').inputValue(), PRIVATE);
          await page.locator('[data-tile-motion="4"]').getByRole('button', { name: 'Close this tile', exact: true }).click();
          await workers.waitFor({ state: 'detached' });
          assert.equal(await page.locator('[data-pane-kind="workers"]').count(), 0);
          assert.equal(await page.locator('[data-pane-motion]').count(), 1);
          assert.equal(await session.locator('[data-pane-kind="session"]').count(), 1);
          const reported = page.waitForResponse(response => new URL(response.url()).pathname.endsWith('/ui/error'));
          await page.evaluate(async () => {
            // Exercise the same synchronous Wasm → JS stack boundary as the
            // Rust panic hook, without deliberately breaking the running app.
            // (module (import "e" "panic" (func))
            //   (func (export "crash") call 0))
            const { instance } = await WebAssembly.instantiate(new Uint8Array([
              0, 97, 115, 109, 1, 0, 0, 0,
              1, 4, 1, 96, 0, 0,
              2, 11, 1, 1, 101, 5, 112, 97, 110, 105, 99, 0, 0,
              3, 2, 1, 0,
              7, 9, 1, 5, 99, 114, 97, 115, 104, 0, 1,
              10, 6, 1, 4, 0, 16, 0, 11,
            ]), { e: { panic: () => window.c6Crashed('intentional crash diagnostic test') } });
            function appJSFrame() { instance.exports.crash(); }
            appJSFrame();
          });
          await reported;
          assert.equal(reports.length, 1);
          const logged = [...reports[0].stack].slice(0, 2000).join('');
          assert.match(logged, /^mounted=t2\/p3:session\nua=/);
          assert(logged.includes(await page.evaluate(() => navigator.userAgent.slice(0, 160))));
          assert.match(logged, /appJSFrame/);
          assert.match(logged, /wasm-function\[1\]/);
          assert(!JSON.stringify(reports[0]).includes(PRIVATE));
          await page.evaluate(() => {
            window.c6Crashed('later panic');
            dispatchEvent(new ErrorEvent('error', { error: new WebAssembly.RuntimeError('unreachable') }));
            dispatchEvent(new Event('unhandledrejection'));
          });
          await settle(page);
          assert.equal(reports.length, 1);
          assert.equal(await page.locator('#c6-crashed[role="alert"]').count(), 1);
          assert.deepEqual(f.errors, [], `${engine}: unexpected browser errors are still fatal`);
        };
        await Promise.race([checks(), f.crash]);
      } finally {
        release();
        await f?.context.close();
        await browser.close();
      }
    }
  } finally {
    await new Promise(resolve => server.close(resolve));
  }
});
