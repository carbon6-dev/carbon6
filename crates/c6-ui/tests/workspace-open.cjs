#!/usr/bin/env node
'use strict';

// Production WASM, private loopback HTTP/WS, real application link handlers.
// ROOT=/path/to/final/dist ENGINE=both node crates/c6-ui/tests/workspace-open.cjs
// No UI build here: the combined-change build owner supplies ROOT.
const assert = require('node:assert/strict');
const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');
const playwright = require(process.env.PLAYWRIGHT_CORE || '/home/zheilbron/.bun/install/cache/playwright-core@1.50.1@@@1');
const KEY = 'c6.home.workspace.v2';
const DOC = '00000000-0000-0000-0000-000000000042';
const worker = { id: '00000000-0000-0000-0000-000000000000', name: 'Workspace fixture', hostname: 'fixture',
  os: 'linux', arch: 'x86_64', version: 'test', status: 'online', enrolled_at: 1, last_seen: 1 };
const session = { id: 'parent', realm: 'home', worker_id: worker.id, title: 'Workspace parent', cwd: '/tmp',
  connection_id: 'test', model: 'test', status: 'idle', error: null, effort: null, created_at: 1, updated_at: 1,
  last_seq: 70, awaiting_approval: false, parent_id: null, parent_call_id: null, end_reason: null };
const secondary = { ...session, id: 'secondary', title: 'Second document writer' };
const block = (id, text) => ({ id, kind: 'paragraph', text, attrs: {}, hash: `hash-${id}`, by: 'you', rev: 1 });
const doc = { id: DOC, realm: 'home', kind: 'doc', title: 'Workspace document', labels: [], rev: 1,
  created_by: 'session:parent', created_at: 1, updated_by: 'you', updated_at: 1, pending: 0,
  tabs: [{ id: 'tab.1', title: 'First tab', blocks: [
    block('block.1', `Editable source. A plain mention: doc:${DOC}`),
    ...Array.from({ length: 18 }, (_, i) => block(`filler.${i}`, `Paragraph ${i}. ${'Readable document text. '.repeat(12)}`)),
    block('block.2', `[Return to first block](/docs/${DOC}#block.1)`),
  ] }, { id: 'tab.2', title: 'Second tab', blocks: [block('block.3', 'A target on another tab.')] }] };
const tile = (key, columns) => ({ key, columns, cursor: columns.length - 1, active: columns.at(-1).key,
  used: key, back: [], forward: [], stack: null, minimized: false });
const layout = () => ({ tiles: [tile(2, [{ key: 3, view: { view: 'session', id: 'parent' } }]),
  tile(4, [{ key: 5, view: { view: 'docs' } }, { key: 6, view: { view: 'docs', id: DOC } }])],
  focus: 1, next: 7, full: false });
const snapshot = page => page.evaluate(key => JSON.parse(localStorage.getItem(key)), KEY);
async function waitView(page, view, target, count) {
  try {
    await page.waitForFunction(({ key, view, target, count }) => {
      const l = JSON.parse(localStorage.getItem(key)), t = l?.tiles[l.focus];
      const c = t?.columns.find(c => c.key === t.active);
      return c?.view.view === view && (target === undefined || JSON.stringify(c.view.target) === JSON.stringify(target))
        && (count === undefined || l.tiles.length === count) && !t.minimized && t.columns[t.cursor].key === t.active;
    }, { key: KEY, view, target, count }, { timeout: Number(process.env.C6_TEST_DEADLINE_SECS || 300) * 1000 });
  } catch (error) {
    console.error('WAIT VIEW DIAGNOSTICS', JSON.stringify(await page.evaluate(key => ({
      url: location.href, layout: JSON.parse(localStorage.getItem(key)),
      panes: Array.from(document.querySelectorAll('[data-pane-motion]'), el => el.getAttribute('data-pane-motion')),
    }), KEY)));
    throw error;
  }
  await settle(page);
}
async function check(browser, origin, viewport, label) {
  console.log(`RUN workspace-open ${label}`);
  let f;
  try {
    f = await fixture(browser, origin, viewport, 'light', { choice: 'off', os: 'no-preference', effective: 'off' }, value => { f = value; });
    const { page, context } = f, lives = new Set(), liveWaiters = new Set();
    const phase = name => { f.phase = name; console.log(`  ${label}: ${name}`); };
    page.on('pageerror', error => console.error(`  ERROR during ${f.phase}: ${error.message}`));
    page.on('console', message => {
      if (message.type() === 'error') console.error(`  CONSOLE during ${f.phase}: ${message.text()}`);
    });
    await context.routeWebSocket(/\/live(?:\?.*)?$/, socket => {
      lives.add(socket); socket.onClose(() => lives.delete(socket));
      socket.send(JSON.stringify({ type: 'sessions', sessions: [session, secondary] }));
      socket.send(JSON.stringify({ type: 'workers', workers: [worker] }));
      for (const ready of liveWaiters) ready();
      liveWaiters.clear();
    });
    await context.route('**/api/**', async route => {
      const p = new URL(route.request().url()).pathname;
      if (p.endsWith('/sessions/secondary')) return route.fulfill({ json: {
        session: secondary, worker_name: worker.name, worker_online: true, queued: [] } });
      if (p.endsWith('/sessions/secondary/suggestion') || p.endsWith('/sessions/secondary/catchup')) return route.fulfill({ status: 204 });
      if (p.endsWith('/sessions/secondary/view')) return route.fulfill({ json: {} });
      if (p.endsWith(`/docs/${DOC}`)) return route.fulfill({ json: doc });
      if (p.endsWith('/docs')) return route.fulfill({ json: [{ ...doc, tabs: undefined }] });
      if (p.endsWith(`/docs/${DOC}/ops`)) return route.fulfill({ json: { id: DOC, rev: 1, ids: [], keys: {}, hashes: {} } });
      return route.fallback();
    });
    const seed = async (saved, path) => {
      // A same-URL/hash goto can be same-document navigation: WASM would
      // never restore the seeded layout. Stop the old app before seeding,
      // also preventing its reactive storage writer from racing this write.
      await page.goto(origin + '/__seed', { waitUntil: 'domcontentloaded' });
      await page.evaluate(({ key, saved }) => localStorage.setItem(key, JSON.stringify(saved)), { key: KEY, saved });
      await page.goto(origin + '/r/home' + path, { waitUntil: 'domcontentloaded' });
      // DOMContentLoaded precedes WASM's keyboard handlers. A shortcut sent
      // here too early is lost; desktop eagerly mounts both sessions and
      // hid that fixture race, while a phone waits forever for the second.
      await page.locator('[data-workspace-motion]').waitFor({ state: 'attached' });
    };
    const parent = () => page.locator('#col-3');
    const documentPane = () => page.locator('#col-6');
    const links = async () => {
      await page.keyboard.press('Control+1');
      await parent().locator('textarea').waitFor();
      await parent().locator('[id="parent-e20"]').waitFor({ state: 'attached' });
      f.send({ type: 'assistant', turn: 80, blocks: [{ type: 'text', text:
        `[Document block](/docs/${DOC}#block.2) [Other tab](doc:${DOC}?block=block.3) ` +
        `doc:${DOC} [Session turn](/sessions/parent#e20) [Inbox](/inbox) [Sessions](/)` }], stop_reason: 'end_turn' });
      await parent().locator('a[href$="#block.2"]').last().waitFor({ state: 'attached' });
    };
    const clickBlock = () => parent().locator('a[href$="#block.2"]').last().click();

    if (process.env.ONLY !== 'sessionTarget') {
      phase('two sessions create and repeatedly edit one document');
      const writers = { tiles: [tile(2, [{ key: 3, view: { view: 'session', id: 'parent' } }]),
        tile(8, [{ key: 9, view: { view: 'session', id: 'secondary' } }])], focus: 0, next: 10, full: false };
      await seed(writers, '/sessions/parent');
      await parent().locator('[id="parent-e20"]').waitFor({ state: 'attached' });
      // Mount both sessions even on phones, whose other tiles load lazily.
      await page.keyboard.press('Control+2');
      await page.locator('#col-9 [id="secondary-e20"]').waitFor({ state: 'attached' });
      await page.keyboard.press('Control+1');
      await parent().locator('[id="parent-e20"]').waitFor({ state: 'attached' });
      await within('live socket ready', () => lives.size ? Promise.resolve() : new Promise(resolve => liveWaiters.add(resolve)));
      let rev = 1;
      const write = async (by, what) => {
        // Wait for the actual document refresh, not a timer that could let
        // an unhandled/late changed event falsely pass the focus assertion.
        const refreshed = page.waitForResponse(response =>
          new URL(response.url()).pathname.endsWith(`/docs/${DOC}`) && response.request().method() === 'GET');
        for (const socket of lives) socket.send(JSON.stringify({ type: 'doc', change: {
          id: DOC, realm: 'home', rev: ++rev, by: `session:${by}`, what } }));
        await refreshed;
        await settle(page);
      };
      await write('parent', 'created');
      await waitView(page, 'docs', undefined, 3);
      const opened = await snapshot(page), docTile = opened.tiles[opened.focus], docKey = docTile.columns[0].key;
      const autoDoc = page.locator(`#col-${docKey}`);
      await autoDoc.locator('[data-block="block.1"] p').first().dblclick({ position: { x: 4, y: 4 } });
      const draft = autoDoc.locator('[data-block="block.1"] textarea');
      await draft.fill('An unsaved draft survives a second session editing.');
      await draft.evaluate(el => { window.__autoDocEditor = el; });
      await write('secondary', 'changed');
      assert(await draft.evaluate(el => el === window.__autoDocEditor));
      assert.equal(await draft.inputValue(), 'An unsaved draft survives a second session editing.');
      await draft.press('Escape');
      await autoDoc.locator('[data-block="block.1"]').evaluate(el => { window.__autoDocNode = el; });
      for (const by of ['parent', 'parent', 'secondary', 'secondary']) {
        await page.keyboard.press(by === 'parent' ? 'Control+1' : 'Control+2');
        const before = await snapshot(page);
        await write(by, 'changed');
        assert.deepEqual(await snapshot(page), before, `${by}'s streaming edit must refresh, not open/refocus or rewrite history`);
        assert(await autoDoc.locator('[data-block="block.1"]').evaluate(el => el === window.__autoDocNode));
      }
      // Repeated creation notifications use OwnTile and find the existing
      // pane, even when the notification comes from the other session.
      for (const by of ['parent', 'secondary', 'parent']) {
        await page.keyboard.press('Control+1');
        await write(by, 'created');
        await waitView(page, 'docs', undefined, 3);
        const after = await snapshot(page);
        assert.equal(after.tiles[after.focus].key, docTile.key);
        assert.deepEqual(after.tiles.map(t => t.columns.map(c => c.key)), opened.tiles.map(t => t.columns.map(c => c.key)));
        assert.deepEqual(after.tiles.map(t => [t.back, t.forward]), opened.tiles.map(t => [t.back, t.forward]));
      }
      assert.deepEqual(f.errors, [], 'two-session document creation/edit refresh must be panic-free');
      if (process.env.ONLY === 'autoDocs') { console.log(`PASS workspace-open ${label} autoDocs`); return; }
      await seed({ tiles: [tile(2, [{ key: 3, view: { view: 'session', id: 'parent' } }])], focus: 0, next: 4, full: false }, '/sessions/parent');
      await parent().locator('textarea').waitFor();
    }

    // Isolate session replacement before mounting any Docs. The old unkeyed
    // content boundary tried to rebuild live attributes from a disposed
    // SessionView owner (StoredValue panics, then Tachys class::rebuild).
    phase('session-only query target before mounting Docs');
    const sessionTarget = { q: 'needle', anchor: 'e20', open: [], at: null };
    const addSessionTarget = async () => {
      // A new targeted owner opens a new stream. Wait for its replay before
      // sending a new link, not just the already-mounted composer shell.
      await parent().locator('[id="parent-e20"]').waitFor({ state: 'attached' });
      f.send({ type: 'assistant', turn: 81, blocks: [{ type: 'text', text: '[Target session turn](/sessions/parent?q=needle#e20)' }], stop_reason: 'end_turn' });
      await parent().locator('a[href*="?q=needle"]').last().waitFor();
    };
    await addSessionTarget();
    await parent().locator('a[href*="?q=needle"]').last().click();
    await waitView(page, 'session', sessionTarget, 1);
    assert.deepEqual(f.errors, [], 'session-only target must replace its old reactive owner without a panic');
    phase('session target Back and Forward');
    await page.keyboard.press('Control+[');
    await waitView(page, 'session', undefined, 1);
    assert.equal((await snapshot(page)).tiles[0].columns[0].view.target, null);
    await page.keyboard.press('Control+]');
    await waitView(page, 'session', sessionTarget, 1);
    phase('repeat identical session target');
    await addSessionTarget();
    const sessionHistory = (await snapshot(page)).tiles[0].back;
    await parent().locator('a[href*="?q=needle"]').last().click();
    await waitView(page, 'session', sessionTarget, 1);
    assert.deepEqual((await snapshot(page)).tiles[0].back, sessionHistory);
    assert.deepEqual(f.errors, [], 'session target history and repeated visits must not rebuild disposed attributes');
    phase('repeat session target while tile Actions is open');
    await addSessionTarget();
    const actions = page.locator('[data-tile-motion="2"]').getByRole('button', { name: 'Actions', exact: true });
    await actions.click();
    await page.getByRole('button', { name: /Session details…/ }).waitFor();
    phase('navigate from the open Actions menu');
    await parent().locator('a[href*="?q=needle"]').last().dispatchEvent('click');
    await waitView(page, 'session', sessionTarget, 1);
    assert.deepEqual(f.errors, [], 'header controls and an open Actions menu must survive session owner replacement');
    if (process.env.ONLY === 'sessionTarget') { console.log(`PASS workspace-open ${label} sessionTarget`); return; }

    // A doc already in a right pane: doc links, plain mentions, intra-doc
    // links and Back/Forward all reuse that pane and its DOM/editor state.
    phase('mount existing doc and session');
    await seed(layout(), `/docs/${DOC}`);
    await documentPane().locator('[data-block="block.1"]').waitFor();
    await page.evaluate(() => { window.__docNode = document.querySelector('#col-6 [data-block="block.1"]'); });
    await links();
    phase('doc target from session');
    await clickBlock();
    await waitView(page, 'docs', 'block.2', 2);
    assert.equal((await snapshot(page)).tiles[1].active, 6);
    assert(await page.evaluate(() => window.__docNode === document.querySelector('#col-6 [data-block="block.1"]')), 'target retains doc blocks');
    await documentPane().locator('[data-block="block.2"].c6-flash').waitFor();
    // Same-target visits are commands, not just equal View values: after
    // scrolling away, opening it again must reveal it without adding history.
    phase('repeat same doc target');
    const sameHistory = (await snapshot(page)).tiles[1].back;
    await documentPane().locator('[data-block="block.2"]').evaluate(el => {
      el.classList.remove('c6-flash');
      for (let p = el.parentElement; p; p = p.parentElement) {
        if (p.scrollHeight > p.clientHeight) { p.scrollTop = 0; break; }
      }
    });
    await parent().locator('a[href$="#block.2"]').last().dispatchEvent('click');
    await documentPane().locator('[data-block="block.2"].c6-flash').waitFor();
    assert.deepEqual((await snapshot(page)).tiles[1].back, sameHistory);
    phase('plain doc mention');
    const beforePlain = await snapshot(page);
    await links();
    await parent().locator(`a[href$="/docs/${DOC}"]`).last().click();
    await waitView(page, 'docs', 'block.2', 2);
    assert.deepEqual((await snapshot(page)).tiles[1].back, beforePlain.tiles[1].back, 'plain mention only focuses; no history churn');
    phase('doc target back and forward');
    await page.keyboard.press('Control+[');
    await waitView(page, 'docs', undefined, 2);
    assert.equal((await snapshot(page)).tiles[1].columns[1].view.target, undefined);
    await page.keyboard.press('Control+]');
    await waitView(page, 'docs', 'block.2', 2);
    phase('targeted link inside doc');
    await documentPane().locator('[data-block="block.2"] a').click();
    await waitView(page, 'docs', 'block.1', 2); // data-ref must not swallow href's hash

    phase('open block editor');
    await documentPane().locator('[data-block="block.1"] p').first().dblclick({ position: { x: 4, y: 4 } });
    const editor = documentPane().locator('[data-block="block.1"] textarea');
    await editor.fill('An unsaved retained block draft.');
    await editor.evaluate(el => { window.__docEditor = el; });
    // Dispatch the real link click handler without pointer focus/blur: blur
    // intentionally saves/closes an editor and is not a retention test.
    phase('target while block editor retained');
    await parent().locator('a[href$="#block.2"]').last().dispatchEvent('click');
    await waitView(page, 'docs', 'block.2', 2);
    assert(await editor.evaluate(el => el === window.__docEditor), 'target must not remount DocPage/editor');
    assert.equal(await editor.inputValue(), 'An unsaved retained block draft.');
    await page.keyboard.press('Control+[');
    await waitView(page, 'docs', 'block.1', 2);
    await page.keyboard.press('Control+]');
    await waitView(page, 'docs', 'block.2', 2);
    assert(await editor.evaluate(el => el === window.__docEditor), 'target history retains editor identity');
    phase('save block editor and navigate another tab');
    await editor.press('Escape');
    await links();
    await parent().locator('a[href*="?block=block.3"]').last().click();
    await waitView(page, 'docs', 'block.3', 2);
    await documentPane().locator('[data-block="block.3"].c6-flash').waitFor();
    assert.deepEqual(f.errors, [], 'doc target, draft-retention and history phases must be panic-free');

    // An incoming document-created event follows exactly the same open
    // path and must focus the existing doc instead of making another tile.
    phase('incoming session-created document');
    await links();
    await within('live socket ready', () => lives.size ? Promise.resolve() : new Promise(resolve => liveWaiters.add(resolve)));
    for (const socket of lives) socket.send(JSON.stringify({ type: 'doc', change: {
      id: DOC, realm: 'home', rev: 2, by: 'session:parent', what: 'created' } }));
    await waitView(page, 'docs', 'block.3', 2);

    // An ordinary session-turn link navigates its already-open transcript.
    phase('session turn target remount');
    await parent().locator('a[href$="#e20"]').last().dispatchEvent('click');
    await waitView(page, 'session', { q: '', anchor: 'e20', open: [], at: null }, 2);
    assert.equal((await snapshot(page)).tiles[0].active, 3);
    await page.locator('[id="parent-e20"]').waitFor();
    assert.deepEqual(f.errors, [], 'session target replacement after Docs must be panic-free');

    // Reveal a minimized doc while a different tile is maximized.
    phase('reveal minimized doc under maximized session');
    const hidden = layout(); hidden.tiles[1].minimized = true; hidden.focus = 0; hidden.full = true;
    await seed(hidden, '/sessions/parent');
    await links(); await clickBlock();
    await waitView(page, 'docs', 'block.2', 2);
    await documentPane().locator('[data-block="block.2"]').waitFor();
    assert.equal((await snapshot(page)).focus, 1);

    // Modifier-click makes a live copy. Restore now collapses doc copies,
    // but keeps session copies and a duplicate doc's companion pane.
    phase('explicit document copy');
    await links();
    await parent().locator('a[href$="#block.2"]').last().click({ modifiers: ['Control'] });
    await waitView(page, 'docs', 'block.2', 3);
    phase('explicit session copy');
    await links();
    await parent().locator('a[href$="#e20"]').last().click({ modifiers: ['Control'] });
    await waitView(page, 'session', { q: '', anchor: 'e20', open: [], at: null }, 4);
    await page.locator('[id="parent-e20"]').last().waitFor();
    const copies = await snapshot(page), keys = copies.tiles.map(t => t.key);
    phase('restore copies with document deep link');
    await page.goto(origin + `/r/home/docs/${DOC}#block.1`, { waitUntil: 'domcontentloaded' });
    await waitView(page, 'docs', 'block.1', 3);
    const restored = await snapshot(page);
    assert.deepEqual(restored.tiles.map(t => t.key), keys.filter((_, i) => i !== 2), 'first doc copy survives; empty-history duplicate-only tile disappears');
    assert.equal(restored.tiles.flatMap(t => t.columns).filter(c => c.view.view === 'session' && c.view.id === 'parent').length, 2, 'session copies are not collapsed');
    phase('restore keeps focused doc copy and the losing copy companion');
    copies.focus = 2;
    await seed(copies, `/docs/${DOC}#block.1`);
    await waitView(page, 'docs', 'block.1', 4);
    const focusedCopy = await snapshot(page);
    assert.equal(focusedCopy.tiles[focusedCopy.focus].key, keys[2]);
    assert.deepEqual(focusedCopy.tiles[1].columns.map(c => c.key), [5], 'Docs list companion stays in its original tile');
    assert.deepEqual(focusedCopy.tiles[1].back, copies.tiles[1].back);
    assert.deepEqual(focusedCopy.tiles[1].forward, copies.tiles[1].forward);
    assert.equal(focusedCopy.tiles.flatMap(t => t.columns).filter(c => c.view.view === 'docs' && c.view.id === DOC).length, 1);
    phase('restore retains duplicate-only history without recreating a copy on Back');
    const history = layout(), loser = tile(10, [{ key: 11, view: { view: 'docs', id: DOC, target: 'block.2' } }]);
    loser.back = [[{ key: 11, view: { view: 'docs', id: DOC, target: 'block.1' } }]];
    loser.forward = [[{ key: 11, view: { view: 'docs', id: DOC, target: 'block.3' } }]];
    history.tiles.push(loser); history.next = 12;
    await seed(history, `/docs/${DOC}`);
    await waitView(page, 'docs', undefined, 3);
    await page.keyboard.press('Control+3');
    const shell = (await snapshot(page)).tiles[2];
    assert.notEqual(shell.columns[0].view.view, 'docs');
    assert.deepEqual(shell.back, loser.back);
    assert.deepEqual(shell.forward, [...loser.forward, loser.columns]);
    await page.keyboard.press('Control+[');
    await waitView(page, 'docs', 'block.1', 3);
    const revisited = await snapshot(page);
    assert.deepEqual(revisited.tiles[2], shell, 'Back reuses the existing doc and leaves its source history intact');
    assert.equal(revisited.tiles.flatMap(t => t.columns).filter(c => c.view.view === 'docs' && c.view.id === DOC).length, 1);
    assert.deepEqual(f.errors, [], `${label}: browser errors`);
    console.log(`PASS workspace-open ${label}`);
  } finally { await f?.context?.close(); }
}

async function main() {
  const { server, origin } = await serve();
  try {
    const engines = process.env.ENGINE === 'both' ? ['chromium', 'webkit'] : (process.env.ENGINE || 'chromium').split(',');
    for (const engine of engines) {
      const browser = await launchBrowser(playwright, engine);
      try {
        for (const [name, viewport] of [['desktop', { width: 1900, height: 1000 }], ['narrow', { width: 320, height: 568 }]]) {
          if (process.env.VIEWPORT && process.env.VIEWPORT !== name) continue;
          await within(`workspace-open ${engine}/${name}`, () => check(browser, origin, viewport, `${engine}/${name}`));
        }
      } finally { await browser.close(); }
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
}
if (require.main === module) main().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
