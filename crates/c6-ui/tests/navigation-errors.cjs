#!/usr/bin/env node
'use strict';

// App-free, serial native-navigation controls. No production build or real head.
// ENGINE=both EVIDENCE=... node crates/c6-ui/tests/navigation-errors.cjs
const assert = require('node:assert/strict');
const fs = require('node:fs');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const { launchBrowser, within } = require('./motion.cjs');
const { diagnostic, exactDiagnostic, navigationObserver } = require('./browser-navigation.cjs');
const DEADLINE = Number(process.env.C6_TEST_DEADLINE_SECS || 15) * 1000;
const EVIDENCE = process.env.EVIDENCE || path.join(os.homedir(), '.cache/c6-helper/browser-fast/navigation-errors');
const deferred = () => { let resolve; const promise = new Promise(r => { resolve = r; }); return { promise, resolve }; };
async function bounded(promise, label) {
  let timer;
  try { return await Promise.race([promise, new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(`${label}: operation deadline`)), DEADLINE);
  })]); } finally { clearTimeout(timer); }
}
async function serve() {
  const requests = [];
  const server = http.createServer((req, res) => {
    requests.push(req.url);
    if (req.url === '/api/v1/attention' && server.redirectAttention) {
      res.writeHead(302, { location: server.redirectOrigin + '/cors' }); res.end(); return;
    }
    res.setHeader('content-type', req.url.startsWith('/api/') || req.url === '/cors' ? 'application/json' : 'text/html');
    res.end(req.url.startsWith('/api/') || req.url === '/cors' ? '{}' : '<!doctype html><title>navigation controls</title><body>owned fixture</body>');
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  return { server, requests, origin: `http://127.0.0.1:${server.address().port}`,
    async close() { server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); } };
}

const CELLS = [
  'live-baseline', 'held-native', 'ordinary-throw', 'ordinary-rejection', 'actual-cors', 'redirect-cors',
  'diagnostic-live', 'outside-explicit-window', 'different-endpoint', 'different-origin', 'different-signature',
  'missing-identity', 'missing-beforeunload', 'custom-route-ineligible',
  'exact-message-throw', 'exact-message-rejection', 'exact-stack-throw', 'exact-stack-rejection',
  'console-error-preserved', 'rust-panic-console', 'wasm-trap', 'destination-throw',
];
// Retained old-context evaluation timed out in the held Chromium probe. These
// native-refusal/lifecycle challenges require an inspectable departing document
// and are WebKit controls, not skipped Chromium passes. Chromium runs live fatal
// controls and a real, ungated replacement/destination error.
const HELD = ['held-native', 'outside-explicit-window', 'different-endpoint', 'different-origin',
  'different-signature', 'missing-identity', 'missing-beforeunload', 'custom-route-ineligible',
  'exact-message-throw', 'exact-message-rejection', 'exact-stack-throw', 'exact-stack-rejection',
  'console-error-preserved'];
const cellsFor = engine => engine === 'webkit' ? CELLS : CELLS.filter(name => !HELD.includes(name));

async function cell(browser, engine, name) {
  const primary = await serve(), secondary = await serve();
  const { origin } = primary;
  primary.server.redirectOrigin = secondary.origin;
  const context = await browser.newContext({ serviceWorkers: 'block' });
  const result = { engine, name, origin, fatal: [], console: [], requests: [], rawLog: [] };
  const step = stage => { result.stage = stage; if (process.env.DEBUG_FIXTURE) console.log(`  ${engine}/${name}: ${stage}`); };
  let observer, navigation, old, held, release;
  try {
    const page = await context.newPage();
    page.setDefaultTimeout(DEADLINE); page.setDefaultNavigationTimeout(DEADLINE);
    let eligible = true, proved = false;
    observer = await navigationObserver({ page, engine, origin, baselineAttention: () => eligible && proved,
      fatal: error => result.fatal.push(String(error.stack || error)),
      log: (...args) => result.rawLog.push(args.join(' ')) });
    page.on('pageerror', observer.pageError);
    // Match the existing fixture's console semantics. The navigation helper has
    // no console listener/filter and cannot remove these failures or raw logs.
    page.on('console', message => {
      result.console.push({ type: message.type(), text: message.text() });
      if (/bad stream message|unreachable|panicked at|ResizeObserver loop/.test(message.text())) result.fatal.push(message.text());
    });
    for (const event of ['request', 'requestfailed', 'requestfinished']) page.on(event, request => {
      result.requests.push({ event, url: request.url(), ...(event === 'requestfailed' ? { failure: request.failure() } : {}) });
    });
    const attention = origin + '/api/v1/attention';
    const mock = async route => { await route.fulfill({ status: 200, json: {} }); proved = true; };
    await context.route(attention, mock);
    await page.goto(origin + '/initial');
    // Prove the specific owned 200/no-redirect mock works before replacement.
    assert.deepEqual(await page.evaluate(async url => {
      const response = await fetch(url); await response.text();
      return { status: response.status, url: response.url, redirected: response.redirected };
    }, attention), { status: 200, url: attention, redirected: false });
    await observer.drain();
    assert.equal(result.fatal.length, 0);

    const heldCell = HELD.includes(name);
    if (heldCell) {
      step('capture old context');
      held = deferred(); release = deferred();
      await context.route(origin + '/replacement', async route => { held.resolve(); await release.promise; await route.continue(); });
      old = await page.evaluateHandle(() => window.__navigationDocument);
      result.old = await bounded(old.evaluate(probe => probe.snapshot()), 'old snapshot');
      step('start replacement');
      navigation = name === 'outside-explicit-window' ? page.goto(origin + '/replacement') :
        observer.navigate(origin + '/replacement', { waitUntil: 'domcontentloaded' });
      navigation.catch(() => {}); // Awaited below/at cleanup; never an app Promise.
      await bounded(held.promise, 'held destination request');
      step('destination held; old lifecycle snapshot');
      const departing = await bounded(old.evaluate(probe => probe.snapshot()), 'departing snapshot');
      assert.equal(departing.token, result.old.token);
      assert.equal(departing.url, origin + '/initial');
      assert.equal(departing.beforeunload, true);
      assert.equal(departing.pagehide, false);
      result.departing = departing;
    }

    const synthetic = text => ({ name: 'Fetch API cannot load http', message: text, stack: text + '\nfetch@[native code]' });
    // page.evaluate waits for a pending destination context in Chromium. The
    // explicit old-context handle is also essential to the negative probes.
    const evaluate = (fn, arg) => bounded(old ? old.evaluate((_probe, { source, arg }) =>
      eval(`(${source})`)(arg), { source: String(fn), arg }) : page.evaluate(fn, arg), 'control evaluation');
    step('inject control');
    if (name === 'diagnostic-live') observer.pageError(synthetic(diagnostic(origin)));
    else if (name === 'different-signature') observer.pageError(synthetic(diagnostic(origin) + ' extra'));
    else if (['ordinary-throw', 'ordinary-rejection', 'exact-message-throw', 'exact-message-rejection',
      'exact-stack-throw', 'exact-stack-rejection', 'wasm-trap', 'destination-throw'].includes(name)) {
      if (name === 'destination-throw') await observer.navigate(origin + '/destination', { waitUntil: 'domcontentloaded' });
      await evaluate(({ name, message }) => {
        if (name === 'wasm-trap') {
          const wasm = new WebAssembly.Instance(new WebAssembly.Module(Uint8Array.from([
            0, 97, 115, 109, 1, 0, 0, 0, 1, 4, 1, 96, 0, 0, 3, 2, 1, 0,
            7, 8, 1, 4, 116, 114, 97, 112, 0, 0, 10, 5, 1, 3, 0, 0, 11,
          ])));
          setTimeout(() => wasm.exports.trap(), 0); // Actual WASM unreachable, not just similar error text.
          return;
        }
        const error = new Error(message);
        if (name.startsWith('exact-stack')) { error.name = ''; error.stack = message; }
        if (name.endsWith('rejection')) Promise.reject(error);
        else setTimeout(() => { throw error; }, 0);
      }, { name, message: name.startsWith('exact-') ? diagnostic(origin) : `intentional ${name}` });
    } else if (name === 'rust-panic-console') await evaluate(() => console.error('panicked at navigation negative control'));
    else if (name !== 'live-baseline') {
      if (name === 'missing-identity') await old.evaluate(probe => {
        const snapshot = probe.snapshot.bind(probe); probe.snapshot = () => ({ ...snapshot(), token: 'not-the-old-document' });
      });
      if (name === 'missing-beforeunload') await old.evaluate(probe => {
        const snapshot = probe.snapshot.bind(probe); probe.snapshot = () => ({ ...snapshot(), beforeunload: false });
      });
      if (name === 'custom-route-ineligible' || name === 'actual-cors' || name === 'redirect-cors') eligible = false;
      if (name === 'console-error-preserved') await evaluate(() => console.error('unreachable console negative control'));
      let url = attention;
      if (name === 'different-endpoint') url = origin + '/api/v1/not-attention';
      if (name === 'different-origin') url = secondary.origin + '/api/v1/attention';
      if (name === 'actual-cors') url = secondary.origin + '/cors';
      if (name === 'redirect-cors') {
        primary.server.redirectAttention = true;
        await context.unroute(attention, mock);
      }
      // Intentionally handled fetch only: native diagnostics must still be
      // classified/fatal as appropriate. JS throws/rejections above are NOT caught.
      result.fetch = await evaluate(async url => {
        try { const response = await fetch(url); await response.text(); return { status: response.status }; }
        catch (error) { return { caught: error.message }; }
      }, url);
    }

    step('drain old observations');
    await observer.drain(old); // Dispatch/ack DOM negatives while the old page is held.
    if (heldCell) {
      result.beforeRelease = await bounded(old.evaluate(probe => probe.snapshot()), 'before-release snapshot');
      step('release replacement');
      release.resolve(); assert.equal((await bounded(navigation, 'replacement commit')).status(), 200);
      assert.notEqual((await page.evaluate(() => window.__navigationDocument.snapshot())).token, result.old.token);
    }
    await observer.drain();
    const classified = observer.ledger.filter(row => row.kind === 'classified-native');
    result.classified = classified.length;
    const nativeCell = ['held-native', 'console-error-preserved'].includes(name);
    if (nativeCell && engine === 'webkit') {
      assert.equal(classified.length, 1, 'held native refusal must be positively classified');
      assert.equal(observer.rawPageErrors.length, 1, 'native diagnostic stays in raw ledger');
      assert(result.rawLog.some(line => line.startsWith('PAGEERROR ')), 'native diagnostic stays in raw log');
      assert.equal(result.requests.filter(row => row.url === attention && row.event === 'request').length, 1,
        'refused new load has no second attention request (no unrelated requestfailed needed)');
      assert(!observer.ledger.some(row => ['window.error', 'window.unhandledrejection'].includes(row.event)));
    } else assert.equal(classified.length, 0, 'negative must never be classified');
    const engineOnly = ['actual-cors', 'redirect-cors', 'outside-explicit-window', 'different-endpoint',
      'different-origin', 'missing-identity', 'missing-beforeunload', 'custom-route-ineligible'].includes(name);
    if (name === 'live-baseline' || name === 'held-native') assert.equal(result.fatal.length, 0);
    else if (!engineOnly || engine === 'webkit') assert(result.fatal.length > 0, 'negative must remain fatal');
    if (name.includes('throw') || name.includes('rejection') || name === 'wasm-trap') {
      assert(observer.ledger.some(row => row.event === (name.endsWith('rejection') ? 'window.unhandledrejection' : 'window.error')),
        'independent DOM observer must catch genuine JS failure');
    }
    if (name.startsWith('exact-')) {
      const kind = name.endsWith('rejection') ? 'window.unhandledrejection' : 'window.error';
      assert(observer.ledger.some(row => row.event === kind && row.message.includes(diagnostic(origin))));
    }
    if (name === 'exact-stack-throw') {
      assert(observer.rawPageErrors.some(error => exactDiagnostic(error, origin)), 'genuine JS throw must challenge exact native predicate');
      assert(observer.ledger.some(row => row.kind === 'missing-attribution'), 'DOM failure must veto exact-signature candidate');
    }
    if (name === 'actual-cors' || name === 'redirect-cors') {
      assert(result.fetch.caught);
      assert(result.console.some(row => row.type === 'error'), 'real CORS console evidence must survive');
    }
    result.pass = true;
  } catch (error) { result.failure = String(error.stack || error); }
  finally {
    release?.resolve();
    if (navigation) await bounded(navigation, 'cleanup navigation').catch(error => { result.cleanupFailure = String(error); });
    result.ledger = observer?.ledger;
    result.rawPageErrors = observer?.rawPageErrors;
    result.serverRequests = primary.requests;
    await context.close(); await primary.close(); await secondary.close();
    fs.writeFileSync(path.join(EVIDENCE, `${engine}-${name}.json`), JSON.stringify(result, null, 2) + '\n');
  }
  console.log(`${result.pass ? 'PASS' : 'FAIL'} ${engine}/${name}: classified=${result.classified ?? '?'} fatal=${result.fatal.length}${result.failure ? '\n' + result.failure : ''}`);
  return result;
}

async function main() {
  const engine = process.env.ENGINE || 'both';
  assert(['both', 'chromium', 'webkit'].includes(engine));
  const cells = process.env.CONTROL ? process.env.CONTROL.split(',') : CELLS;
  assert(cells.length > 0 && cells.every(name => CELLS.includes(name)), 'CONTROL must name committed controls');
  const engines = engine === 'both' ? ['chromium', 'webkit'] : [engine];
  const planned = engines.reduce((count, engine) => count + cells.filter(name => cellsFor(engine).includes(name)).length, 0);
  assert(planned > 0, 'no applicable controls for selected engine');
  fs.mkdirSync(EVIDENCE, { recursive: true });
  const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core'), results = [];
  try {
    for (const name of engines) {
      const browser = await launchBrowser(playwright, name);
      try { for (const test of cells.filter(test => cellsFor(name).includes(test))) results.push(await within(`${name}/${test}`, () => cell(browser, name, test))); }
      finally { await browser.close(); }
    }
  } finally {
    fs.writeFileSync(path.join(EVIDENCE, 'summary.json'), JSON.stringify({ passed: results.filter(row => row.pass).length,
      total: results.length, planned, results }, null, 2) + '\n');
  }
  assert.equal(results.filter(row => row.pass).length, results.length, 'all controls must pass; no retries or discarded cells');
  console.log(`ALL PASS (${results.length}/${results.length} controls)`);
}

module.exports = { CELLS, cellsFor };
if (require.main === module) main().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
