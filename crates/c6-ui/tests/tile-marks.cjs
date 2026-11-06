#!/usr/bin/env node
'use strict';

// Production WASM and CSS, private loopback head, no credentials or npm install.
// See docs/testing.md. Like motion.cjs, deadlines catch hangs, not slow machines.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const http = require('node:http');
const path = require('node:path');
const { settle } = require('./motion.cjs');
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
    const bar = first.locator(':scope > div[draggable]');
    const control = first.getByRole('button', { name: 'Close this tile', exact: true });
    const composer = await first.locator('textarea').elementHandle(), close = await control.elementHandle();
    const titleNode = await title.elementHandle();
    const fixedControls = await bar.locator('button').elementHandles();
    const fine = await page.evaluate(() => matchMedia('(min-width: 768px) and (pointer: fine)').matches);
    const expectedSlot = width => !fine ? 44 : process.env.BEFORE === '1' ? 28 : width >= 640 ? 36 : width >= 480 ? 32 : 28;
    const initialWidth = await first.evaluate(el => el.clientWidth);
    const slotSize = expectedSlot(initialWidth);
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
    assert.equal(after.close.width, slotSize); assert.equal(after.close.height, slotSize);
    assert.deepEqual(after.close, before.close, 'marks must not move or squeeze controls');
    if (process.env.BEFORE === '1' || process.env.MARKS_BEFORE === '1') {
      assert.equal(before.title.width - after.title.width, 80, 'old five 16px shapes take only title room');
    } else {
      assert(after.title.width <= before.title.width, 'marks may shrink the title, never stretch it');
    }
    for (const key of [2, 4, 6]) {
      const labels = await panel(key).locator('[data-aspect-strip] button').evaluateAll(els => els.map(e => e.getAttribute('aria-label').split(':')[0]));
      assert.deepEqual(labels, ['Worker', 'Work', 'Project', 'Repo', 'Model'], 'chosen aspects, with the home realm omitted');
      for (const name of ['Changes', 'Terminal', 'Pin', 'Focus', 'Actions', 'Close this tile']) {
        const box = await panel(key).getByRole('button', { name, exact: true }).boundingBox();
        assert.equal(box.width, slotSize); assert.equal(box.height, slotSize);
      }
      const ring = await panel(key).getByRole('button', { name: /Open session sheet/ }).boundingBox();
      assert.equal(ring.width, slotSize); assert.equal(ring.height, slotSize);
    }
    await shot('on');
    phase('marks yield before secondary controls in narrow desktop tiles');
    const strip = await first.locator('[data-aspect-strip]').elementHandle();
    const tileRoot = page.locator('[data-tile-motion="2"]');
    // Saving the setting focused its tile. Focus the session before applying
    // fixture widths: clicking Actions must not trigger a workspace layout
    // update that restores the tile's normal width during the assertions.
    await bar.getByRole('button', { name: 'Actions', exact: true }).click();
    await first.locator(':scope > .phone-popover').waitFor();
    await page.keyboard.press('Escape');
    await first.locator(':scope > .phone-popover').waitFor({ state: 'detached' });
    await settle(page);
    const originalWidth = await tileRoot.evaluate(el => el.style.width);
    // Resize the real tile, not the window: phone mode must remain off. The
    // named /tile container queries use the section's content-box width.
    const resizeTile = async width => {
      await tileRoot.evaluate((el, width) => {
        const panel = el.querySelector(':scope > section');
        const style = getComputedStyle(panel);
        const inset = ['paddingLeft', 'paddingRight', 'borderLeftWidth', 'borderRightWidth'].reduce((sum, key) => sum + parseFloat(style[key]), 0);
        const contentWidth = panel.getBoundingClientRect().width - inset;
        el.style.width = `${width + el.getBoundingClientRect().width - contentWidth}px`;
      }, width);
      await page.waitForFunction(width => {
        const panel = document.querySelector('[data-tile-motion="2"] > section');
        const style = getComputedStyle(panel);
        return Math.abs(panel.clientWidth - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight) - width) < 0.5;
      }, width);
      // Container-query styles settle on the rendering opportunity after
      // the geometry read; do not sample the preceding width's visibility.
      await settle(page);
    };
    const barGeometry = () => title.evaluate(el => {
      const bar = el.closest('[draggable]');
      const rect = node => {
        const r = node.getBoundingClientRect();
        return { left: r.left, right: r.right, top: r.top, bottom: r.bottom, width: r.width, height: r.height };
      };
      const controls = [...bar.querySelectorAll('button')].filter(button => button.getClientRects().length).map(button => ({
        label: button.getAttribute('aria-label'), box: rect(button), icon: !!button.querySelector('svg'),
      }));
      const text = document.createRange();
      text.selectNodeContents(el);
      const css = getComputedStyle(el);
      const titleIntrinsic = text.getBoundingClientRect().width + parseFloat(css.paddingLeft) + parseFloat(css.paddingRight);
      return { bar: rect(bar), title: rect(el), titleIntrinsic, controls };
    });
    let wideGeometry;
    for (const width of [960, 800, 720, 640, 639, 560, 480, 479, 400, 320, 240, 720]) {
      await resizeTile(width);
      assert.equal(await first.locator('[data-aspect-strip]').isVisible(), width >= 640, `marks yield below 40rem at ${width}`);
      for (const name of ['Changes', 'Terminal', 'Pin']) {
        assert.equal(await bar.getByRole('button', { name, exact: true, includeHidden: true }).isVisible(), width >= 480,
          `${name} yields only below 30rem at ${width}`);
      }
      const geometry = await barGeometry();
      if (process.env.BEFORE !== '1' && process.env.MARKS_BEFORE !== '1') {
        assert(geometry.title.width <= geometry.titleIntrinsic + 0.5, 'title does not stretch to push marks right');
        if (width === 960) assert(Math.abs(geometry.title.width - geometry.titleIntrinsic) < 0.5, 'wide title keeps its natural text width');
      }
      if (width >= 640 && process.env.BEFORE !== '1' && process.env.MARKS_BEFORE !== '1') {
        const marks = await first.locator('[data-aspect-strip] button').evaluateAll(els => els.map(el => {
          const r = el.getBoundingClientRect(); return { left: r.left, right: r.right, width: r.width };
        }));
        assert.equal(marks[0].left - geometry.title.right, 8, 'marks sit immediately after the intrinsic title');
        for (let i = 1; i < marks.length; i++) {
          assert.equal(marks[i].left - marks[i - 1].right, 8, 'comfortable gaps between marks');
        }
        assert.equal(marks[0].width, 16, 'keep the original mark and icon size');
        const nextControl = geometry.controls.find(c => c.box.left >= marks.at(-1).right);
        assert(nextControl && nextControl.box.left - marks.at(-1).right >= 8, 'marks stay clear of controls');
      }
      await bar.screenshot({ path: path.join(SHOTS, `${label}-bar-${width}.png`) });
      const core = geometry.controls.filter(c => /Open session sheet/.test(c.label) || ['Actions', 'Focus', 'Close this tile'].includes(c.label));
      assert.deepEqual(core.map(c => /Open session sheet/.test(c.label) ? 'Session sheet' : c.label),
        ['Session sheet', 'Actions', 'Focus', 'Close this tile'], `core order at ${width}`);
      assert.deepEqual(geometry.controls.slice(-3).map(c => c.label), ['Actions', 'Focus', 'Close this tile'],
        `Maximize immediately precedes Close at ${width}`);
      for (const c of core) {
        assert.equal(c.box.width, expectedSlot(width)); assert.equal(c.box.height, expectedSlot(width));
        assert(c.icon, `${c.label} keeps its icon at ${width}`);
      }
      assert(geometry.title.width > 0, `title retains room at ${width}`);
      const boxes = [{ label: 'Title', box: geometry.title }, ...geometry.controls];
      for (const [i, a] of boxes.entries()) {
        assert(a.box.left >= geometry.bar.left - 0.5 && a.box.right <= geometry.bar.right + 0.5 &&
          a.box.top >= geometry.bar.top - 0.5 && a.box.bottom <= geometry.bar.bottom + 0.5,
        `${a.label} fits the bar at ${width}`);
        for (const b of boxes.slice(i + 1)) {
          assert(Math.min(a.box.right, b.box.right) - Math.max(a.box.left, b.box.left) <= 0.5,
            `${a.label} and ${b.label} do not overlap at ${width}`);
        }
      }
      const canvasFits = await page.locator('[data-workspace-motion]').evaluate(el =>
        el.getBoundingClientRect().width <= el.parentElement.clientWidth + 1 && el.parentElement.scrollWidth <= el.parentElement.clientWidth + 1);
      assert(canvasFits, `no horizontal canvas overflow at ${width}`);
      assert(await strip.evaluate(el => el.isConnected && el === el.closest('section').querySelector('[data-aspect-strip]')), 'resize retains the existing strip');
      assert(await titleNode.evaluate(el => el.isConnected), 'resize retains the title');
      assert(await composer.evaluate(el => el.isConnected), 'resize retains the composer');
      for (const handle of fixedControls) assert(await handle.evaluate(el => el.isConnected), 'resize retains fixed controls');
      if (width < 480) {
        await bar.getByRole('button', { name: 'Actions', exact: true }).click();
        // Scope to this tile's Actions popover, not unrelated realm menus
        // elsewhere in the wall (including menus inside closed details).
        const menu = first.locator(':scope > .phone-popover');
        await menu.waitFor();
        for (const name of ['Changes', 'Terminal', 'Pin']) {
          const action = menu.getByRole('button', { name, exact: true });
          assert(await action.isVisible(), `${name} remains available in Actions at ${width}`);
          assert(await action.locator('svg').count(), `${name} keeps its menu icon at ${width}`);
        }
        await page.keyboard.press('Escape');
        await menu.waitFor({ state: 'detached' });
      }
      if (width === 720) {
        if (wideGeometry) assert.deepEqual(geometry, wideGeometry, 'widening restores title and control positions');
        else wideGeometry = geometry;
      }
    }
    await tileRoot.evaluate((el, width) => { el.style.width = width; }, originalWidth);
    await first.locator('[data-aspect-strip] button').first().waitFor();
    assert.deepEqual({ title: await title.boundingBox(), close: await control.boundingBox() }, after, 'restoring the tile restores the original bar geometry');
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
    for (const handle of fixedControls) assert(await handle.evaluate(el => el.isConnected), 'toggle retains every bar control');
    assert(await titleNode.evaluate(el => el.isConnected), 'toggle retains the title');
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
