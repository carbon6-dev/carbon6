#!/usr/bin/env node
'use strict';

// Production WASM/CSS, private loopback API, no live head or user settings.
// BEFORE=1 records the same initial states without requiring the new chrome.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, launchBrowser } = require('./motion.cjs');

const SHOTS = process.env.SCREENSHOTS || '/tmp/s14-bar';
const BEFORE = process.env.BEFORE === '1';
const PHASE = BEFORE ? 'before' : 'after';
const MOTION = { choice: 'off', effective: 'off', os: 'no-preference' };
// Fixture operations retain their usual 300s timeout (or the explicit env
// override). Give that timeout's catch block time to save evidence before a
// separate case watchdog closes the browser for a genuine harness hang.
const CASE_WATCHDOG_MS = 2 * Number(process.env.C6_TEST_DEADLINE_SECS || 300) * 1000;
async function caseWatchdog(label, run) {
  let timer;
  try {
    return await Promise.race([run(), new Promise((_, reject) => {
      timer = setTimeout(() => reject(new Error(`${label}: case watchdog (${CASE_WATCHDOG_MS}ms)`)), CASE_WATCHDOG_MS);
    })]);
  } finally { clearTimeout(timer); }
}
// Verified on the pristine production baseline. This exact engine diagnostic
// means WebKit ignores an unsupported viewport key, not that the app crashed.
// Keep the raw message in evidence; no other console error is classified away.
const WEBKIT_VIEWPORT_DIAGNOSTIC = 'Viewport argument key "interactive-widget" not recognized and ignored.';
const knownViewportDiagnostic = (engine, message) => engine === 'webkit'
  && message.type === 'error' && message.text === WEBKIT_VIEWPORT_DIAGNOSTIC;
const consoleDiagnostics = (engine, messages) => ({
  webkitUnsupportedViewportKey: messages.filter(m => knownViewportDiagnostic(engine, m)),
});
const DOC = 'phone-bar-doc';
const USED_WORK = 'phone-bar-used-work';
const USED_WORK_TITLE = 'Review the phone workspace bar';
const SESSION = 'Motion fixture parent';
const TITLE = 'Release review — a deliberately long document title';
const VIEWPORTS = [
  ['portrait-320', { width: 320, height: 568 }],
  ['portrait-390', { width: 390, height: 844 }],
  ['portrait-430', { width: 430, height: 932 }],
  ['landscape-900', { width: 900, height: 430 }],
  ['landscape-430', { width: 430, height: 320 }],
];
const sessionView = { view: 'session', id: 'parent' };
const docView = { view: 'docs', id: DOC };
const CASES = [
  { name: 'session', views: [sessionView], cursor: 0, kind: 'session', ready: '[data-pane-kind="session"] textarea' },
  { name: 'session-doc-doc', views: [sessionView, docView], cursor: 1, kind: 'docs', ready: '[data-doc-reading] [data-block]' },
  { name: 'session-doc-session', views: [sessionView, docView], cursor: 0, kind: 'session', ready: '[data-pane-kind="session"] textarea' },
  { name: 'session-changes', views: [sessionView, { view: 'session_changes', id: 'parent' }], cursor: 1, kind: 'session_changes', ready: '[data-pane-kind="session_changes"] p:text-is("No changes.")' },
  { name: 'session-terminal', views: [sessionView, { view: 'session_terminal', id: 'parent' }], cursor: 1, kind: 'session_terminal', ready: '[data-pane-kind="session_terminal"] .xterm-screen' },
  { name: 'doc', views: [docView], cursor: 0, kind: 'docs', ready: '[data-doc-reading] [data-block]' },
  { name: 'settings', views: [{ view: 'settings', section: 'appearance' }], cursor: 0, kind: 'settings', ready: '[data-pane-kind="settings"] h1' },
  // The list chooses its search row by pane/container width, independently
  // of phone chrome's short-height breakpoint. 900x430 uses the wide row.
  { name: 'sessions', views: [{ view: 'sessions' }], cursor: 0, kind: 'sessions', ready: '[data-pane-kind="sessions"] input[type="search"]:visible' },
];
const selected = (key, value) => !process.env[key] || process.env[key].split(',').includes(value);
const layoutFor = test => ({ tiles: [{ key: 2,
  columns: test.views.map((view, i) => ({ key: 3 + i, view })), cursor: test.cursor,
  active: 3 + test.cursor, used: 3, back: [], forward: [], stack: 0, minimized: false,
}], focus: 0, next: 3 + test.views.length, full: false });
const address = view => view.view === 'docs' ? `/docs/${DOC}`
  : view.view === 'session' ? '/sessions/parent'
    : view.view.startsWith('session_') ? `/sessions/parent/${view.view.slice(8)}`
      : view.view === 'settings' ? '/settings/appearance' : '/sessions';
const stored = page => page.evaluate(() => JSON.parse(localStorage.getItem('c6.home.workspace.v2')));
const visiblePane = (page, kind) => page.locator(`[data-pane-kind="${kind}"]:visible`);
const bar = page => page.locator('[data-tile-bar]:visible');
const pop = page => page.locator('.phone-popover:visible');
// Session actions use buttons; document actions intentionally use menuitems.
const action = (page, name) => pop(page).getByRole('button', { name, exact: true })
  .or(pop(page).getByRole('menuitem', { name, exact: true }));
const phase = (page, label) => {
  page.__phoneBarPhase = label;
  console.log(`  ${label}`);
};
async function requireAction(page, name) {
  phase(page, `wait Actions entry ${name}`);
  await action(page, name).waitFor();
}

function documentFixture() {
  return { id: DOC, realm: 'home', kind: 'doc', title: TITLE, labels: [], rev: 1,
    created_by: 'session:parent', updated_by: 'you', created_at: 1, updated_at: 1, pending: 0,
    tabs: [{ id: 'reading', title: 'Reading', blocks: Array.from({ length: 12 }, (_, i) => ({
      id: `paragraph-${i}`, kind: 'paragraph', attrs: {}, hash: `hash-${i}`, by: 'you', rev: 1, children: [],
      text: `Section ${i + 1}. ${'The document remains readable under one phone bar. '.repeat(8)}`,
    })) }] };
}

async function setup({ context, page }, calls, consoleErrors) {
  let doc = documentFixture();
  const work = { id: USED_WORK, realm: 'home', version: 1, title: USED_WORK_TITLE,
    done_when: 'Document navigation dismisses Actions and preserves the reading pane.',
    status: 'open', state: 'open', why: '', labels: [], created_at: 1, updated_at: 1,
    attached: [{ ref: `doc:${DOC}`, title: TITLE, by: 'you', at: 1 }],
    live: [{ ref: `doc:${DOC}`, title: TITLE, state: '', at: 1 }] };
  const terminal = { id: 'bar-terminal', cwd: '/tmp', exited: null };
  // Motion keeps only its last 50 console messages. Keep every console error
  // so later warnings cannot hide an earlier failure or duplicate diagnostic.
  page.on('console', message => {
    if (message.type() === 'error') consoleErrors.push({ type: 'error', text: message.text() });
  });
  // Workspace restoration remembers previously visited document titles even
  // before a deferred hidden document mounts. Seed that normal browser cache.
  await context.addInitScript(({ id, title }) => {
    localStorage.setItem('c6.doc_titles', JSON.stringify({ [id]: title }));
  }, { id: DOC, title: TITLE });
  // Registered after motion's catch-all: only these PTY sockets are handled
  // here. Real bundled xterm mounts; no outside shell or terminal is started.
  await context.routeWebSocket(/\/terminals\/[^/?]+\/attach(?:\?.*)?$/, socket => {
    calls.push('terminal attach');
    socket.send(Buffer.from('Phone bar terminal fixture\r\n$ '));
    socket.onMessage(() => {});
  });
  await context.route('**/api/**', async route => {
    const req = route.request(), url = new URL(req.url()), p = url.pathname;
    const method = req.method();
    let json;
    if (p.endsWith(`/docs/${DOC}`)) {
      if (method === 'PATCH') {
        const patch = req.postDataJSON();
        calls.push({ docPatch: patch });
        doc = { ...doc, title: patch.title ?? doc.title, rev: doc.rev + 1,
          archived_at: patch.archived === undefined ? doc.archived_at : patch.archived ? 2 : null };
      }
      json = doc;
    } else if (p.endsWith(`/docs/${DOC}/read`)) json = doc;
    else if (p.endsWith(`/docs/${DOC}/google`)) json = { accounts: [], app: 'Google Docs', file: null };
    else if (p.endsWith(`/docs/${DOC}/history`)) json = [{ rev: 1, by: 'you', at: 1, ops: 1,
      added: ['paragraph-0'], changed: [], removed: [], summary: 'created' }];
    else if (p.startsWith('/api/v1/docs/')) json = [];
    else if (p.endsWith('/work/of')) json = url.searchParams.get('ref') === `doc:${DOC}` ? [work] : [];
    else if (p.endsWith(`/work/${USED_WORK}`)) json = work;
    else if (p.endsWith('/work')) json = url.searchParams.get('closed') === 'true' ? [] : [work];
    else if (p.endsWith('/sessions/parent/events')) json = [{ seq: 1, ts: 1, type: 'started',
      connection_id: 'test', model: 'test', cwd: '/tmp', system_prompt: '', tools: [], context_window: 128000 }];
    else if (p.endsWith('/terminals')) json = method === 'POST' ? terminal : [terminal];
    else if (p.endsWith('/fs/list')) json = { path: '/tmp', entries: [] };
    else return route.fallback();
    calls.push(`${method} ${p}`);
    await route.fulfill({ json });
  });
}

// Measure the actual session body's inner wrapper, not data-pane-kind's top:
// a second SessionStrip inside that outer wrapper must count as chrome too.
async function geometry(page) {
  return page.evaluate(() => {
    const visible = el => !!el && el.getBoundingClientRect().width > 0 && el.getBoundingClientRect().height > 0
      && getComputedStyle(el).visibility !== 'hidden' && !el.closest('[inert]');
    const rect = el => { if (!el) return null; const r = el.getBoundingClientRect();
      return { x: r.x, y: r.y, width: r.width, height: r.height, right: r.right, bottom: r.bottom }; };
    const panes = [...document.querySelectorAll('[data-pane-kind]')].filter(visible);
    const tile = document.querySelector('[data-tile-surface]');
    const bars = [...document.querySelectorAll('[data-tile-bar]')].filter(visible);
    // Baseline has no data-tile-bar. Record its first tile row without
    // requiring any controls or interpreting the old geometry as a pass.
    const header = bars[0] || tile?.firstElementChild;
    const pane = panes[0];
    const content = pane?.dataset.paneKind === 'session'
      ? pane.querySelector(':scope > div > div.relative.min-w-0')
      : pane?.dataset.paneKind === 'docs' ? pane.querySelector('[data-doc-reading]') : pane?.firstElementChild;
    const controls = header ? [...header.querySelectorAll('button,a')].filter(visible).map(el => ({
      label: el.getAttribute('aria-label') || el.textContent.trim(), disabled: el.matches(':disabled'), ...rect(el),
    })) : [];
    return { viewport: { width: innerWidth, height: innerHeight }, appTop: tile?.getBoundingClientRect().top,
      bars: bars.map(rect), header: rect(header), panes: panes.map(el => ({ kind: el.dataset.paneKind, ...rect(el) })),
      pane: rect(pane), content: rect(content), controls,
      title: header?.querySelector('[data-phone-title]')?.textContent,
      oldBars: [...document.querySelectorAll('[data-pane-bar],[data-session-strip],[data-doc-header]')].filter(visible)
        .map(el => ({ kind: el.hasAttribute('data-pane-bar') ? 'column' : el.hasAttribute('data-session-strip') ? 'session' : 'doc', ...rect(el) })),
      documentOverflow: document.documentElement.scrollWidth - innerWidth };
  });
}

async function capture(f, prefix, suffix) {
  await settle(f.page);
  const g = await geometry(f.page);
  await f.page.screenshot({ path: path.join(SHOTS, `${prefix}-${suffix}.png`) });
  fs.writeFileSync(path.join(SHOTS, `${prefix}-${suffix}.json`), JSON.stringify({ ...g,
    browserErrors: f.errors, consoleMessages: f.consoleMessages, consoleErrors: f.phoneBarConsoleErrors,
    consoleDiagnostics: consoleDiagnostics(f.page.context().browser().browserType().name(), f.phoneBarConsoleErrors) }, null, 2) + '\n');
  return g;
}

async function oneBar(f, prefix, suffix, kind, title) {
  await visiblePane(f.page, kind).waitFor();
  if (title) await f.page.waitForFunction(title => document.querySelector('[data-phone-title]')?.textContent === title, title);
  const g = await capture(f, prefix, suffix);
  assert.equal(g.bars.length, 1, 'exactly one workspace bar');
  assert.equal(g.panes.length, 1, 'only the selected pane is shown');
  assert(g.content, 'measure real mounted content, not an empty shell');
  assert(g.header.height > 0 && g.header.height <= 44, `bar height: ${JSON.stringify(g)}`);
  assert(Math.abs(g.header.y - g.appTop) < 1 && g.appTop >= 0 && g.appTop <= 1, 'bar starts at app top');
  assert(g.content.y - g.appTop <= 44.5, `all chrome to actual content <=44px: ${JSON.stringify(g)}`);
  assert(g.content.y >= g.header.bottom - 0.5, 'content does not overlap the bar');
  assert.deepEqual(g.oldBars, [], 'no visible column header, session strip, or document toolbar on phones');
  assert(g.header.x >= 0 && g.header.right <= g.viewport.width + 0.5);
  assert(g.documentOverflow <= 0, 'no viewport horizontal overflow');
  const ordered = g.controls.sort((a, b) => a.x - b.x);
  for (const control of ordered) {
    assert(control.x >= g.header.x - 0.5 && control.right <= g.header.right + 0.5, `control fits: ${JSON.stringify(control)}`);
    assert(control.y >= g.header.y - 0.5 && control.bottom <= g.header.bottom + 0.5, 'one-row controls');
    assert(control.width >= 43.5 && control.height >= 43.5, `44px phone target: ${JSON.stringify(control)}`);
  }
  for (let i = 1; i < ordered.length; i++) assert(ordered[i - 1].right <= ordered[i].x + 0.5,
    `no colliding controls at ${g.viewport.width}px: ${JSON.stringify(ordered)}`);
  assert(g.controls.some(c => c.label === 'Actions'), 'Actions remains in the bar');
  if (title) assert.equal(g.title, title, 'title is selected pane, not joined breadcrumbs');
  return g;
}

async function openActions(page) {
  phase(page, 'open Actions');
  await bar(page).getByRole('button', { name: 'Actions', exact: true }).click();
  await requireAction(page, 'Close this tile');
}
async function dismiss(page) {
  phase(page, 'dismiss Actions/pane switcher');
  await page.keyboard.press('Escape');
  await pop(page).waitFor({ state: 'hidden' });
}
async function choosePane(page, title, kind) {
  phase(page, `switch pane to ${title}`);
  await bar(page).getByRole('button', { name: 'Switch panes', exact: true }).click();
  assert.equal(await pop(page).getByRole('button').count(), 2, 'both columns remain reachable by title');
  assert.equal(await pop(page).locator('[aria-current="page"]').count(), 1, 'switcher marks the selected column');
  await pop(page).getByRole('button', { name: title, exact: true }).click();
  await visiblePane(page, kind).waitFor();
  if (kind === 'session') await visiblePane(page, kind).locator('textarea').waitFor();
  else if (kind === 'docs') await visiblePane(page, kind).locator('[data-block]').first().waitFor();
  else if (kind === 'session_terminal') await visiblePane(page, kind).locator('.xterm-screen').waitFor();
  else if (kind === 'session_changes') await visiblePane(page, kind).getByText('No changes.', { exact: true }).waitFor();
  await pop(page).waitFor({ state: 'hidden' });
}
async function backToSession(page) {
  phase(page, 'back to first pane');
  await bar(page).getByRole('button', { name: 'Back to the previous pane', exact: true }).click();
  await visiblePane(page, 'session').locator('textarea').waitFor();
  await page.waitForFunction(() => JSON.parse(localStorage.getItem('c6.home.workspace.v2')).tiles[0].cursor === 0);
}
async function expectRing(page, exists) {
  const ring = bar(page).getByRole('button', { name: /Open session sheet/ });
  if (exists) await ring.waitFor();
  else assert.equal(await ring.count(), 0, 'standalone doc/app pages have no session ring');
}

async function menuContract(page, { session, doc, split }) {
  phase(page, `Actions contract: session=${session}, doc=${doc}, split=${split}`);
  await openActions(page);
  for (const name of ['Back', 'Forward', 'Close this tile']) await requireAction(page, name);
  assert.equal(await action(page, 'Close this pane').count(), split ? 1 : 0);
  assert.equal(await action(page, 'Into its own tile').count(), split ? 1 : 0);
  assert.equal(await action(page, 'Pin').count() + await action(page, 'Unpin').count(), session ? 1 : 0,
    'Pin belongs to the shown session, including split tiles');
  if (session) {
    for (const name of [/^Rename…/, /^Find in session/, 'Files', /^Session details…/, 'Copy session ID', 'Copy session link',
      /^New like this…/, /^Save as template…/, 'Archive']) await requireAction(page, name);
  } else {
    assert.equal(await action(page, /^Find in session/).count(), 0, 'no hidden-session actions leak into another pane');
    assert.equal(await action(page, 'Files').count(), 0);
  }
  if (doc) {
    for (const name of ['Rename document', 'Show comments', 'Comment on the whole doc', 'Edit document',
      'History', 'Export…', 'Archive', 'Open the session that made it']) await requireAction(page, name);
    await pop(page).getByRole('link', { name: `Work · ${USED_WORK_TITLE}`, exact: true }).waitFor();
  } else assert.equal(await action(page, 'Rename document').count(), 0, 'no hidden document actions');
  await dismiss(page);
}

async function pin(page) {
  phase(page, 'pin then unpin session');
  await openActions(page);
  phase(page, 'click Pin');
  await action(page, 'Pin').click();
  await openActions(page);
  phase(page, 'wait Unpin');
  const unpin = action(page, 'Unpin');
  await unpin.waitFor();
  assert(await unpin.isEnabled(), 'split-session pin is actionable, not a disabled placeholder');
  await unpin.click();
  await openActions(page);
  await action(page, 'Pin').waitFor();
  await dismiss(page);
}

async function sessionActions(f, prefix) {
  const { page } = f;
  await openActions(page);
  phase(page, 'session Switch and Copy keep Actions open');
  const retainedMenu = await pop(page).elementHandle();
  const catchup = pop(page).getByRole('switch', { name: 'Catch-up', exact: true });
  await catchup.waitFor();
  assert.equal(await catchup.getAttribute('aria-checked'), 'true', 'fixture begins with Catch-up enabled');
  for (const on of [false, true]) {
    await catchup.click();
    await page.waitForFunction(on => document.querySelector('.phone-popover [role="switch"][aria-label="Catch-up"]')
      ?.getAttribute('aria-checked') === String(on), on);
    assert.equal(await pop(page).count(), 1, 'changing Catch-up keeps Actions open');
    assert(await retainedMenu.evaluate(el => el.isConnected), 'changing Catch-up retains the open menu');
  }
  const copy = action(page, 'Copy session ID');
  await copy.click();
  // No clipboard stub or permissions change: either native result is valid
  // feedback, and neither result may dismiss the Actions menu.
  await copy.getByRole('status').filter({ hasText: /^(Copied|Couldn't copy)$/ }).waitFor();
  assert.equal(await pop(page).count(), 1, 'Copy feedback keeps Actions open');
  assert(await retainedMenu.evaluate(el => el.isConnected), 'Copy feedback retains the open menu');
  phase(page, 'open Find in session');
  await action(page, /^Find in session/).click();
  const find = page.getByPlaceholder('Find in this session');
  phase(page, 'wait Find in this session input');
  await find.waitFor();
  await find.press('Escape');
  await find.waitFor({ state: 'hidden' });
  await openActions(page);
  phase(page, 'open Session details dialog');
  await action(page, /^Session details…/).click();
  const details = page.getByRole('dialog', { name: 'Session details', exact: true });
  await details.waitFor();
  await details.getByRole('button', { name: 'Close', exact: true }).click();
  await details.waitFor({ state: 'hidden' });
  await openActions(page);
  phase(page, 'open Files pane');
  await action(page, 'Files').click();
  await visiblePane(page, 'session_files').getByText('Select a file to view it.', { exact: true }).waitFor();
  await oneBar(f, prefix, 'files', 'session_files', `Files · ${SESSION}`);
  await backToSession(page);
  await choosePane(page, `Files · ${SESSION}`, 'session_files');
  await openActions(page);
  await action(page, 'Close this pane').click();
  await page.waitForFunction(() => JSON.parse(localStorage.getItem('c6.home.workspace.v2')).tiles[0].columns.length === 1);
  await visiblePane(page, 'session').locator('textarea').waitFor();
}

async function ringPanes(f, prefix) {
  const { page } = f;
  for (const [name, kind] of [['Changes', 'session_changes'], ['Terminal', 'session_terminal']]) {
    phase(page, `open session ring for ${name}`);
    await bar(page).getByRole('button', { name: /Open session sheet/ }).click();
    const sheet = page.getByRole('dialog', { name: 'Session', exact: true });
    phase(page, `open ${name} from Session sheet`);
    await sheet.getByRole('button', { name, exact: true }).click();
    await visiblePane(page, kind).waitFor();
    if (name === 'Terminal') await visiblePane(page, kind).locator('.xterm-screen').waitFor();
    else await visiblePane(page, kind).getByText('No changes.', { exact: true }).waitFor();
    await expectRing(page, true);
    await oneBar(f, prefix, `ring-${name.toLowerCase()}`, kind, `${name} · ${SESSION}`);
    await backToSession(page);
    await choosePane(page, `${name} · ${SESSION}`, kind);
    await openActions(page);
    await action(page, 'Close this pane').click();
    await page.waitForFunction(() => JSON.parse(localStorage.getItem('c6.home.workspace.v2')).tiles[0].columns.length === 1);
    await visiblePane(page, 'session').locator('textarea').waitFor();
  }
}

async function genericTileActions(f, prefix) {
  const { page } = f;
  phase(page, 'exercise tile history, detach and minimize');
  const columns = JSON.stringify((await stored(page)).tiles[0].columns);
  await openActions(page);
  assert(await action(page, 'Back').isEnabled(), 'pane visits retain tile history');
  await action(page, 'Back').click();
  await page.waitForFunction(columns => JSON.stringify(JSON.parse(localStorage.getItem('c6.home.workspace.v2')).tiles[0].columns) !== columns, columns);
  await openActions(page);
  await action(page, 'Forward').click();
  await page.waitForFunction(columns => JSON.stringify(JSON.parse(localStorage.getItem('c6.home.workspace.v2')).tiles[0].columns) === columns, columns);
  await openActions(page);
  await action(page, 'Back').click();
  await page.waitForFunction(() => JSON.parse(localStorage.getItem('c6.home.workspace.v2')).tiles[0].columns.length === 2);
  await openActions(page);
  await action(page, 'Into its own tile').click();
  await page.waitForFunction(() => {
    const l = JSON.parse(localStorage.getItem('c6.home.workspace.v2'));
    return l.tiles.length === 2 && l.tiles.every(t => t.columns.length === 1);
  });
  await openActions(page);
  await action(page, 'Minimize').click();
  await page.waitForFunction(() => JSON.parse(localStorage.getItem('c6.home.workspace.v2')).tiles.some(t => t.minimized));
  await capture(f, prefix, 'detached-minimized');
  await bar(page).getByRole('button', { name: 'Actions', exact: true }).waitFor();
}

async function documentActions(f, prefix, calls) {
  const { page } = f;
  // The session ring is usable even when the document owns the visible body.
  if ((await stored(page)).tiles[0].columns.some(c => c.view.view === 'session')) {
    phase(page, 'open ring while doc shown');
    await bar(page).getByRole('button', { name: /Open session sheet/ }).click();
    const sheet = page.getByRole('dialog', { name: 'Session', exact: true });
    for (const name of ['Changes', 'Terminal']) await sheet.getByRole('button', { name, exact: true }).waitFor();
    await page.keyboard.press('Escape');
    await sheet.waitFor({ state: 'hidden' });
  }
  await openActions(page);
  phase(page, 'cancel document rename with Escape');
  await action(page, 'Rename document').click();
  const title = page.getByRole('textbox', { name: 'Document title', exact: true });
  await title.waitFor();
  const patchesBeforeCancel = calls.filter(c => c.docPatch);
  await title.fill(`${TITLE} cancelled draft`);
  await title.press('Escape');
  await title.waitFor({ state: 'hidden' });
  await settle(page);
  assert.deepEqual(calls.filter(c => c.docPatch), patchesBeforeCancel, 'Escape cancellation does not PATCH the document');
  assert.equal(await bar(page).locator('[data-phone-title]').textContent(), TITLE, 'Escape cancellation leaves the document title unchanged');
  await openActions(page);
  phase(page, 'rename document with Enter');
  await action(page, 'Rename document').click();
  await title.waitFor();
  assert.equal(await title.inputValue(), TITLE, 'reopened rename discards the cancelled draft');
  await title.fill(`${TITLE} renamed`);
  await title.press('Enter');
  await title.waitFor({ state: 'hidden' });
  await page.waitForFunction(title => document.querySelector('[data-phone-title]')?.textContent === title, `${TITLE} renamed`);
  assert(calls.some(c => c.docPatch?.title === `${TITLE} renamed`), 'rename reaches fixture API');
  await openActions(page);
  phase(page, 'show document comments');
  await action(page, 'Show comments').click();
  const closeComments = page.getByRole('button', { name: 'Close the comments', exact: true });
  await closeComments.waitFor();
  await closeComments.click();
  await closeComments.waitFor({ state: 'hidden' });
  await openActions(page);
  phase(page, 'comment on whole doc');
  await action(page, 'Comment on the whole doc').click();
  await closeComments.waitFor();
  await closeComments.click();
  await closeComments.waitFor({ state: 'hidden' });
  await openActions(page);
  phase(page, 'edit document');
  await action(page, 'Edit document').click();
  await visiblePane(page, 'docs').getByRole('button', { name: 'Add a block at the end', exact: true }).waitFor();
  await openActions(page);
  await action(page, 'Done').click();
  for (const [name, heading] of [['History', 'History'], ['Export…', 'Export']]) {
    await openActions(page);
    phase(page, `document ${name}`);
    await action(page, name).click();
    await visiblePane(page, 'docs').getByRole('heading', { name: heading, exact: true }).waitFor();
    await openActions(page);
    await action(page, name).click();
    await visiblePane(page, 'docs').getByRole('heading', { name: heading, exact: true }).waitFor({ state: 'hidden' });
  }
  await openActions(page);
  phase(page, 'archive document');
  await action(page, 'Archive').click();
  await visiblePane(page, 'docs').getByText('Archived. Read-only.', { exact: true }).waitFor();
  await openActions(page);
  assert.equal(await action(page, 'Edit document').count(), 0, 'archived doc is read-only');
  await action(page, 'Unarchive').click();
  await visiblePane(page, 'docs').getByText('Archived. Read-only.', { exact: true }).waitFor({ state: 'hidden' });
  await oneBar(f, prefix, 'document-actions', 'docs', `${TITLE} renamed`);
  // UsedIn uses Go rather than DocMenuItems' owner callback. Use the standalone
  // doc so normal beside navigation retains it as the tile's first column;
  // a doc already beside a session is instead replaced into tile history.
  const previous = (await stored(page)).tiles[0];
  if (previous.columns.length === 1) {
    const reading = await visiblePane(page, 'docs').locator('[data-doc-reading]').elementHandle();
    await openActions(page);
    phase(page, 'navigate through document Used in');
    await pop(page).getByText('Used in 1', { exact: true }).waitFor();
    await pop(page).getByRole('link', { name: `Work · ${USED_WORK_TITLE}`, exact: true }).click();
    await visiblePane(page, 'work').getByRole('heading', { name: USED_WORK_TITLE, exact: true }).waitFor();
    await settle(page);
    assert.equal(await pop(page).count(), 0, 'Used in navigation dismisses its Actions popover');
    assert(await reading.evaluate(el => el.isConnected), 'Used in navigation retains the document body');
    const opened = (await stored(page)).tiles[0];
    assert.deepEqual(opened.columns[0], previous.columns[0], 'Used in keeps the source document column unchanged');
    assert.deepEqual(opened.columns[opened.cursor].view, { view: 'work', id: USED_WORK }, 'Used in reveals the correct work destination');
    await capture(f, prefix, 'used-in-work');
    await openActions(page);
    phase(page, 'return from Used in through tile history');
    assert(await action(page, 'Back').isEnabled(), 'Used in navigation preserves tile history');
    await action(page, 'Back').click();
    await page.waitForFunction(previous => {
      const tile = JSON.parse(localStorage.getItem('c6.home.workspace.v2')).tiles[0];
      return JSON.stringify(tile.columns) === JSON.stringify(previous.columns) && tile.cursor === previous.cursor;
    }, previous);
    await visiblePane(page, 'docs').locator('[data-doc-reading]').waitFor();
    assert(await reading.evaluate(el => el.isConnected), 'history return reuses the retained document body');
    await oneBar(f, prefix, 'used-in-return', 'docs', `${TITLE} renamed`);
  }
  await openActions(page);
  phase(page, 'open document owner');
  await action(page, 'Open the session that made it').click();
  await visiblePane(page, 'session').locator('textarea').waitFor();
  await settle(page);
  assert.equal(await pop(page).count(), 0, 'owner navigation dismisses its Actions popover');
  const layout = await stored(page);
  assert.equal(layout.tiles.flatMap(t => t.columns).filter(c => c.view.view === 'session' && c.view.id === 'parent').length, 1,
    'open owner reuses the session when already present');
}

async function check(browser, origin, theme, viewportName, viewport, test, desktop = false) {
  const prefix = `${PHASE}-${browser.browserType().name()}-${theme}-${viewportName}-${test.name}`;
  console.log(`RUN ${prefix}`);
  const calls = [];
  const consoleErrors = [];
  let progress, f;
  try {
    f = await fixture(browser, origin, viewport, theme, MOTION, p => {
      progress = p; p.phoneBarConsoleErrors = consoleErrors;
    }, {}, {
      layout: layoutFor(test), path: address(test.views[test.cursor]), ready: test.ready,
      instrumentation: false, setup: args => setup(args, calls, consoleErrors),
    });
    f.phase = prefix;
    const { page } = f;
    if (desktop && viewportName === 'desktop-narrow') {
      // Desktop breakpoint with a genuinely narrow tile, not a phone viewport.
      await page.addStyleTag({ content: '[data-tile-motion] { width: 350px !important; }' });
      await settle(page);
    }
    if (BEFORE) { await capture(f, prefix, 'initial'); return; }
    if (desktop) {
      const g = await capture(f, prefix, 'initial');
      assert.equal(g.bars.length, 1);
      assert.equal(await page.locator('[data-phone-title]:visible').count(), 0, 'phone title does not replace desktop title');
      assert.equal(await bar(page).getByRole('button', { name: 'Switch panes', exact: true }).count(), 0);
      await bar(page).getByRole('button', { name: 'Actions', exact: true }).waitFor();
      await page.getByRole('button', { name: /Open session sheet/ }).first().waitFor();
      assert(await page.locator('[data-pane-bar]:visible').count() > 0, 'desktop column chrome retained');
      if (viewportName === 'desktop-wide') {
        await page.locator('[data-doc-header]:visible').waitFor();
        await page.locator('[data-session-strip]:visible').waitFor();
        await page.getByRole('button', { name: 'Document actions', exact: true }).click();
        await page.getByRole('menuitem', { name: 'Open the session that made it', exact: true }).waitFor();
        await page.keyboard.press('Escape');
        await page.getByRole('menuitem', { name: 'Open the session that made it', exact: true }).waitFor({ state: 'hidden' });
        const header = page.locator('[data-doc-header]:visible');
        const heading = header.locator('[data-doc-heading] h1');
        const input = header.locator('[data-doc-heading]').getByRole('textbox');
        const entered = `${TITLE} desktop Enter`;
        const blurred = `${TITLE} desktop blur`;
        phase(page, 'cancel desktop document rename with Escape');
        await heading.click();
        await input.fill(`${TITLE} cancelled desktop draft`);
        await input.press('Escape');
        await input.waitFor({ state: 'hidden' });
        await settle(page);
        assert.deepEqual(calls.filter(c => c.docPatch), [], 'desktop Escape cancellation does not PATCH the document');
        assert.equal(await heading.textContent(), TITLE, 'desktop Escape cancellation leaves the document title unchanged');
        phase(page, 'desktop document rename with Enter');
        await heading.click();
        assert.equal(await input.inputValue(), TITLE, 'reopened desktop rename discards the cancelled draft');
        await input.fill(entered);
        await input.press('Enter');
        await input.waitFor({ state: 'hidden' });
        await header.getByRole('heading', { name: entered, exact: true }).waitFor();
        assert.equal(await heading.textContent(), entered, 'desktop Enter rename updates the document heading');
        assert.deepEqual(calls.filter(c => c.docPatch).map(c => c.docPatch.title), [entered], 'desktop Enter saves the exact title through PATCH');
        phase(page, 'desktop document rename with blur');
        await heading.click();
        await input.fill(blurred);
        await header.getByRole('button', { name: 'Document actions', exact: true }).focus();
        await input.waitFor({ state: 'hidden' });
        await header.getByRole('heading', { name: blurred, exact: true }).waitFor();
        assert.equal(await heading.textContent(), blurred, 'desktop blur rename updates the document heading');
        assert.deepEqual(calls.filter(c => c.docPatch).map(c => c.docPatch.title), [entered, blurred], 'desktop Enter and blur each PATCH their exact title');
        await capture(f, prefix, 'desktop-renames');
      }
      return;
    }
    const hasSession = test.views.some(v => v.view === 'session');
    await expectRing(page, hasSession); // Hidden-session initial mounting must finish first.
    const title = test.kind === 'session' ? SESSION : test.kind === 'docs' ? TITLE
      : test.kind === 'settings' ? 'Settings · Appearance' : test.kind === 'sessions' ? 'Sessions'
        : `${test.kind === 'session_changes' ? 'Changes' : 'Terminal'} · ${SESSION}`;
    // Settings labels follow the existing View::label; the test also checks the
    // actual shown pane rather than relying on a concatenated session title.
    await oneBar(f, prefix, 'initial', test.kind, title);
    await menuContract(page, { session: test.kind === 'session', doc: test.kind === 'docs', split: test.views.length === 2 });
    if (test.kind === 'session') await pin(page);
    if (test.views.length === 2) {
      const other = test.cursor === 0 ? 1 : 0;
      const secondKind = test.views[other].view;
      const secondTitle = secondKind === 'session' ? SESSION : secondKind === 'docs' ? TITLE
        : `${secondKind === 'session_changes' ? 'Changes' : 'Terminal'} · ${SESSION}`;
      await choosePane(page, secondTitle, secondKind);
      await expectRing(page, true);
      await oneBar(f, prefix, 'switched', secondKind, secondTitle);
      await menuContract(page, { session: secondKind === 'session', doc: secondKind === 'docs', split: true });
      await choosePane(page, title, test.kind);
      if (test.cursor === 1) {
        await backToSession(page);
        await oneBar(f, prefix, 'back', 'session', SESSION);
        await choosePane(page, title, test.kind);
      }
    }
    if (test.name === 'session') {
      await sessionActions(f, prefix);
      await ringPanes(f, prefix);
      await genericTileActions(f, prefix);
    } else if (test.kind === 'docs') await documentActions(f, prefix, calls);
    if (test.views.length === 2 && test.kind !== 'docs') {
      await openActions(page);
      await action(page, 'Close this pane').click();
      await page.waitForFunction(() => JSON.parse(localStorage.getItem('c6.home.workspace.v2')).tiles[0].columns.length === 1);
      const remaining = (await stored(page)).tiles[0].columns[0].view.view;
      await visiblePane(page, remaining).waitFor();
      await expectRing(page, remaining === 'session');
      assert(await bar(page).getByRole('button', { name: 'Switch panes', exact: true }).isDisabled(), 'no phantom second pane after close');
      await openActions(page);
      assert.equal(await action(page, 'Close this pane').count(), 0);
      await dismiss(page);
    }
    // Closing the last tile must lead to another real pane, not a dead bar.
    await openActions(page);
    await action(page, 'Close this tile').click();
    await page.waitForFunction(() => {
      const l = JSON.parse(localStorage.getItem('c6.home.workspace.v2'));
      return l.tiles.length > 0 && l.tiles.every(t => t.key !== 2);
    });
    await bar(page).getByRole('button', { name: 'Actions', exact: true }).waitFor();
  } catch (error) {
    const p = f || progress;
    if (p?.page && !p.page.isClosed()) {
      await p.page.screenshot({ path: path.join(SHOTS, `${prefix}-FAIL.png`) }).catch(() => {});
      fs.writeFileSync(path.join(SHOTS, `${prefix}-FAIL.json`), JSON.stringify({
        error: error.stack, phase: p.page.__phoneBarPhase || p.phase, browserErrors: p.errors, consoleMessages: p.consoleMessages,
        consoleErrors,
        calls, requests: p.requests,
        consoleDiagnostics: consoleDiagnostics(browser.browserType().name(), consoleErrors),
        menu: await pop(p.page).locator('button,[role="menuitem"]').allTextContents().catch(() => []),
        dialogs: await p.page.getByRole('dialog').evaluateAll(els => els.map(el => el.getAttribute('aria-label'))).catch(() => []),
        layout: await stored(p.page).catch(() => null),
      }, null, 2) + '\n');
    }
    throw error;
  } finally {
    const p = f || progress;
    if (p) {
      await p.context.close();
      assert.deepEqual(p.errors, [], `${prefix}: browser errors are fatal, including BEFORE`);
      const engine = browser.browserType().name();
      const diagnostics = consoleDiagnostics(engine, consoleErrors);
      assert(diagnostics.webkitUnsupportedViewportKey.length <= 1, `${prefix}: at most one known WebKit viewport diagnostic per page`);
      assert.deepEqual(consoleErrors.filter(m => m.type === 'error' && !knownViewportDiagnostic(engine, m)), [],
        `${prefix}: all other console errors are fatal`);
    }
  }
  console.log(`PASS ${prefix}`);
}

async function main() {
  fs.mkdirSync(SHOTS, { recursive: true });
  const { server, origin } = await serve();
  const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
  const engines = process.env.ENGINE === 'both' ? ['chromium', 'webkit'] : [process.env.ENGINE || 'chromium'];
  try {
    for (const engine of engines) {
      assert(['chromium', 'webkit'].includes(engine), `unsupported ENGINE=${engine}`);
      const browser = await launchBrowser(playwright, engine);
      try {
        for (const theme of ['light', 'dark'].filter(t => selected('THEME', t))) {
          for (const [name, viewport] of VIEWPORTS.filter(([name]) => selected('VIEWPORT', name))) {
            for (const test of CASES.filter(t => selected('CASE', t.name))) {
              await caseWatchdog(`${engine}/${theme}/${name}/${test.name}`, () => check(browser, origin, theme, name, viewport, test));
            }
          }
          // Deliberately separate desktop and narrow-desktop controls from the
          // complete phone matrix; each still has its own fresh context.
          if (!process.env.VIEWPORT || process.env.VIEWPORT.split(',').some(v => v.startsWith('desktop-'))) {
            for (const name of ['desktop-wide', 'desktop-narrow'].filter(n => selected('VIEWPORT', n))) {
              if (selected('CASE', 'session-doc-session')) await caseWatchdog(`${engine}/${theme}/${name}`, () => check(browser, origin, theme, name,
                { width: 1440, height: 1000 }, CASES.find(t => t.name === 'session-doc-session'), true));
            }
          }
        }
      } finally { await browser.close(); }
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
}

main().catch(error => {
  console.error(error.stack || error); process.exitCode = 1;
});
