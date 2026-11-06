#!/usr/bin/env node
'use strict';

// Production WASM and CSS, private loopback head, no credentials or npm install.
// See docs/testing.md. Like motion.cjs, deadlines catch hangs, not slow machines.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');
const playwright = require(process.env.PLAYWRIGHT_CORE || '/home/zheilbron/.bun/install/cache/playwright-core@1.50.1@@@1');
const ROOT = path.resolve(process.env.ROOT || path.join(__dirname, '../dist'));
const SHOTS = process.env.SCREENSHOTS || '/tmp/z2-screenshots';
const DEADLINE = Number(process.env.C6_TEST_DEADLINE_SECS || 300) * 1000;
const facets = ['worker', 'work', 'project', 'realm', 'repo', 'model'];
const workers = [0, 1].map(n => ({ id: `00000000-0000-0000-0000-00000000000${n}`,
  name: n ? 'Second worker' : 'Shared worker', hostname: 'fixture', os: 'linux', arch: 'x86_64',
  version: 'test', status: 'online', enrolled_at: 1, last_seen: 1 }));
const sessions = ['a', 'b', 'c'].map((id, i) => ({ id, realm: 'home', worker_id: workers[i === 2 ? 1 : 0].id,
  title: `A long session title on a dense wall ${id.toUpperCase()}`, cwd: '/tmp', connection_id: 'test',
  model: i === 2 ? 'other-model' : 'shared-model', status: 'idle', error: null, effort: null,
  colour: ['sky', 'amber', 'violet'][i], auto_labels: [`repo:project-${id}`],
  work: { id: `work-${id}`, title: `Work ${id}`, project: i === 2 ? 'y' : 'x', project_title: i === 2 ? 'Other project' : 'Shared project' },
  created_at: 1, updated_at: 1, last_seq: 2, parent_id: null, parent_call_id: null, awaiting_approval: false }));
const layout = { tiles: [...sessions.map((s, i) => ({ key: i * 2 + 2,
  columns: [{ key: i * 2 + 3, view: { view: 'session', id: s.id } }], cursor: 0, active: i * 2 + 3,
  used: i * 2 + 3, back: [], forward: [], stack: 0, minimized: false })),
{ key: 8, columns: [{ key: 9, view: { view: 'settings', section: 'appearance' } }], cursor: 0,
  active: 9, used: 9, back: [], forward: [], stack: 0, minimized: false }], focus: 0, next: 10, full: false };

async function serve() {
  assert(fs.existsSync(path.join(ROOT, 'index.html')), 'Build the UI before running this check');
  const server = http.createServer((req, res) => {
    const pathname = new URL(req.url, 'http://127.0.0.1').pathname;
    const file = path.resolve(ROOT, pathname === '/' || pathname.startsWith('/r/') ? 'index.html' : `.${pathname}`);
    if (!file.startsWith(ROOT + path.sep) || !fs.existsSync(file) || !fs.statSync(file).isFile()) {
      res.writeHead(404); res.end(); return;
    }
    const mime = { '.html': 'text/html', '.js': 'text/javascript', '.css': 'text/css', '.wasm': 'application/wasm' };
    res.writeHead(200, { 'content-type': mime[path.extname(file)] || 'application/octet-stream' });
    fs.createReadStream(file).pipe(res);
  });
  await new Promise(resolve => server.listen(0, '127.0.0.1', resolve));
  return { server, origin: `http://127.0.0.1:${server.address().port}` };
}

async function check(browser, origin, label) {
  console.log(`RUN ${label}`);
  const phase = name => console.log(`  ${label}: ${name}`);
  const context = await browser.newContext({ viewport: { width: 1900, height: 1000 } });
  context.setDefaultTimeout(DEADLINE);
  const page = await context.newPage(), errors = [], writes = [], lives = new Set();
  let saved = { shown: facets }; // The old head has no tile_bars field.
  page.on('pageerror', error => errors.push(error.message));
  page.on('console', message => { if (/panicked at|unreachable|bad stream message|ResizeObserver loop/.test(message.text())) errors.push(message.text()); });
  await context.route('**/*', route => {
    const url = route.request().url();
    if (url.startsWith(origin + '/') || /^(blob|data):/.test(url)) return route.continue();
    errors.push(`External request: ${url}`); return route.abort();
  });
  await context.routeWebSocket('**/*', socket => {
    const url = new URL(socket.url());
    assert.equal(url.host, new URL(origin).host);
    if (url.pathname.endsWith('/live')) {
      lives.add(socket);
      socket.onClose(() => lives.delete(socket));
      socket.send(JSON.stringify({ type: 'sessions', sessions }));
      socket.send(JSON.stringify({ type: 'workers', workers }));
    } else if (url.pathname.endsWith('/stream')) {
      socket.send(JSON.stringify({ type: 'event', event: { seq: 1, ts: 1, type: 'started',
        connection_id: 'test', model: 'shared-model', cwd: '/tmp', system_prompt: '', tools: [], context_window: 128000 } }));
      socket.send(JSON.stringify({ type: 'event', event: { seq: 2, ts: 2, type: 'assistant', turn: 1,
        blocks: [{ type: 'text', text: 'A quiet transcript, ready for the next message.' }], stop_reason: 'end_turn' } }));
      socket.send(JSON.stringify({ type: 'synced' }));
    } else { errors.push(`Unexpected socket: ${url}`); socket.close(); }
  });
  await context.route('**/api/**', async route => {
    const req = route.request(), p = new URL(req.url()).pathname;
    let json = [];
    if (p.endsWith('/auth/status')) json = { authenticated: true, required: false, passkeys_here: false, has_passkeys: false };
    else if (p.endsWith('/elsewhere')) json = [{ id: 'home', name: 'Home', waiting: 0 }];
    else if (p.endsWith('/version')) json = { version: 'tile-marks-test' };
    else if (p.endsWith('/settings/session_facets')) {
      if (req.method() === 'PUT') { saved = req.postDataJSON(); writes.push(saved); return route.fulfill({ status: 204 }); }
      json = saved;
    } else if (p.endsWith('/settings/transcript')) json = { show_thinking: false };
    else if (p.endsWith('/settings/session_bar')) json = { colour: 'tinted' };
    else if (p.endsWith('/attention')) json = { waiting: [], worth: [], elsewhere: [] };
    else if (p.endsWith('/connections')) json = [{ id: 'test', name: 'Test', kind: 'openai_compat',
      base_url: 'http://127.0.0.1', default_model: 'shared-model', has_api_key: true, created_at: 1 }];
    else if (p.endsWith('/marks')) {
      if (req.method() === 'PUT') { writes.push(req.postDataJSON()); return route.fulfill({ status: 204 }); }
      json = {};
    } else if (/\/sessions\/[abc]$/.test(p)) {
      const s = sessions.find(s => p.endsWith('/' + s.id));
      json = { session: s, worker_name: workers[s.id === 'c' ? 1 : 0].name, worker_online: true, queued: [] };
    } else if (p.endsWith('/suggestion') || p.endsWith('/catchup')) return route.fulfill({ status: 204 });
    else if (p.endsWith('/view')) json = { catchup: false, folded_turns: false, saved_through: 0 };
    else if (p.endsWith('/git/status')) json = { branch: 'fixture', files: [], clean: true };
    else if (p.endsWith('/ui/error')) { errors.push(req.postData()); json = {}; }
    await route.fulfill({ json });
  });
  await page.route('**/__seed', route => route.fulfill({ contentType: 'text/html', body: '<!doctype html><title>Seed</title>' }));
  try {
    await page.goto(origin + '/__seed');
    await page.evaluate(({ layout, theme }) => {
      localStorage.setItem('c6.home.workspace.v2', JSON.stringify(layout));
      localStorage.setItem('c6.theme', theme);
      localStorage.setItem('c6.motion', 'off');
    }, { layout, theme: label.endsWith('dark') ? 'dark' : 'light' });
    await page.goto(origin + '/r/home/sessions/a');
    const toggle = page.getByRole('switch', { name: 'Show on tile bars', exact: true });
    const change = async locator => {
      const saved = page.waitForResponse(r => r.url().includes('/settings/session_facets') && r.request().method() === 'PUT');
      await locator.click();
      assert.equal((await saved).status(), 204, 'assert the setting only after its save finishes');
    };
    phase('initial hydration');
    await toggle.waitFor();
    await page.locator('[data-tile-motion="6"] textarea').waitFor();
    assert.equal(await toggle.getAttribute('aria-checked'), 'false');
    assert.equal(await page.locator('[data-aspect-strip]').count(), 0, 'old head defaults off');
    assert.equal(await page.locator('[data-aspect-dim]').count(), 0, 'no per-tile spotlight work off');
    const panel = key => page.locator(`[data-tile-motion="${key}"] > section`);
    const first = panel(2), title = first.locator('span[title*="Double-click"]');
    const control = first.getByRole('button', { name: 'Close this tile', exact: true });
    const composer = await first.locator('textarea').elementHandle(), close = await control.elementHandle();
    const scrollHome = () => page.locator('[data-workspace-motion]').evaluate(el => { el.parentElement.scrollLeft = 0; });
    const shot = async name => { await scrollHome(); await page.screenshot({ path: path.join(SHOTS, `${label}-${name}.png`) }); };
    await scrollHome();
    const before = { title: await title.boundingBox(), close: await control.boundingBox() };
    await shot('off');
    phase('enable');
    await change(toggle);
    await first.locator('[data-aspect-strip] button').first().waitFor();
    await page.waitForFunction(() => document.querySelectorAll('[data-aspect-strip] button').length === 15);
    await scrollHome();
    assert.equal(saved.tile_bars, true);
    assert.deepEqual(saved.shown, facets);
    const after = { title: await title.boundingBox(), close: await control.boundingBox() };
    assert.equal(after.close.width, 44); assert.equal(after.close.height, 44);
    assert.deepEqual(after.close, before.close, 'marks must not move or squeeze controls');
    assert.equal(before.title.width - after.title.width, 80, 'five 16px shapes take only title room');
    for (const key of [2, 4, 6]) {
      assert((await panel(key).boundingBox()).width >= 480, 'the canvas retains its 480px floor');
      const labels = await panel(key).locator('[data-aspect-strip] button').evaluateAll(els => els.map(e => e.getAttribute('aria-label').split(':')[0]));
      assert.deepEqual(labels, ['Worker', 'Work', 'Project', 'Repo', 'Model'], 'chosen aspects, with the home realm omitted');
      for (const name of ['Changes', 'Terminal', 'Pin', 'Focus', 'Actions', 'Close this tile']) {
        const box = await panel(key).getByRole('button', { name, exact: true }).boundingBox();
        assert.equal(box.width, 44); assert.equal(box.height, 44);
      }
      const ring = await panel(key).getByRole('button', { name: /Open session sheet/ }).boundingBox();
      assert.equal(ring.width, 44); assert.equal(ring.height, 44);
    }
    await shot('on');
    const worker = first.getByRole('button', { name: 'Worker: Shared worker', exact: true });
    phase('resting spotlight');
    await worker.hover(); await page.mouse.move(0, 0);
    // Negative assertion: waiting longer can only weaken it (testing.md rule 4).
    await page.waitForTimeout(450);
    assert.equal(await page.locator('[data-aspect-dim="true"]').count(), 0, 'passing over cancels the dwell');
    await worker.hover();
    await page.waitForFunction(() => document.querySelector('[data-tile-motion="6"] > section').dataset.aspectDim === 'true');
    assert.equal(await first.getAttribute('data-aspect-dim'), 'false');
    assert.equal(await panel(4).getAttribute('data-aspect-dim'), 'false', 'a shared worker stays bright');
    assert.equal(await panel(6).evaluate(el => getComputedStyle(el).opacity), '0.3', 'built arbitrary dim class works');
    await shot('spotlight');
    await worker.click();
    const card = page.locator('.c6-pop').filter({ hasText: 'Its mark, wherever it shows' });
    await card.waitFor();
    assert.match(await card.innerText(), /1 other session/);
    const extras = Array.from({ length: 8 }, (_, i) => ({ ...sessions[0], id: `extra-${i}`, title: `Related session ${i}` }));
    for (const live of lives) live.send(JSON.stringify({ type: 'sessions', sessions: [...sessions, ...extras] }));
    await card.getByText('9 other sessions', { exact: true }).waitFor();
    assert.equal(await card.getByText('+1', { exact: true }).count(), 1, 'eight rows plus the one undisplayed match');
    for (const live of lives) live.send(JSON.stringify({ type: 'sessions', sessions }));
    await card.getByText('1 other session', { exact: true }).waitFor();
    assert.equal(await page.locator('[data-aspect-dim="true"]').count(), 0, 'click clears spotlight');
    assert(await card.getByText('Open', { exact: true }).locator('svg').count(), 'typed destination icon remains');
    await shot('card');
    phase('palette');
    const paletteSaved = page.waitForResponse(r => new URL(r.url()).pathname.endsWith('/marks') && r.request().method() === 'PUT');
    await card.getByRole('button', { name: 'rose', exact: true }).click();
    assert.equal((await paletteSaved).status(), 204);
    phase('palette clicked');
    await card.waitFor({ state: 'detached' });
    assert(writes.some(w => w.key === `worker:${workers[0].id}` && w.mark === 'rose'), 'card saves the shared value palette');
    phase('disable');
    await change(toggle);
    await page.waitForFunction(() => document.querySelectorAll('[data-aspect-strip]').length === 0);
    assert.equal(saved.tile_bars, false);
    assert.equal(await page.locator('[data-aspect-dim]').count(), 0, 'off removes spotlight attributes');
    assert(await close.evaluate(el => el.isConnected), 'toggle retains fixed controls');
    assert(await composer.evaluate(el => el.isConnected), 'toggle retains composers');
    phase('re-enable and reload');
    await change(toggle);
    await first.locator('[data-aspect-strip] button').first().waitFor();
    phase('shown facets preserve the opt-in');
    await change(page.getByRole('switch', { name: 'Worker', exact: true }));
    await page.waitForFunction(() => document.querySelectorAll('[data-aspect-strip] button').length === 12);
    assert.equal(saved.tile_bars, true, 'editing shown facets must not drop tile_bars');
    assert(!saved.shown.includes('worker'));
    await change(page.getByRole('switch', { name: 'Worker', exact: true }));
    await page.waitForFunction(() => document.querySelectorAll('[data-aspect-strip] button').length === 15);
    await page.reload();
    await first.locator('[data-aspect-strip] button').first().waitFor();
    assert.equal(await toggle.getAttribute('aria-checked'), 'true', 'saved opt-in round trips on reload');
    // Keep a card open across the layout change; the feature's owner must
    // remove its body portal, not merely hide its buttons.
    await worker.click();
    await card.waitFor();
    for (const viewport of [{ width: 320, height: 568 }, { width: 390, height: 844 }, { width: 900, height: 480 }]) {
      phase(`phone ${viewport.width}`);
      await page.setViewportSize(viewport);
      await page.waitForFunction(() => document.querySelectorAll('[data-aspect-strip]').length === 0);
      assert.equal(await page.locator('[data-aspect-dim]').count(), 0, 'phone unmounts spotlight');
      assert.equal(await card.count(), 0, 'phone unmounts aspect cards');
      await shot(`phone-${viewport.width}`);
    }
    await page.setViewportSize({ width: 1900, height: 1000 });
    await first.locator('[data-aspect-strip] button').first().waitFor();
    phase('closing a hovered tile clears its spotlight');
    await worker.hover();
    await page.waitForFunction(() => document.querySelector('[data-tile-motion="6"] > section').dataset.aspectDim === 'true');
    // Do not move the pointer first: no mouseleave is sent for removed DOM.
    await control.evaluate(el => el.click());
    await page.waitForFunction(() => !document.querySelector('[data-tile-motion="2"]'));
    await page.waitForFunction(() => document.querySelectorAll('[data-aspect-dim="true"]').length === 0);
    assert.deepEqual(errors, [], 'no browser crashes, disposed signals or external traffic');
    console.log(`PASS ${label}`);
  } catch (error) {
    await page.screenshot({ path: path.join(SHOTS, `${label}-FAIL.png`) }).catch(() => {});
    fs.writeFileSync(path.join(SHOTS, `${label}-FAIL.json`), JSON.stringify({ error: error.stack, errors,
      html: await page.content().catch(() => '') }, null, 2));
    throw error;
  } finally { await context.close(); }
}

async function main() {
  fs.mkdirSync(SHOTS, { recursive: true });
  const { server, origin } = await serve();
  try {
    const engine = process.env.ENGINE || 'chromium';
    assert(['chromium', 'webkit', 'both'].includes(engine));
    for (const name of engine === 'both' ? ['chromium', 'webkit'] : [engine]) {
      const libraries = process.env.CHROMIUM_LIBS || '/tmp/c7-browser-libs/usr/lib/x86_64-linux-gnu';
      const browser = await playwright[name].launch({ headless: true, timeout: DEADLINE,
        executablePath: name === 'webkit' ? (process.env.WEBKIT_EXECUTABLE || '/tmp/c7-webkit.sh') : (process.env.CHROMIUM || '/tmp/c7-chromium/chrome-linux/chrome'),
        ...(name === 'chromium' ? { args: ['--no-sandbox'], env: { ...process.env,
          LD_LIBRARY_PATH: [libraries, process.env.LD_LIBRARY_PATH].filter(Boolean).join(':') } } : {}) });
      try { for (const theme of ['light', 'dark']) await check(browser, origin, `${name}-${theme}`); }
      finally { await browser.close(); }
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
}
main().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
