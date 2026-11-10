#!/usr/bin/env node
'use strict';

// Four arrival-window controls, against real production assets. No builds,
// retries, redirects, provenance flags, or retained execution-context proofs.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const http = require('node:http');
const os = require('node:os');
const path = require('node:path');
const { fixture, launchBrowser, settle, within } = require('./motion.cjs');
const { diagnostic } = require('./browser-navigation.cjs');
const ROOT = path.resolve(process.env.ROOT || path.join(__dirname, '../dist'));
const EVIDENCE = process.env.EVIDENCE || path.join(os.homedir(), '.cache/c6-helper/browser-fast/navigation-errors');
const DEADLINE = Number(process.env.C6_TEST_DEADLINE_SECS || 15) * 1000;
const CELLS = ['same-message-outside-window', 'different-message-inside-window',
  'different-url-inside-window', 'cold-reload-attention-in-flight'];
const cellsFor = () => CELLS;
const deferred = () => { let resolve; const promise = new Promise(r => { resolve = r; }); return { promise, resolve }; };
async function bounded(promise, label) {
  let timer;
  try { return await Promise.race([promise, new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error(`${label}: operation deadline`)), DEADLINE);
  })]); } finally { clearTimeout(timer); }
}

async function serve() {
  assert(fs.existsSync(path.join(ROOT, 'index.html')), `No production assets at ${ROOT}`);
  const requests = [], attention = deferred(), document = deferred(), release = deferred();
  const server = http.createServer((req, res) => {
    requests.push(req.url);
    if (req.url === '/api/v1/attention') {
      attention.resolve(); // Intentionally keep the real HTTP fetch in flight.
      return;
    }
    const respond = () => {
      if (res.destroyed) return;
      const pathname = decodeURIComponent(new URL(req.url, 'http://127.0.0.1').pathname);
      const file = path.resolve(ROOT, pathname === '/' || pathname.startsWith('/r/') ? 'index.html' : `.${pathname}`);
      if (!file.startsWith(ROOT + path.sep) || !fs.existsSync(file) || !fs.statSync(file).isFile()) {
        res.writeHead(404); res.end('Not found'); return;
      }
      const mime = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.wasm': 'application/wasm',
        '.json': 'application/json', '.svg': 'image/svg+xml', '.png': 'image/png' };
      res.writeHead(200, { 'content-type': mime[path.extname(file)] || 'application/octet-stream', 'cache-control': 'no-store' });
      fs.createReadStream(file).pipe(res);
    };
    if (server.holdDocument && req.url.startsWith('/r/')) {
      document.resolve(); release.promise.then(respond);
    } else respond();
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  return { origin: `http://127.0.0.1:${server.address().port}`, server, requests, attention, document, release,
    async close() { release.resolve(); server.closeAllConnections(); await new Promise(resolve => server.close(resolve)); } };
}

async function cell(browser, engine, name) {
  const host = await serve(), { origin } = host;
  const result = { engine, name, pass: false, origin, rawLog: [] };
  let f, navigation, holdAttention = false;
  try {
    f = await fixture(browser, origin, { width: 1440, height: 1000 }, 'dark',
      { choice: 'off', effective: 'off', os: 'no-preference' }, progress => { f = progress; }, {}, {
        instrumentation: false,
        log: { log: (...args) => result.rawLog.push(args.join(' ')), error: (...args) => result.rawLog.push(args.join(' ')) },
        setup: async ({ page }) => page.route(origin + '/api/v1/attention', route =>
          holdAttention ? route.continue() : route.fallback()),
      });
    const positive = name === 'cold-reload-attention-in-flight';
    if (positive) {
      holdAttention = true;
      await f.page.evaluate(url => {
        // These handled fetches belong only to this diagnostic control, not the
        // application. The first is genuinely in flight at the real reload;
        // the unload fetch exercises WebKit's native AccessControl diagnostic.
        void fetch(url).catch(() => {});
        addEventListener('beforeunload', () => { void fetch(url).catch(() => {}); }, { once: true });
      }, origin + '/api/v1/attention');
      await bounded(host.attention.promise, 'actual attention request in flight');
      result.attentionInFlightBeforeReload = true;
      holdAttention = false;
      await f.reload({ waitUntil: 'domcontentloaded' });
    } else {
      if (name !== 'same-message-outside-window') {
        host.server.holdDocument = true;
        navigation = f.reload({ waitUntil: 'domcontentloaded' });
        navigation.catch(() => {}); // Owned navigation, awaited below and at cleanup.
        await bounded(host.document.promise, 'held replacement request');
      }
      const text = name === 'different-message-inside-window' ? diagnostic(origin) + ' extra' :
        name === 'different-url-inside-window' ? diagnostic(origin).replace('/api/v1/attention', '/api/v1/not-attention') : diagnostic(origin);
      result.injection = 'direct policy control; positive uses only real browser pageerrors';
      f.navigation.pageError({ stack: text + '\nfetch@[native code]', message: text });
      await assert.rejects(f.crash, /browser crash/);
      host.release.resolve();
      if (navigation) await navigation;
    }
    await f.page.locator('textarea').first().waitFor();
    await settle(f.page); await f.navigation.drain();
    result.destinationHydrated = true;
    result.classified = f.navigation.ledger.filter(row => row.kind === 'classified-native').length;
    if (positive) {
      assert.deepEqual(f.errors, [], 'real cold reload and destination hydration remain error-free');
      assert(result.attentionInFlightBeforeReload);
      if (engine === 'webkit') {
        assert(result.classified > 0, 'must observe a real native refusal, not an uneventful reload');
        assert(f.navigation.rawPageErrors.length > 0 && result.rawLog.some(row => row.startsWith('PAGEERROR ')));
      } else assert.equal(result.classified, 0, 'Chromium never receives WebKit tolerance');
    } else {
      assert.equal(result.classified, 0); assert.equal(f.errors.length, 1, 'the challenged error remains fatal');
    }
    result.pass = true;
  } catch (error) { result.failure = String(error.stack || error); }
  finally {
    host.release.resolve();
    if (navigation) await bounded(navigation, 'cleanup replacement').catch(error => { result.cleanupFailure = String(error); });
    result.errors = f?.errors; result.ledger = f?.navigation?.ledger; result.rawPageErrors = f?.navigation?.rawPageErrors;
    result.serverRequests = host.requests;
    await f?.context.close(); await host.close();
    fs.writeFileSync(path.join(EVIDENCE, `${engine}-${name}.json`), JSON.stringify(result, null, 2) + '\n');
  }
  console.log(`${result.pass ? 'PASS' : 'FAIL'} ${engine}/${name}: classified=${result.classified ?? '?'}${result.failure ? '\n' + result.failure : ''}`);
  return result;
}

async function main() {
  const engine = process.env.ENGINE || 'both';
  assert(['both', 'chromium', 'webkit'].includes(engine));
  const engines = engine === 'both' ? ['chromium', 'webkit'] : [engine];
  fs.mkdirSync(EVIDENCE, { recursive: true });
  const results = [], playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
  try {
    for (const engine of engines) {
      const browser = await launchBrowser(playwright, engine);
      try { for (const name of CELLS) results.push(await within(`${engine}/${name}`, () => cell(browser, engine, name))); }
      finally { await browser.close(); }
    }
  } finally {
    fs.writeFileSync(path.join(EVIDENCE, 'summary.json'), JSON.stringify({ passed: results.filter(row => row.pass).length,
      total: results.length, planned: engines.length * CELLS.length, results }, null, 2) + '\n');
  }
  assert(results.every(row => row.pass), 'all four controls must pass; no retries or discarded cells');
}

module.exports = { CELLS, cellsFor };
if (require.main === module) main().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
