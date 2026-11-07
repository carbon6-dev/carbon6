#!/usr/bin/env node
'use strict';

// Production WASM, private loopback HTTP/WS, real application link handlers.
// ROOT=/path/to/final/dist ENGINE=both node crates/c6-ui/tests/workspace-open.cjs
// No UI build here: the combined-change build owner supplies ROOT.
const assert = require('node:assert/strict');
const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');
const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
const KEY = 'c6.home.workspace.v2';
const DOC = '00000000-0000-0000-0000-000000000042';
const OTHER_DOC = '00000000-0000-0000-0000-000000000043';
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
const liveColumns = saved => saved.tiles.flatMap(t => t.columns);
const sessionColumns = (saved, id = 'parent') => liveColumns(saved).filter(c => c.view.view === 'session' && c.view.id === id);

// Observe the real sockets without replacing the shared fixture's replay or
// send path. /live is the head feed, not a transcript stream. Listener identity
// includes capture, just as EventTarget does; our observer adds no listeners.
function observeSockets() {
  const Native = window.WebSocket;
  const draftListeners = window.__workspaceDraftListeners = [];
  const capture = options => typeof options === 'boolean' ? options : !!options?.capture;
  const addWindow = window.addEventListener.bind(window), removeWindow = window.removeEventListener.bind(window);
  window.addEventListener = (type, callback, options) => {
    if (['pagehide', 'visibilitychange'].includes(type) && callback && !draftListeners.some(l => l.type === type && l.callback === callback && l.capture === capture(options))) {
      draftListeners.push({ type, callback, capture: capture(options) });
    }
    return addWindow(type, callback, options);
  };
  window.removeEventListener = (type, callback, options) => {
    const i = draftListeners.findIndex(l => l.type === type && l.callback === callback && l.capture === capture(options));
    if (i !== -1) draftListeners.splice(i, 1);
    return removeWindow(type, callback, options);
  };
  window.__workspaceSockets = [];
  window.WebSocket = new Proxy(Native, { construct(Target, args) {
    const socket = new Target(...args), listeners = [];
    const record = { socket, listeners, path: new URL(socket.url).pathname };
    window.__workspaceSockets.push(record);
    const add = socket.addEventListener.bind(socket), remove = socket.removeEventListener.bind(socket);
    socket.addEventListener = (type, callback, options) => {
      if (callback && !listeners.some(l => l.type === type && l.callback === callback && l.capture === capture(options))) {
        listeners.push({ type, callback, capture: capture(options) });
      }
      return add(type, callback, options);
    };
    socket.removeEventListener = (type, callback, options) => {
      const i = listeners.findIndex(l => l.type === type && l.callback === callback && l.capture === capture(options));
      if (i !== -1) listeners.splice(i, 1);
      return remove(type, callback, options);
    };
    return socket;
  } });
}
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
async function check(browser, origin, viewport, label, platform) {
  console.log(`RUN workspace-open ${label}`);
  let f;
  try {
    f = await fixture(browser, origin, viewport, 'light', { choice: 'off', os: 'no-preference', effective: 'off' }, value => { f = value; });
    const { page, context } = f, lives = new Set(), liveWaiters = new Set();
    const command = platform.startsWith('Mac') ? 'Meta' : 'Control';
    const press = key => page.keyboard.press(`${command}+${key}`);
    // Subsequent seed navigations install this before WASM starts, so the
    // real platform check, not just synthetic modifier flags, is covered.
    await context.addInitScript(platform => Object.defineProperty(navigator, 'platform', { get: () => platform }), platform);
    await context.addInitScript(observeSockets);
    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.locator('textarea').first().waitFor();
    await settle(page);
    assert.equal(await page.evaluate(() => navigator.platform), platform);
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
      if (p.endsWith(`/docs/${OTHER_DOC}`)) return route.fulfill({ json: { ...doc, id: OTHER_DOC, title: 'Other workspace document' } });
      if (p.endsWith('/docs')) return route.fulfill({ json: [{ ...doc, tabs: undefined }] });
      if (p.endsWith('/search')) return route.fulfill({ json: { results: [], ignored: [], truncated: false } });
      if (p.endsWith(`/docs/${DOC}/ops`)) return route.fulfill({ json: { id: DOC, rev: 1, ids: [], keys: {}, hashes: {} } });
      return route.fallback();
    });
    const seed = async (saved, path, drafts = {}, beforeApp) => {
      // A same-URL/hash goto can be same-document navigation: WASM would
      // never restore the seeded layout. Stop the old app before seeding,
      // also preventing its reactive storage writer from racing this write.
      await page.goto(origin + '/__seed', { waitUntil: 'domcontentloaded' });
      await page.evaluate(({ key, saved, drafts }) => {
        localStorage.setItem(key, JSON.stringify(saved));
        for (const [id, text] of Object.entries(drafts)) localStorage.setItem(`c6.home.draft.${id}`, text);
      }, { key: KEY, saved, drafts });
      await beforeApp?.();
      await page.goto(origin + '/r/home' + path, { waitUntil: 'domcontentloaded' });
      // DOMContentLoaded precedes WASM's keyboard handlers. A shortcut sent
      // here too early is lost; desktop eagerly mounts both sessions and
      // hid that fixture race, while a phone waits forever for the second.
      await page.locator('[data-workspace-motion]').waitFor({ state: 'attached' });
    };
    const parent = () => page.locator('#col-3');
    const documentPane = () => page.locator('#col-6');
    const streamState = () => page.evaluate(() => window.__workspaceSockets.map(({ socket, path, listeners }) => ({
      path, state: socket.readyState, messages: listeners.filter(l => l.type === 'message').length,
    })));
    const oneStream = async id => {
      await page.waitForFunction(id => {
        const streams = window.__workspaceSockets.filter(r => r.path.endsWith(`/sessions/${id}/stream`) && r.socket.readyState <= 1);
        return streams.length === 1 && streams[0].socket.readyState === 1;
      }, id);
      const sockets = await streamState();
      assert.equal(sockets.filter(s => s.path.endsWith(`/sessions/${id}/stream`) && s.state <= 1).length, 1, `${id} has one live transcript stream`);
      assert.equal(sockets.find(s => s.path.endsWith(`/sessions/${id}/stream`) && s.state === 1).messages, 1, `${id} has one stream message listener`);
      assert(sockets.filter(s => s.path.endsWith('/stream') && s.state > 1).every(s => s.messages === 0), 'departed feeds remove their message listeners');
      assert.equal(sockets.filter(s => s.path.endsWith('/live') && s.state <= 1).length, 1, 'the global head feed is separate and singular');
    };
    const links = async () => {
      await press('1');
      await parent().locator('textarea').waitFor();
      await parent().locator('[id="parent-e20"]').waitFor({ state: 'attached' });
      f.send({ type: 'assistant', turn: 80, blocks: [{ type: 'text', text:
        `[Document block](/docs/${DOC}#block.2) [Other tab](doc:${DOC}?block=block.3) ` +
        `doc:${DOC} [Other document](/docs/${OTHER_DOC}) [Workers](/workers) ` +
        `[Session turn](/sessions/parent#e20) [session:parent](session:parent) ` +
        `[Second session](/sessions/secondary) [Inbox](/inbox) [Sessions](/)` }], stop_reason: 'end_turn' });
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
      await press('2');
      await page.locator('#col-9 [id="secondary-e20"]').waitFor({ state: 'attached' });
      await press('1');
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
        await press(by === 'parent' ? '1' : '2');
        const before = await snapshot(page);
        await write(by, 'changed');
        assert.deepEqual(await snapshot(page), before, `${by}'s streaming edit must refresh, not open/refocus or rewrite history`);
        assert(await autoDoc.locator('[data-block="block.1"]').evaluate(el => el === window.__autoDocNode));
      }
      // Repeated creation notifications use OwnTile and find the existing
      // pane, even when the notification comes from the other session.
      for (const by of ['parent', 'secondary', 'parent']) {
        await press('1');
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
    await press('[');
    await waitView(page, 'session', undefined, 1);
    assert.equal((await snapshot(page)).tiles[0].columns[0].view.target, null);
    await press(']');
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
    await press('[');
    await waitView(page, 'docs', undefined, 2);
    assert.equal((await snapshot(page)).tiles[1].columns[1].view.target, undefined);
    await press(']');
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
    const editorHistory = (await snapshot(page)).tiles[1].back;
    await parent().locator('a[href$="#block.2"]').last().dispatchEvent('click', { metaKey: command === 'Meta', ctrlKey: command === 'Control' });
    await waitView(page, 'docs', 'block.2', 2);
    assert(await editor.evaluate(el => el === window.__docEditor), 'modifier reveal must retain the mounted editor too');
    assert.equal(await editor.inputValue(), 'An unsaved retained block draft.');
    assert.deepEqual((await snapshot(page)).tiles[1].back, editorHistory);
    await press('[');
    await waitView(page, 'docs', 'block.1', 2);
    await press(']');
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

    phase('modifier reveals existing document without key or history churn');
    await links();
    const beforeModifier = await snapshot(page);
    await parent().locator('a[href$="#block.2"]').last().click({ modifiers: [command] });
    await waitView(page, 'docs', 'block.2', 2);
    const afterModifier = await snapshot(page);
    assert.deepEqual(afterModifier.tiles.map(t => t.columns.map(c => c.key)), beforeModifier.tiles.map(t => t.columns.map(c => c.key)));
    assert.deepEqual(afterModifier.tiles.map(t => [t.back, t.forward]), beforeModifier.tiles.map(t => [t.back, t.forward]));
    assert.equal(afterModifier.tiles[afterModifier.focus].active, 6);
    assert.equal(afterModifier.tiles.flatMap(t => t.columns).filter(c => c.view.view === 'docs' && c.view.id === DOC).length, 1);
    if (command === 'Meta') {
      phase('Mac Control context-click cannot duplicate document');
      await links();
      const link = parent().locator('a[href$="#block.2"]').last();
      const beforeContext = await snapshot(page);
      await link.dispatchEvent('contextmenu', { button: 2, ctrlKey: true });
      await settle(page);
      assert.deepEqual(await snapshot(page), beforeContext, 'context menu is not an app open');
      await link.dispatchEvent('click', { button: 0, ctrlKey: true });
      await waitView(page, 'docs', 'block.2', 2);
    }

    phase('modifier opens not-yet-open document in new tile, then reuses it');
    await links();
    const sourceBeforeNew = (await snapshot(page)).tiles[0];
    await parent().locator(`a[href$="/docs/${OTHER_DOC}"]`).last().click({ modifiers: [command] });
    await waitView(page, 'docs', undefined, 3);
    const newDoc = await snapshot(page), newDocTile = newDoc.tiles[newDoc.focus];
    assert.equal(newDocTile.columns[0].view.id, OTHER_DOC);
    assert.deepEqual(newDoc.tiles[0].columns, sourceBeforeNew.columns, 'new destination does not replace source panes');
    assert.deepEqual(newDoc.tiles[0].back, sourceBeforeNew.back);
    await links();
    await parent().locator(`a[href$="/docs/${OTHER_DOC}"]`).last().click({ modifiers: [command] });
    await waitView(page, 'docs', undefined, 3);
    const newDocAgain = await snapshot(page);
    assert.equal(newDocAgain.tiles[newDocAgain.focus].key, newDocTile.key);

    phase('modifier opens a new single-copy page, then reuses it');
    await links();
    await parent().locator('a[href$="/workers"]').last().click({ modifiers: [command] });
    await waitView(page, 'workers', undefined, 4);
    const workers = await snapshot(page), workersTile = workers.tiles[workers.focus];
    await links();
    await parent().locator('a[href$="/workers"]').last().click({ modifiers: [command] });
    await waitView(page, 'workers', undefined, 4);
    const workersAgain = await snapshot(page);
    assert.equal(workersAgain.tiles[workersAgain.focus].key, workersTile.key);
    assert.deepEqual(workersAgain.tiles[workersAgain.focus].columns, workersTile.columns);
    assert.deepEqual(workersAgain.tiles[workersAgain.focus].back, workersTile.back);

    phase('session modifier reveals the live transcript and preserves its draft');
    await seed(layout(), '/sessions/parent');
    await links();
    const parentDraft = 'Parent draft survives targeting, dedupe, history and moves.';
    const secondaryDraft = 'A different draft belongs only to the second session.';
    await parent().locator('textarea').fill(parentDraft);
    const beforeSession = await snapshot(page);
    await parent().locator('a[href$="#e20"]').last().click({ modifiers: [command] });
    const turnTarget = { q: '', anchor: 'e20', open: [], at: null };
    await waitView(page, 'session', turnTarget, 2);
    await parent().locator('[id="parent-e20"]').waitFor({ state: 'attached' });
    assert.equal(await parent().locator('textarea').inputValue(), parentDraft);
    assert.deepEqual((await snapshot(page)).tiles.map(t => t.columns.map(c => c.key)), beforeSession.tiles.map(t => t.columns.map(c => c.key)));
    await oneStream('parent');
    await links();
    const sameSession = await snapshot(page);
    // Both a targeted link and a mention use the production Markdown handler.
    // Targeted sessions deliberately remount to revisit the turn. Untargeted
    // mentions only reveal, retaining the composer, stream and reading target.
    for (const selector of ['a[href$="#e20"]', 'a[data-ref="session:parent"]']) {
      const sameSockets = await streamState(), sameRequests = f.requests.length;
      await parent().locator('textarea').evaluate(el => { window.__sessionComposer = el; });
      await parent().locator(selector).last().click({ modifiers: [command] });
      await waitView(page, 'session', turnTarget, 2);
      assert.deepEqual((await snapshot(page)).tiles.map(t => [t.key, t.columns, t.back, t.forward]), sameSession.tiles.map(t => [t.key, t.columns, t.back, t.forward]), 'repeat/untargeted session opens neither copy nor history');
      await parent().locator('[id="parent-e20"]').waitFor({ state: 'attached' });
      assert.equal(await parent().locator('textarea').inputValue(), parentDraft);
      await oneStream('parent');
      if (selector.includes('data-ref')) {
        assert(await parent().locator('textarea').evaluate(el => el === window.__sessionComposer));
        assert.deepEqual(await streamState(), sameSockets, 'untargeted opens do not add sockets or stream listeners');
        assert(!f.requests.slice(sameRequests).some(r => /GET .*\/sessions\/parent$/.test(r)), 'untargeted opens do not fetch another session owner');
      } else {
        assert(await page.evaluate(() => !window.__sessionComposer.isConnected), 'a revisited target disposes its previous composer');
        await links();
      }
    }
    phase('target remount while a composer model popup is open cleans its owner');
    const lifecycleListeners = await page.evaluate(() => window.__workspaceDraftListeners.length);
    await parent().getByRole('button', { name: /^Model: .*change it/ }).click();
    await page.getByPlaceholder('model id', { exact: true }).waitFor();
    await parent().locator('textarea').evaluate(el => { window.__openPopupComposer = el; });
    // No pointer outside the popup: that would dismiss it before teardown and
    // mask a strong-owner reference held by the global popup registry.
    await press('[');
    await waitView(page, 'session', undefined, 2);
    await parent().locator('textarea').waitFor();
    assert(await page.evaluate(() => !window.__openPopupComposer.isConnected));
    await page.getByPlaceholder('model id', { exact: true }).waitFor({ state: 'detached' });
    assert.equal(await parent().locator('textarea').inputValue(), parentDraft);
    await oneStream('parent');
    assert.equal(await page.evaluate(() => window.__workspaceDraftListeners.length), lifecycleListeners, 'open-popup teardown replaces all composer listeners without retaining the old owner');
    await press(']');
    await waitView(page, 'session', turnTarget, 2);
    assert.equal(await parent().locator('textarea').inputValue(), parentDraft);
    await oneStream('parent');

    phase('modifier opens an unopened session in a new tile, then list modifiers reuse it');
    await links();
    const sourceSession = (await snapshot(page)).tiles[0];
    phase('unopened secondary: modifier-click transcript link');
    await parent().locator('a[href$="/sessions/secondary"]').last().click({ modifiers: [command] });
    phase('unopened secondary: wait for focused session in three tiles');
    await waitView(page, 'session', undefined, 3);
    const newSession = await snapshot(page), secondaryTile = newSession.tiles[newSession.focus];
    const secondaryKey = secondaryTile.columns[0].key, secondaryPane = page.locator(`#col-${secondaryKey}`);
    assert.equal(secondaryTile.columns[0].view.id, 'secondary');
    assert.deepEqual(newSession.tiles[0].columns, sourceSession.columns);
    assert.deepEqual(newSession.tiles[0].back, sourceSession.back);
    phase('unopened secondary: wait for replay event secondary-e20');
    await secondaryPane.locator('[id="secondary-e20"]').waitFor({ state: 'attached' });
    phase('unopened secondary: fill and retain composer');
    await secondaryPane.locator('textarea').fill(secondaryDraft);
    await secondaryPane.locator('textarea').evaluate(el => { window.__secondaryComposer = el; });
    phase('unopened secondary: require one stream');
    await oneStream('secondary');
    // Open the real Sessions list from the transcript, not a synthetic router
    // call; it is itself single-copy, on both desktop and phone.
    phase('Sessions list: reinject parent transcript links');
    await links();
    phase('Sessions list: modifier-click root link');
    await parent().locator('a[href$="/"]').last().click({ modifiers: [command] });
    phase('Sessions list: wait for focused Sessions in four tiles');
    await waitView(page, 'sessions', undefined, 4);
    const withList = await snapshot(page), listTile = withList.tiles[withList.focus], listKey = listTile.columns[0].key;
    const listSource = structuredClone(listTile);
    for (const id of ['parent', 'secondary', 'parent', 'secondary']) {
      phase(`Sessions list reuse ${id}: focus list and modifier-click row`);
      // The previous row's hover preview is a portal above the list and
      // can cover the next row. Dismiss it as a user would, not with a
      // forced click that would bypass the real pointer hit test.
      await page.mouse.move(0, 0);
      const previewClose = page.getByRole('button', { name: 'Close session details', exact: true });
      if (await previewClose.isVisible()) await previewClose.click();
      await press('4');
      await page.locator(`#col-${listKey} a[href$="/sessions/${id}"]`).first().click({ modifiers: [command] });
      phase(`Sessions list reuse ${id}: wait for existing session in four tiles`);
      await waitView(page, 'session', id === 'parent' ? turnTarget : undefined, 4);
      const after = await snapshot(page);
      assert.equal(sessionColumns(after, id).length, 1);
      assert.equal(after.tiles[after.focus].key, id === 'parent' ? 2 : secondaryTile.key);
      assert.deepEqual(after.tiles.find(t => t.key === listTile.key).columns, listSource.columns);
      assert.deepEqual(after.tiles.find(t => t.key === listTile.key).back, listSource.back);
      assert.deepEqual(after.tiles.find(t => t.key === listTile.key).forward, listSource.forward);
      assert.equal(await (id === 'parent' ? parent() : secondaryPane).locator('textarea').inputValue(), id === 'parent' ? parentDraft : secondaryDraft);
      phase(`Sessions list reuse ${id}: require one stream`);
      await oneStream(id);
    }
    assert(await secondaryPane.locator('textarea').evaluate(el => el === window.__secondaryComposer));
    if (viewport.width < 768) {
      phase('phone picker lists one row per live session and reveals the retained composer');
      const beforePicker = await snapshot(page);
      await page.getByRole('button', { name: /^Switch sessions and pages,/ }).click();
      const picker = page.getByRole('dialog', { name: 'Sessions', exact: true });
      const parentRow = picker.getByRole('button', { name: /(?:Workspace parent|Motion fixture parent)/ });
      const secondaryRow = picker.getByRole('button', { name: 'Second document writer idle · Workspace fixture', exact: true });
      assert.equal(await parentRow.count(), 1);
      assert.equal(await secondaryRow.count(), 1);
      await parentRow.click();
      await waitView(page, 'session', turnTarget, 4);
      const afterPicker = await snapshot(page);
      assert.deepEqual(afterPicker.tiles.map(t => [t.key, t.columns, t.back, t.forward]), beforePicker.tiles.map(t => [t.key, t.columns, t.back, t.forward]));
      assert.equal(await parent().locator('textarea').inputValue(), parentDraft);
      await oneStream('parent');
    }

    phase('restore keeps the focused session target, companion and independent histories');
    const firstTarget = { q: '', anchor: 'e20', open: [], at: null };
    const focusedTarget = { q: '', anchor: 'e30', open: [], at: null };
    const legacy = { tiles: [tile(2, [{ key: 3, view: { view: 'session', id: 'parent', target: firstTarget } },
      { key: 7, view: { view: 'session_changes', id: 'parent' } }]),
      tile(8, [{ key: 9, view: { view: 'session', id: 'parent', target: focusedTarget } }])], focus: 1, next: 10, full: false };
    legacy.tiles[0].back = [[{ key: 20, view: { view: 'search', q: 'loser-back', kind: '', worker: '' } }]];
    legacy.tiles[0].forward = [[{ key: 21, view: { view: 'search', q: 'loser-forward', kind: '', worker: '' } }]];
    legacy.tiles[1].back = [[{ key: 22, view: { view: 'workers' } }]];
    legacy.next = 23;
    await seed(legacy, '/sessions/parent', { parent: parentDraft, secondary: secondaryDraft });
    await waitView(page, 'session', focusedTarget, 2);
    const focusedSession = await snapshot(page);
    assert.deepEqual(sessionColumns(focusedSession).map(c => c.key), [9]);
    assert.deepEqual(focusedSession.tiles[0].columns.map(c => c.key), [7], 'losing transcript leaves its Changes companion');
    assert.deepEqual(focusedSession.tiles.map(t => [t.back, t.forward]), legacy.tiles.map(t => [t.back, t.forward]));
    await page.locator('#col-9 [id="parent-e30"]').waitFor({ state: 'attached' });
    assert.equal(await page.locator('#col-9 textarea').inputValue(), parentDraft);
    await oneStream('parent');
    assert.equal((await streamState()).filter(s => s.path.endsWith('/sessions/parent/stream')).length, 1, 'restore collapses losers before any duplicate Feed mounts');

    phase('restore keeps the first session if focus is elsewhere; Home and Realms collapse too');
    const historySession = tile(8, [{ key: 9, view: { view: 'session', id: 'parent', target: focusedTarget } }]);
    historySession.back = [[{ key: 9, view: { view: 'session', id: 'parent', target: firstTarget } }]];
    historySession.forward = [[{ key: 9, view: { view: 'session', id: 'parent', target: focusedTarget } }]];
    const legacyHomes = { tiles: [tile(2, [{ key: 3, view: { view: 'session', id: 'parent', target: firstTarget } }]), historySession,
      tile(10, [{ key: 11, view: { view: 'sessions' } }]), tile(12, [{ key: 13, view: { view: 'sessions' } }]),
      tile(14, [{ key: 15, view: { view: 'realms' } }]), tile(16, [{ key: 17, view: { view: 'realms' } }])], focus: 2, next: 18, full: false };
    await seed(legacyHomes, '/');
    await waitView(page, 'sessions', undefined, 4);
    const firstSession = await snapshot(page), historyShell = firstSession.tiles.find(t => t.key === 8);
    assert.deepEqual(sessionColumns(firstSession).map(c => c.key), [3]);
    assert.deepEqual(sessionColumns(firstSession)[0].view.target, firstTarget);
    for (const view of ['sessions', 'realms']) assert.equal(liveColumns(firstSession).filter(c => c.view.view === view).length, 1);
    assert(historyShell, 'duplicate-only tile retains its independent history');
    assert.deepEqual(historyShell.columns, [], 'an already-live Home cannot become a phantom history-shell copy');
    assert.deepEqual(historyShell.back, historySession.back);
    assert.deepEqual(historyShell.forward, [...historySession.forward, historySession.columns]);
    await press('2');
    const shownShell = (await snapshot(page)).tiles.find(t => t.key === 8);
    await press('[');
    await waitView(page, 'session', firstTarget, 4);
    assert.deepEqual((await snapshot(page)).tiles.find(t => t.key === 8), shownShell, 'Back reveals the surviving transcript without consuming source history');
    assert.equal(await parent().locator('textarea').inputValue(), parentDraft);
    await oneStream('parent');

    phase('reload with a focused history-only tile releases initial loading');
    const focusedShell = structuredClone(firstSession);
    focusedShell.focus = focusedShell.tiles.findIndex(t => t.key === historyShell.key);
    let initialGit;
    await seed(focusedShell, '/', {}, () => {
      // Subscribe after the previous app is gone, before this startup's
      // request: a response from an older document cannot satisfy it.
      if (viewport.width >= 768) initialGit = page.waitForResponse(response =>
        new URL(response.url()).pathname.endsWith('/git/status') && response.request().method() === 'GET');
    });
    await page.getByText("This tile's pages are already open elsewhere.", { exact: false }).waitFor();
    if (viewport.width >= 768) {
      // Do not focus another tile: its visible transcript must mount once
      // the empty focused tile releases the initial-load barrier.
      await parent().locator('textarea').waitFor();
      await oneStream('parent');
      // Also require its deferred initial metadata to finish. Otherwise
      // the next full-page seed can unload WebKit before fetch starts.
      const response = await initialGit;
      assert.equal(response.status(), 200, 'the visible transcript starts its initial git metadata');
      assert.equal(await response.finished(), null, 'initial git metadata settles before the next fixture navigation');
    }
    const afterShellReload = await snapshot(page);
    assert.equal(afterShellReload.tiles[afterShellReload.focus].key, historyShell.key);
    assert.deepEqual(afterShellReload.tiles.find(t => t.key === historyShell.key), focusedShell.tiles[focusedShell.focus]);

    for (const view of ['sessions', 'realms']) {
      phase(`restore prefers the focused ${view} page without losing the other tile's companion/history`);
      const pages = { tiles: [tile(2, [{ key: 3, view: { view } }, { key: 7, view: { view: 'workers' } }]),
        tile(8, [{ key: 9, view: { view } }])], focus: 1, next: 10, full: false };
      pages.tiles[0].back = [[{ key: 20, view: { view: 'search', q: `${view}-back`, kind: '', worker: '' } }]];
      pages.tiles[0].forward = [[{ key: 21, view: { view: 'search', q: `${view}-forward`, kind: '', worker: '' } }]];
      pages.next = 22;
      await seed(pages, view === 'sessions' ? '/' : '/realms');
      await waitView(page, view, undefined, 2);
      const restoredPage = await snapshot(page);
      assert.deepEqual(liveColumns(restoredPage).filter(c => c.view.view === view).map(c => c.key), [9]);
      assert.deepEqual(restoredPage.tiles[0].columns.map(c => c.key), [7]);
      assert.deepEqual(restoredPage.tiles.map(t => [t.back, t.forward]), pages.tiles.map(t => [t.back, t.forward]));
    }

    // Seed a legacy document copy; new clicks can no longer create one.
    // Keep a distinct second session, not the formerly intentional live copy.
    const copies = layout();
    copies.tiles.push(tile(10, [{ key: 11, view: { view: 'session', id: 'secondary' } }]));
    copies.tiles.splice(2, 0, tile(20, [{ key: 21, view: { view: 'docs', id: DOC, target: 'block.2' } }]));
    copies.next = 22;
    copies.focus = 0;
    const keys = copies.tiles.map(t => t.key);
    phase('restore copies with document deep link');
    await seed(copies, `/docs/${DOC}#block.1`);
    await waitView(page, 'docs', 'block.1', 3);
    const restored = await snapshot(page);
    assert.deepEqual(restored.tiles.map(t => t.key), keys.filter((_, i) => i !== 2), 'first doc copy survives; empty-history duplicate-only tile disappears');
    assert.equal(sessionColumns(restored).length, 1);
    assert.equal(sessionColumns(restored, 'secondary').length, 1, 'different sessions remain independent');
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
    await press('3');
    const shell = (await snapshot(page)).tiles[2];
    assert.notEqual(shell.columns[0].view.view, 'docs');
    assert.deepEqual(shell.back, loser.back);
    assert.deepEqual(shell.forward, [...loser.forward, loser.columns]);
    await press('[');
    await waitView(page, 'docs', 'block.1', 3);
    const revisited = await snapshot(page);
    assert.deepEqual(revisited.tiles[2], shell, 'Back reuses the existing doc and leaves its source history intact');
    assert.equal(revisited.tiles.flatMap(t => t.columns).filter(c => c.view.view === 'docs' && c.view.id === DOC).length, 1);

    phase('tile Actions detaches a document instead of copying it; Back reveals the moved pane');
    await seed(layout(), `/docs/${DOC}`);
    await documentPane().locator('[data-block="block.1"]').waitFor();
    await page.locator('[data-tile-motion="4"]').getByRole('button', { name: 'Actions', exact: true }).click();
    await page.getByRole('button', { name: 'Into its own tile', exact: true }).click();
    await waitView(page, 'docs', undefined, 3);
    const detached = await snapshot(page), detachedKey = detached.tiles[detached.focus].key;
    assert.equal(detached.tiles.flatMap(t => t.columns).filter(c => c.view.view === 'docs' && c.view.id === DOC).length, 1);
    assert.deepEqual(detached.tiles[1].columns.map(c => c.key), [5]);
    await press('2');
    await waitView(page, 'docs', undefined, 3);
    const detachedSource = (await snapshot(page)).tiles[1];
    await press('[');
    await page.waitForFunction(({ key, tile }) => {
      const l = JSON.parse(localStorage.getItem(key));
      return l?.tiles[l.focus]?.key === tile;
    }, { key: KEY, tile: detachedKey });
    await waitView(page, 'docs', undefined, 3);
    const afterDetachBack = await snapshot(page);
    assert.equal(afterDetachBack.tiles[afterDetachBack.focus].key, detachedKey);
    assert.deepEqual(afterDetachBack.tiles[1], detachedSource, 'the detached source history remains available');

    phase('session detach hands off same-task text and attachments before the draft debounce');
    const withSessionList = { tiles: [tile(2, [{ key: 7, view: { view: 'sessions' } },
      { key: 3, view: { view: 'session', id: 'parent' } }])], focus: 0, next: 8, full: false };
    withSessionList.tiles[0].cursor = 0; withSessionList.tiles[0].active = 7;
    await seed(withSessionList, '/', { parent: parentDraft });
    await page.locator('#col-7 textarea').waitFor();
    await page.locator('#col-7 a[href$="/sessions/parent"]').first().click();
    await parent().locator('[id="parent-e20"]').waitFor({ state: 'attached' });
    await parent().locator('input[type="file"]').setInputFiles({ name: 'q10-draft.txt', mimeType: 'text/plain', buffer: Buffer.from('An unsent session attachment.') });
    await parent().getByText('q10-draft.txt', { exact: true }).waitFor();
    const draftListeners = await page.evaluate(() => window.__workspaceDraftListeners.length);
    const composerListeners = await page.evaluate(() => CSS.supports('field-sizing', 'content') ? 2 : 3);
    await page.locator('[data-tile-motion="2"]').getByRole('button', { name: 'Actions', exact: true }).click();
    await page.getByRole('button', { name: 'Into its own tile', exact: true }).waitFor();
    const freshDraft = 'Typed and detached in one task, before the 250ms text save.';
    await page.evaluate(text => {
      const input = document.querySelector('#col-3 textarea');
      input.value = text;
      input.dispatchEvent(new Event('input', { bubbles: true }));
      if (localStorage.getItem('c6.home.draft.parent') === text) throw new Error('fixture must exercise an unsaved draft');
      [...document.querySelectorAll('button')].find(b => b.textContent.trim() === 'Into its own tile').click();
    }, freshDraft);
    await waitView(page, 'session', undefined, 2);
    const movedSession = await snapshot(page), movedTile = movedSession.tiles[movedSession.focus];
    assert.notEqual(movedTile.columns[0].key, 3, 'detach allocates a fresh session column');
    assert.deepEqual(movedSession.tiles[0].columns.map(c => c.key), [7]);
    assert.equal(sessionColumns(movedSession).length, 1);
    const movedPane = page.locator(`#col-${movedTile.columns[0].key}`);
    await movedPane.locator('textarea').waitFor();
    assert.equal(await movedPane.locator('textarea').inputValue(), freshDraft, 'new composer must receive the fresh draft, not the last debounced storage value');
    await movedPane.getByText('q10-draft.txt', { exact: true }).waitFor();
    await oneStream('parent');
    assert.equal(await page.evaluate(() => window.__workspaceDraftListeners.length), draftListeners, 'detach replaces, rather than leaks, draft lifecycle listeners');
    await press('1');
    const detachedSessionSource = (await snapshot(page)).tiles[0];
    await press('[');
    await waitView(page, 'session', undefined, 2);
    assert.equal((await snapshot(page)).tiles[(await snapshot(page)).focus].key, movedTile.key);
    assert.deepEqual((await snapshot(page)).tiles[0], detachedSessionSource, 'Back reveals the moved session without consuming source history');
    if (viewport.width < 768) {
      await page.getByRole('button', { name: /^Switch sessions and pages,/ }).click();
      const picker = page.getByRole('dialog', { name: 'Sessions', exact: true });
      await picker.locator('button[aria-current="page"]').locator('..').getByRole('button', { name: 'Close this session or page', exact: true }).click();
      // This picker belongs to the removed tile's header, so closing its
      // own row destroys the dialog along with that tile.
      await picker.waitFor({ state: 'detached' });
    } else {
      await page.locator(`[data-tile-motion="${movedTile.key}"]`).getByRole('button', { name: 'Close this tile', exact: true }).click();
    }
    await waitView(page, 'sessions', undefined, 1);
    await page.waitForFunction(() => window.__workspaceSockets.filter(s => s.path.endsWith('/sessions/parent/stream')).every(s => s.socket.readyState > 1 && !s.listeners.some(l => l.type === 'message')));
    assert.equal(sessionColumns(await snapshot(page)).length, 0);
    assert.equal(await page.evaluate(() => window.__workspaceDraftListeners.length), draftListeners - composerListeners, 'closing removes the draft lifecycle and fallback autosize visibility listeners');

    phase('Changes companions keep explicit-copy behavior without copying the transcript');
    await seed(layout(), '/sessions/parent');
    for (const count of [3, 4]) {
      await press('1');
      await page.locator('[data-tile-motion="2"]').getByRole('button', { name: /Open session sheet/ }).click();
      await page.getByRole('button', { name: 'Changes', exact: true }).last().click({ modifiers: [command] });
      await waitView(page, 'session_changes', undefined, count);
      const companions = await snapshot(page);
      assert.equal(liveColumns(companions).filter(c => c.view.view === 'session_changes' && c.view.id === 'parent').length, count - 2);
      assert.equal(sessionColumns(companions).length, 1);
      await oneStream('parent');
    }
    const companionCopies = await snapshot(page);
    await seed(companionCopies, '/sessions/parent/changes');
    await waitView(page, 'session_changes', undefined, 4);
    assert.equal(liveColumns(await snapshot(page)).filter(c => c.view.view === 'session_changes').length, 2, 'restore does not collapse companion copies');

    phase('Back and Forward reveal an existing session target without consuming source history');
    for (const direction of ['[', ']']) {
      const saved = { tiles: [tile(2, [{ key: 3, view: { view: 'session', id: 'secondary' } }]),
        tile(4, [{ key: 5, view: { view: 'session', id: 'parent', target: firstTarget } }])], focus: 0, next: 10, full: false };
      saved.tiles[0][direction === '[' ? 'back' : 'forward'] = [[{ key: 9, view: { view: 'session', id: 'parent', target: focusedTarget } }]];
      await seed(saved, '/sessions/secondary');
      await page.locator('#col-3 textarea').waitFor();
      const source = (await snapshot(page)).tiles[0];
      await press(direction);
      await waitView(page, 'session', focusedTarget, 2);
      const after = await snapshot(page);
      assert.deepEqual(after.tiles[0], source, 'session history collision leaves both source stacks intact');
      assert.deepEqual(sessionColumns(after).map(c => c.key), [5]);
      assert.equal(await page.locator('#col-5 textarea').inputValue(), freshDraft);
      assert.equal(await page.locator('#col-3 textarea').inputValue(), secondaryDraft);
      await oneStream('parent');
    }

    phase('Back and Forward reuse an already-open ordinary page');
    for (const direction of ['[', ']']) {
      const saved = { tiles: [tile(2, [{ key: 3, view: { view: 'session', id: 'parent' } }]),
        tile(4, [{ key: 5, view: { view: 'workers' } }])], focus: 0, next: 10, full: false };
      saved.tiles[0][direction === '[' ? 'back' : 'forward'] = [[{ key: 9, view: { view: 'workers' } }]];
      await seed(saved, '/sessions/parent');
      const source = (await snapshot(page)).tiles[0];
      await press(direction);
      await waitView(page, 'workers', undefined, 2);
      const after = await snapshot(page);
      assert.deepEqual(after.tiles[0], source, 'history collision must not consume either stack or replace the source');
      assert.equal(after.tiles.flatMap(t => t.columns).filter(c => c.view.view === 'workers').length, 1);
    }

    phase('in-place search route reveals an existing destination instead of duplicating it');
    const searches = { tiles: [tile(2, [{ key: 3, view: { view: 'search', q: 'alpha', kind: '', worker: '' } }]),
      tile(4, [{ key: 5, view: { view: 'search', q: 'beta', kind: '', worker: '' } }])], focus: 0, next: 6, full: false };
    await seed(searches, '/search?q=alpha');
    const originalSearch = (await snapshot(page)).tiles[0];
    const query = page.locator('#col-3 input[type="search"]');
    await query.fill('beta');
    await query.press('Enter');
    await page.waitForFunction(key => JSON.parse(localStorage.getItem(key))?.focus === 1, KEY);
    await waitView(page, 'search', undefined, 2);
    const replaced = await snapshot(page);
    assert.equal(replaced.focus, 1);
    assert.equal(replaced.tiles[1].columns[0].view.q, 'beta');
    assert.deepEqual(replaced.tiles[0].columns, originalSearch.columns);
    assert.deepEqual(replaced.tiles[0].back, originalSearch.back);
    assert.deepEqual(replaced.tiles[0].forward, originalSearch.forward);

    phase('Home New tile requests Sessions once, then reveals the same live page');
    await seed(layout(), `/docs/${DOC}`);
    let homeOwner;
    for (let i = 0; i < 3; i++) {
      if (viewport.width < 768) {
        await page.getByRole('button', { name: /^Switch sessions and pages,/ }).click();
        await page.getByRole('dialog', { name: 'Sessions', exact: true }).getByRole('button', { name: /New session or page/ }).click();
      } else {
        await page.getByRole('button', { name: 'Tile', exact: true }).click();
      }
      await waitView(page, 'sessions', undefined, 3);
      const homes = await snapshot(page), owner = homes.tiles[homes.focus];
      assert.equal(liveColumns(homes).filter(c => c.view.view === 'sessions').length, 1);
      if (homeOwner) assert.deepEqual([owner.key, owner.columns, owner.back, owner.forward], homeOwner, 'New tile reuses the existing launcher identity/history');
      homeOwner = [owner.key, owner.columns, owner.back, owner.forward];
      if (i < 2) await press('1');
    }
    const homes = await snapshot(page);
    await seed(homes, '/');
    await waitView(page, 'sessions', undefined, 3);

    if (viewport.width >= 1280) {
      phase('drag/drop moves tile identities without copying any panes');
      // Keep the existing document drag, and also move the session tile.
      // Neither movement may create another Feed or discard a composer draft.
      for (const [from, to] of [[4, 2], [2, 4]]) {
        phase(`drag/drop tile ${from} onto ${to}: preserve keyed panes and live composer`);
        await seed(layout(), `/docs/${DOC}`);
        await parent().locator('textarea').waitFor();
        await parent().locator('textarea').fill(parentDraft);
        await parent().locator('textarea').evaluate(el => { window.__dragComposer = el; });
        const before = await snapshot(page);
        const source = page.locator(`[data-tile-motion="${from}"] [draggable="true"]`);
        const target = page.locator(`[data-tile-motion="${to}"]`);
        // Drive the real HTML drag handlers with a real DataTransfer. Native
        // pointer DnD support differs by engine; identity is the invariant.
        const transfer = await page.evaluateHandle(() => new DataTransfer());
        await source.dispatchEvent('dragstart', { dataTransfer: transfer });
        const rect = await target.boundingBox();
        const drop = { dataTransfer: transfer, clientX: rect.x + rect.width * .5, clientY: rect.y + rect.height * .9 };
        await target.dispatchEvent('dragover', drop);
        await target.dispatchEvent('drop', drop);
        await source.dispatchEvent('dragend', { dataTransfer: transfer });
        await settle(page);
        const after = await snapshot(page);
        // Above/below drops intentionally reorder tiles. Compare each
        // tile's identity/history by key, independently of its placement.
        const identities = saved => saved.tiles.map(t => [t.key, t.columns, t.back, t.forward]).sort((a, b) => a[0] - b[0]);
        assert.deepEqual(identities(after), identities(before));
        const stacks = saved => saved.tiles.map(t => [t.key, t.stack]).sort((a, b) => a[0] - b[0]);
        assert.notDeepEqual(stacks(after), stacks(before), 'drop must actually move the tile between stacks');
        assert.equal(sessionColumns(after).length, 1);
        assert(await parent().locator('textarea').evaluate(el => el === window.__dragComposer), 'drag keeps the live composer DOM');
        assert.equal(await parent().locator('textarea').inputValue(), parentDraft);
        await oneStream('parent');
        await transfer.dispose();
      }
    }
    assert.deepEqual(f.errors, [], `${label}: browser errors`);
    console.log(`PASS workspace-open ${label}`);
  } catch (error) {
    if (f) {
      console.error(`FAIL during ${f.phase}: ${error.message}`);
      console.error('WORKSPACE FAILURE DIAGNOSTICS', JSON.stringify(await f.page.evaluate(key => ({
        url: location.href, layout: JSON.parse(localStorage.getItem(key)),
        links: Array.from(document.querySelectorAll('[data-pane-motion] a[href]'), a => ({ href: a.getAttribute('href'), text: a.textContent })),
        sockets: window.__workspaceSockets?.map(({ socket, path, listeners }) => ({ path, state: socket.readyState, messages: listeners.filter(l => l.type === 'message').length })),
      }), KEY).catch(diagnostic => ({ error: diagnostic.message }))));
      console.error('RECENT REQUESTS', JSON.stringify(f.requests.slice(-25)));
    }
    throw error;
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
          for (const platform of ['Linux x86_64', 'MacIntel']) {
            if (process.env.PLATFORM && process.env.PLATFORM !== platform) continue;
            const label = `${engine}/${name}/${platform}`;
            await within(`workspace-open ${label}`, () => check(browser, origin, viewport, label, platform));
          }
        }
      } finally { await browser.close(); }
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
}
if (require.main === module) main().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
