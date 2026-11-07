#!/usr/bin/env node
'use strict';

// Production WASM, private loopback fake API, and the shared motion harness.
// No build, settings mutation, real head, credentials, or external services.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, within, launchBrowser, session } = require('./motion.cjs');
const baseline = process.env.BASELINE === '1';
const shots = process.env.SCREENSHOTS || '/tmp/n4-picker';
const phase = baseline ? 'before' : 'after';
const sizes = {
  narrow: { width: 320, height: 568 },
  phone: { width: 390, height: 844 },
  wide: { width: 430, height: 932 },
  desktop: { width: 1440, height: 1000 },
};
const motions = {
  full: { choice: 'full', effective: 'full', os: 'no-preference' },
  reduced: { choice: 'reduced', effective: 'reduced', os: 'no-preference' },
  off: { choice: 'off', effective: 'off', os: 'reduce' },
};
const ownText = 'Phone picker own message, editable before rewind';
const agentText = 'Phone picker agent reply, ask about this answer';
const layout = {
  tiles: [
    { key: 2, columns: [{ key: 3, view: { view: 'session', id: 'parent' } }], cursor: 0, active: 3, used: 3, back: [], forward: [], stack: 0, minimized: false },
    { key: 4, columns: [{ key: 5, view: { view: 'docs' } }], cursor: 0, active: 5, used: 5, back: [], forward: [], stack: 1, minimized: true },
  ], focus: 0, next: 6, full: false,
};
const events = [
  { seq: 72, ts: 72, type: 'user', command_id: 'picker-own', text: ownText },
  { seq: 73, ts: 73, type: 'assistant', turn: 73, blocks: [{ type: 'text', text: agentText }], stop_reason: 'end_turn' },
];
const deferred = () => {
  let resolve;
  const promise = new Promise(r => { resolve = r; });
  return { promise, resolve };
};
const step = (f, name) => {
  f.phase = name;
  console.log(`  PHASE ${f.label}: ${name}`);
};

// Page routes are installed by fixture's seed.setup before WASM starts. Each
// delayed response has an explicit gate: stale/retry assertions never depend
// on a network sleep, nor do they silently accept unmocked result shapes.
function historyFixture() {
  const calls = [], actionRequests = [], gates = new Map();
  const gate = q => {
    if (!gates.has(q)) gates.set(q, { started: deferred(), release: deferred() });
    return gates.get(q);
  };
  const result = (q, title) => ({ results: [{ session: { ...session(`saved-${q}`), title, ...(q === 'head' ? { on_head: true } : {}) },
    worker_name: q === 'head' ? '' : 'Saved worker',
    hits: [{ anchor: `saved-${q}-e73`, seq: 73, part: 0, kind: 'assistant', ts: 73,
      snippet: [{ text: 'Saved context ', hit: false }, { text: q, hit: true }, { text: ' after the match.', hit: false }] }], total_hits: 1,
    ...(q === 'route' ? { open_in: { session_id: 'parent', title: 'Top-level fixture', side_chats: ['saved-route'] } } : {}),
  }],
    terms: [q], ignored: [], truncated: false });
  return {
    calls,
    actionRequests,
    wait: q => gate(q).started.promise,
    release: q => gate(q).release.resolve(),
    setup: async ({ page }) => {
      page.on('request', request => {
        const p = new URL(request.url()).pathname;
        if (request.method() === 'POST' && /\/(side-chats|rewind|messages)$/.test(p)) {
          actionRequests.push({ path: p, body: request.postDataJSON() });
        }
      });
      await page.addInitScript(() => {
        window.__pickerInputs = {};
        document.addEventListener('input', event => {
          if (event.target.matches('input[type="search"]')) __pickerInputs[event.target.value] = performance.now();
        }, true);
      });
      await page.route('**/api/v1/sessions/parent/matches?*', route => route.fulfill({
        json: { matches: [], terms: ['route'], ignored: [], truncated: false },
      }));
      await page.route('**/api/v1/sessions/recent', route => route.fulfill({
        json: [{ session_id: 'recent', at: 2 }],
      }));
      // A real Ask request reaches its parent/anchor-specific route, then a
      // controlled failure keeps this test from inventing a running session.
      await page.route('**/api/v1/sessions/parent/side-chats', route => route.fulfill({
        status: 409, json: { error: 'Fixture Ask route reached' },
      }));
      await page.route('**/api/v1/search?*', async route => {
        const url = new URL(route.request().url()), q = url.searchParams.get('q');
        const elapsed = await page.evaluate(q => performance.now() - __pickerInputs[q], q);
        const call = { q, elapsed };
        calls.push(call);
        gate(q).started.resolve(call);
        if (['Motion', 'older', 'newer'].includes(q)) await gate(q).release.promise;
        if (q === 'retry' && calls.filter(c => c.q === q).length === 1) {
          await route.fulfill({ status: 500, json: { error: 'Fixture saved-history failure' } });
        } else {
          await route.fulfill({ json: result(q, `Saved history ${q}`) });
        }
      });
    },
  };
}

async function closed(page, opener) {
  // Presence can retain an inert exit node; do not require a particular
  // unmount policy or let a hidden retained dialog count as open.
  await page.waitForFunction(() => !document.querySelector('[role="dialog"][aria-label="Sessions"][data-open="true"]'));
  await settle(page);
  assert.equal(await page.getByRole('dialog', { name: 'Sessions', exact: true }).count(), 0, 'no accessible closed Sessions dialog');
  assert(await opener.evaluate(el => document.activeElement === el), 'closing returns focus to the list opener');
  assert(await opener.evaluate(el => !el.closest('[inert]')), 'closing restores background interactivity');
}

async function modalFocus(page, dialog, opener, label = 'Sessions') {
  assert(await dialog.evaluate(el => el.contains(document.activeElement)), 'initial focus is inside Sessions');
  assert(await dialog.locator('input').evaluateAll(inputs => !inputs.includes(document.activeElement)), 'Sessions does not autofocus Search/open the keyboard');
  assert(await opener.evaluate(el => !!el.closest('[inert]')), 'background opener is inert while modal is open');
  const composer = page.locator('textarea').first();
  assert(await composer.evaluate(el => !!el.closest('[inert]')), 'background transcript/composer is inert');
  await composer.evaluate(el => el.focus());
  assert(await dialog.evaluate(el => el.contains(document.activeElement)), 'native inert rejects programmatic focus behind Sessions');
  const count = await dialog.locator('button, input, a[href], [tabindex="0"]').count();
  assert(count > 2, 'exercise a nontrivial modal Tab cycle');
  for (const key of ['Tab', 'Shift+Tab']) {
    for (let n = 0; n < count + 2; n++) {
      await page.keyboard.press(key);
      assert(await dialog.evaluate(el => el.contains(document.activeElement)), `${key} remains inside ${label} at step ${n}`);
    }
  }
}

async function instantOverlays(f, dialog) {
  if (f.motion.effective === 'full') return;
  for (const surface of [dialog, f.page.locator('.c6-scrim[data-open="true"]')]) {
    const durations = await surface.evaluate(el => {
      const css = getComputedStyle(el);
      return { transition: css.transitionDuration, delay: css.transitionDelay,
        large: css.getPropertyValue('--dur-large').trim(), medium: css.getPropertyValue('--dur-medium').trim(),
        exit: css.getPropertyValue('--dur-sheet-exit').trim() };
    });
    assert([durations.transition, durations.delay].every(values => values.split(',').every(value => parseFloat(value) === 0)),
      `Reduced/Off phone surface and scrim have no timed transition: ${JSON.stringify(durations)}`);
    assert.equal(durations.large, '0ms');
    assert.equal(durations.medium, '0ms');
    assert.equal(durations.exit, '0ms');
  }
}

async function drawerGeometry(f, dialog) {
  const bounds = await dialog.evaluate(el => {
    const r = el.getBoundingClientRect(), css = getComputedStyle(el);
    return { left: r.left, right: r.right, top: r.top, bottom: r.bottom,
      visibleTop: visualViewport.offsetTop, visibleBottom: visualViewport.offsetTop + visualViewport.height,
      width: innerWidth, travel: Number(css.getPropertyValue('--travel')), touch: css.touchAction,
      enter: css.getPropertyValue('--dur-large').trim(), exit: css.getPropertyValue('--dur-sheet-exit').trim() };
  });
  assert(bounds.left > 0 && bounds.right <= bounds.width + 1 && bounds.right >= bounds.width - 1,
    `right drawer leaves a usable scrim, not a bottom/full-width sheet: ${JSON.stringify(bounds)}`);
  assert(bounds.top >= bounds.visibleTop - 1 && bounds.bottom <= bounds.visibleBottom + 1,
    `drawer fits the visible keyboard-bounded viewport: ${JSON.stringify(bounds)}`);
  assert.equal(bounds.travel, f.motion.effective === 'full' ? 1 : 0);
  assert.equal(bounds.enter, f.motion.effective === 'full' ? '280ms' : '0ms', 'Reduced/Off phone overlays open instantly');
  assert.equal(bounds.exit, f.motion.effective === 'full' ? '200ms' : '0ms', 'Reduced/Off phone overlays close instantly');
  assert(bounds.touch.includes('pan-y'), 'shared drawer preserves native vertical panning');
  assert(await dialog.evaluate(el => el.classList.contains('phone-drawer') && el.dataset.drawerSide === 'right'), 'use the shared right-side phone drawer, not a second swipe implementation');
  const header = await dialog.evaluate(el => {
    const title = el.querySelector('h2'), row = title.parentElement;
    const [ask, bell] = row.querySelectorAll('button');
    const box = node => {
      const r = node.getBoundingClientRect();
      return { left: r.left, right: r.right, width: r.width, height: r.height, cy: r.y + r.height / 2 };
    };
    return { row: box(row), title: box(title), ask: box(ask), bell: box(bell),
      askPill: box(ask.querySelector('span')), bellPill: box(bell.querySelector('span')),
      rowClose: box(el.querySelector('button[aria-label="Close this session or page"]')),
      buttons: row.querySelectorAll('button').length, icons: row.querySelectorAll('svg').length };
  });
  const diagnostic = JSON.stringify(header);
  assert.equal(header.buttons, 2, `header has only Ask and bell: ${diagnostic}`);
  assert.equal(header.icons, 2, `Ask and bell retain their SVG icons: ${diagnostic}`);
  assert.equal(header.row.height, 44, `header row is exactly 44px: ${diagnostic}`);
  for (const name of ['ask', 'bell', 'rowClose']) {
    assert.equal(header[name].height, 44, `${name} retains its actual 44px touch target: ${diagnostic}`);
    assert(header[name].width >= 44 && header[name].left >= bounds.left && header[name].right <= bounds.right,
      `${name} touch target fits within the drawer: ${diagnostic}`);
  }
  for (const name of ['askPill', 'bellPill']) {
    assert.equal(header[name].height, 20, `${name} keeps its 20px visual pill inside the larger target: ${diagnostic}`);
  }
  for (const name of ['title', 'ask', 'bell', 'askPill', 'bellPill']) {
    assert(Math.abs(header[name].cy - header.row.cy) < 1, `${name} is centered on the same header row: ${diagnostic}`);
  }
  await instantOverlays(f, dialog);
}

async function swipe(f, dialog, opener) {
  const { page, context } = f;
  // Detaching a secondary CDP session resets Chromium's touch emulation.
  // Reuse this session until our owned context closes (and releases it).
  const cdp = f.touchSession ||= await context.newCDPSession(page);
  const touch = (type, x, y) => cdp.send('Input.dispatchTouchEvent', {
    type, touchPoints: ['touchEnd', 'touchCancel'].includes(type) ? [] : [{ x, y, id: 1 }],
  });
  try {
    const r = await dialog.boundingBox();
    const x = r.x + 40, y = r.y + 24;
    const stillOpen = async reason => {
      await settle(page);
      assert.equal(await dialog.getAttribute('data-open'), 'true', reason);
      assert(Math.abs((await dialog.boundingBox()).x - r.x) < 1, `${reason}: remains docked`);
    };
    for (const [label, moves] of [
      ['small touch stays a tap', [[4, 2], [7, 1]]],
      ['wrong-way locks the whole gesture', [[-20, 1], [110, 1]]],
      ['vertical intent cannot turn into dismissal', [[2, 20], [110, 22]]],
    ]) {
      await touch('touchStart', x, y);
      for (const [dx, dy] of moves) await touch('touchMove', x + dx, y + dy);
      // Cancellation avoids a compatibility click on an unrelated header
      // control and still proves none of these directions was claimed.
      assert.equal(await dialog.getAttribute('data-dragging'), null, label);
      await touch('touchCancel', x, y);
      await stillOpen(label);
    }
    await touch('touchStart', x, y);
    await touch('touchMove', x + 45, y + 1);
    await page.waitForFunction(el => el.hasAttribute('data-dragging'), await dialog.elementHandle());
    await touch('touchCancel', x + 45, y + 1);
    await stillOpen('cancelled right drag does not dismiss');
    await touch('touchStart', x, y);
    await touch('touchMove', x + 35, y + 1);
    await page.waitForFunction(el => el.hasAttribute('data-dragging'), await dialog.elementHandle());
    const moved = (await dialog.boundingBox()).x - r.x;
    assert(f.motion.effective === 'full' ? moved > 10 : Math.abs(moved) < 1, `right drag tracks only with Full motion (${moved}px)`);
    await touch('touchMove', x + Math.ceil(r.width * .45), y + 1);
    await touch('touchMove', x + 25, y);
    await touch('touchEnd', x + 25, y);
    await settle(page);
    assert.equal(await dialog.getAttribute('data-open'), 'true', 'short reversed release snaps back without dismissing');
    assert(Math.abs((await dialog.boundingBox()).x - r.x) < 1, 'reversal returns to the exact right dock');
    await touch('touchStart', x, y);
    for (const dx of [30, 60, 90, 120, 130, 138, 142, 144, 145]) await touch('touchMove', x + dx, y + 2);
    await touch('touchEnd', x + 145, y + 2);
    await closed(page, opener);
  } finally {
    assert(await page.evaluate(() => matchMedia('(pointer: coarse)').matches && navigator.maxTouchPoints > 0),
      'native swipe must preserve the context’s touch-first device policy');
  }
}

async function interruptedEntrance(f, opener, save) {
  const { page, context } = f;
  const cdp = f.touchSession ||= await context.newCDPSession(page);
  const touch = (type, x, y) => cdp.send('Input.dispatchTouchEvent', {
    type, touchPoints: type === 'touchCancel' ? [] : [{ x, y, id: 1 }],
  });
  try {
    // Install before the one native opener tap, then seek the real CSS
    // transition at its transitionrun signal. A loaded host cannot miss a
    // short entrance, and neither the production helper nor CSS is replaced.
    await page.evaluate(() => {
      window.__pickerEntrance = null;
      window.__pickerEntranceListener = event => {
        const el = event.target;
        if (event.propertyName !== 'translate' || !el.matches('[role="dialog"][aria-label="Sessions"][data-open="true"]')) return;
        const animation = el.getAnimations().find(a => a.transitionProperty === 'translate');
        if (!animation) return;
        animation.pause();
        animation.currentTime = Number(animation.effect.getTiming().duration) * .20;
        window.__pickerEntrance = animation;
        document.removeEventListener('transitionrun', __pickerEntranceListener, true);
      };
      document.addEventListener('transitionrun', __pickerEntranceListener, true);
    });
    await opener.tap();
    await page.waitForFunction(() => __pickerEntrance?.playState === 'paused');
    const dialog = page.getByRole('dialog', { name: 'Sessions', exact: true });
    const before = await dialog.evaluate(el => {
      const r = el.getBoundingClientRect();
      return { x: r.x, y: r.y, width: r.width, translate: getComputedStyle(el).translate };
    });
    assert(before.translate.split(' ')[0].endsWith('%'), `the interrupted production entrance really exposes percentage translate: ${JSON.stringify(before)}`);
    const percent = parseFloat(before.translate);
    assert(percent > 5 && percent < 80, 'pause a visible, nontrivial unfinished entrance');
    const x = before.x + 35, y = before.y + 24;
    assert(x + 35 < f.viewport.width, 'native pickup and move remain inside the visible phone');
    await touch('touchStart', x, y);
    await touch('touchMove', x + 35, y + 1);
    await page.waitForFunction(el => el.hasAttribute('data-dragging'), await dialog.elementHandle());
    const after = await dialog.boundingBox();
    assert(Math.abs(after.x - before.x - 35) < 1,
      `native pickup adds 35px to the displayed percentage position without a jump: ${JSON.stringify({ before, after })}`);
    await save('interrupted-entrance');
    await touch('touchCancel', x + 35, y + 1);
    await settle(page);
    assert.equal(await dialog.getAttribute('data-open'), 'true', 'cancelled entrance pickup remains open');
    assert(Math.abs((await dialog.boundingBox()).x - (f.viewport.width - before.width)) < 1, 'cancelled pickup settles at the exact right dock');
    await page.keyboard.press('Escape');
    await closed(page, opener);
  } finally {
    await page.evaluate(() => {
      document.removeEventListener('transitionrun', __pickerEntranceListener, true);
      __pickerEntrance?.cancel();
    }).catch(() => {});
    assert(await page.evaluate(() => matchMedia('(pointer: coarse)').matches && navigator.maxTouchPoints > 0),
      'entrance pickup must preserve the context’s touch-first device policy');
  }
}

async function search(f, dialog, history, save) {
  step(f, 'search: local groups and short queries');
  const { page } = f;
  const input = dialog.getByRole('searchbox');
  const groupNames = ['Open sessions', 'Recently viewed', 'Other session matches', 'Other open pages'];
  const order = () => dialog.evaluate((el, names) => [...el.querySelectorAll('div')]
    .filter(node => names.includes(node.textContent)).map(node => node.textContent), groupNames);
  const expectOrder = async (expected, reason) => {
    for (const heading of expected) await dialog.getByText(heading, { exact: true }).waitFor();
    await settle(page);
    assert.deepEqual(await order(), expected, reason);
  };
  assert.equal(await input.getAttribute('aria-label'), 'Search');
  assert.equal(await dialog.getByText('Open sessions', { exact: true }).count(), 1, 'sessions have their own group');
  assert.equal(await dialog.getByText('Other open pages', { exact: true }).count(), 1, 'non-session open pages have a distinct group');
  await expectOrder(['Open sessions', 'Other open pages', 'Recently viewed'], 'empty search puts open pages before recently viewed sessions');
  await input.fill('He');
  const localHead = dialog.getByRole('button', { name: /^Head task/ });
  await localHead.waitFor();
  assert.equal(await localHead.getByText(/ · head$/).count(), 1, 'local head-session preview names head, not a worker or empty identity');
  assert.equal(history.calls.length, 0, 'short head-title query stays local');
  await input.fill('o');
  await expectOrder(['Open sessions', 'Recently viewed', 'Other open pages'], 'typing puts matching session groups before matching open pages');
  await input.fill('Do');
  await dialog.getByRole('button', { name: /^Motion fixture parent/ }).waitFor({ state: 'hidden' });
  await dialog.getByRole('button', { name: /^Docs/ }).waitFor();
  await dialog.getByText(/Title matches only/).waitFor();
  assert.equal(history.calls.length, 0, 'short queries filter local titles without making an invalid saved-history request');
  await input.fill('head');
  const savedHead = dialog.getByRole('button', { name: /^Saved history head/ });
  await savedHead.waitFor();
  assert.equal(await savedHead.getByText(/ · head$/).count(), 1, 'saved head hit with an empty worker name falls back to head identity');
  await input.fill('Docs');
  await dialog.getByText('Saved history Docs', { exact: true }).waitFor();
  await expectOrder(['Recently viewed', 'Other session matches', 'Other open pages'], 'typing also puts saved-history session matches before open pages');
  await input.fill('Motion');
  step(f, 'search: delayed history and keyboard');
  // The server's response remains blocked while the local row must render.
  await dialog.getByRole('button', { name: /^Motion fixture parent/ }).waitFor();
  await dialog.getByRole('button', { name: /^Docs/ }).waitFor({ state: 'hidden' });
  assert.equal(await dialog.getByRole('button', { name: /^Docs/ }).count(), 0, 'query filters local open pages immediately');
  const first = await history.wait('Motion');
  // This is a minimum policy interval, not a machine-speed upper bound:
  // slow rendering may take arbitrarily longer without failing the test.
  assert(first.elapsed >= 190, `saved history is debounced by 200ms, not sent for each keystroke: ${first.elapsed}ms`);
  assert.equal(await dialog.getByText('Saved history Motion', { exact: true }).count(), 0, 'history is genuinely delayed by the fixture');
  await input.focus();
  await page.evaluate(() => __keyboard(300, 12));
  await settle(page);
  await drawerGeometry(f, dialog);
  const field = await input.boundingBox();
  assert(field.y >= 12 && field.y + field.height <= 312, 'Search remains above the synthetic keyboard');
  await save('keyboard');
  await input.press('Enter');
  assert(await input.evaluate(el => document.activeElement !== el), 'Search Enter dismisses the keyboard by blurring');
  assert(await dialog.evaluate(el => document.activeElement === el), 'Search Enter returns focus to the dialog, retaining its Tab trap');
  assert.equal(await input.inputValue(), 'Motion', 'Search Enter retains the query');
  await page.evaluate(() => __keyboard(innerHeight));
  history.release('Motion');
  await dialog.getByText('Saved history Motion', { exact: true }).waitFor();
  const hit = dialog.getByRole('button', { name: /^Saved history Motion/ });
  assert.equal(await hit.locator('mark').textContent(), 'Motion', 'saved result highlights only the matched snippet');
  assert.equal(await hit.getByText('agent reply', { exact: true }).count(), 1, 'saved hit retains its entry-kind label');
  assert.match(await hit.textContent(), /Saved worker/, 'saved hit retains worker metadata');
  assert.match(await hit.textContent(), /Saved context Motion after the match\./, 'saved hit shows surrounding context, not just its title');
  assert.equal(await dialog.getByText('Other session matches', { exact: true }).count(), 1, 'saved results are grouped separately from open sessions');
  await save('local-and-history');

  await input.fill('older');
  step(f, 'search: stale response');
  await history.wait('older');
  await input.fill('newer');
  await history.wait('newer');
  history.release('newer');
  await dialog.getByText('Saved history newer', { exact: true }).waitFor();
  const olderResponse = page.waitForResponse(r => new URL(r.url()).searchParams.get('q') === 'older');
  history.release('older');
  // A later request/DOM turn is a completion barrier after the old response,
  // rather than a sleep which can miss a late stale replacement.
  await (await olderResponse).finished();
  await settle(page);
  assert.equal(await dialog.getByText('Saved history older', { exact: true }).count(), 0, 'an out-of-order history response cannot replace the current query');
  assert.equal(await dialog.getByText('Saved history newer', { exact: true }).count(), 1, 'newer history result survives stale completion');

  await input.fill('retry');
  step(f, 'search: retry');
  const retry = dialog.getByRole('button', { name: /Retry/i });
  await retry.waitFor();
  await retry.click();
  await dialog.getByText('Saved history retry', { exact: true }).waitFor();
  assert.equal(history.calls.filter(c => c.q === 'retry').length, 2, 'retry performs a second saved-history request');
  assert(history.calls.every(c => c.q), `history requests carry a nonempty query: ${JSON.stringify(history.calls)}`);
  await input.fill('');
  await dialog.getByRole('button', { name: /^Docs/ }).waitFor();
  await expectOrder(['Open sessions', 'Other open pages', 'Recently viewed'], 'clearing the query restores the empty-search group order');
}

async function openIn(f, history, opener, save) {
  step(f, 'saved result: parent-session routing');
  const { page } = f;
  await opener.tap();
  const dialog = page.getByRole('dialog', { name: 'Sessions', exact: true });
  await dialog.waitFor();
  await dialog.getByRole('searchbox').fill('route');
  const result = dialog.getByRole('button', { name: /^Saved history route/ });
  await result.waitFor();
  assert.equal(await result.locator('mark').textContent(), 'route');
  assert.equal(await result.getByText('in “Top-level fixture”', { exact: true }).count(), 1, 'side-chat result names its containing session');
  await save('open-in-result');
  await result.tap();
  await page.waitForFunction(() => {
    const layout = JSON.parse(localStorage.getItem('c6.home.workspace.v2'));
    return layout.tiles.some(t => t.columns.some(c => c.view.view === 'session' && c.view.id === 'parent' &&
      c.view.target?.q === 'route' && c.view.target.anchor === 'saved-route-e73' &&
      c.view.target.open?.length === 1 && c.view.target.open[0] === 'saved-route' && c.view.target.at == null));
  });
  await dialog.waitFor({ state: 'hidden' });
  await page.getByPlaceholder('Find in this session', { exact: true }).waitFor();
  assert.equal(await page.getByPlaceholder('Find in this session', { exact: true }).inputValue(), 'route', 'routing preserves the saved-hit query');
  const url = new URL(page.url());
  assert.equal(url.pathname, '/r/home/sessions/parent', 'open_in routes to the top-level session, not the side-chat session');
  assert.equal(url.searchParams.get('q'), 'route');
  assert.equal(url.searchParams.get('open'), 'saved-route');
  assert.equal(url.hash, '#saved-route-e73');
  assert(!f.requests.some(r => /^GET .*\/sessions\/saved-route(?:$|\/)/.test(r)), 'side-chat search result is not incorrectly opened as its own session');
  assert.equal(history.calls.filter(c => c.q === 'route').length, 1);
  await settle(page);
  await save('open-in-target');
}

async function messageActions(f, phone, history, save) {
  const { page } = f;
  for (const [seq, own] of [[72, true], [73, false]]) {
    const who = own ? 'own message' : 'agent reply';
    step(f, `${who}: actions eligibility`);
    const row = page.locator(`#parent-e${seq}`);
    await row.scrollIntoViewIfNeeded();
    const more = row.getByRole('button', { name: 'Message actions', exact: true });
    if (!phone || baseline) {
      if (!phone) {
        assert.equal(await more.count(), 0, 'desktop retains inline message actions, no phone more button');
        assert.equal(await row.getByRole('button', { name: 'Edit', exact: true }).count(), own ? 1 : 0);
        assert.equal(await row.getByRole('button', { name: 'Rewind', exact: true }).count(), own ? 1 : 0);
        assert.equal(await row.getByRole('button', { name: 'Ask', exact: true }).count(), 1);
      }
      await row.hover();
      await settle(page);
      await save(own ? 'own-actions' : 'agent-actions');
      continue;
    }
    assert.equal(await more.count(), 1, 'each eligible message exposes one phone action button');
    assert(await more.evaluate(el => {
      const r = el.getBoundingClientRect();
      for (let node = el; node; node = node.parentElement) {
        const css = getComputedStyle(node);
        if (css.visibility === 'hidden' || Number(css.opacity) === 0) return false;
      }
      return r.width >= 44 && r.height >= 44;
    }), 'phone more button is always visible without hover and has a 44px touch target');
    const dialog = page.getByRole('dialog', { name: 'Message actions', exact: true });
    const text = row.getByText(own ? ownText : agentText, { exact: true });
    const metadata = await more.evaluate(el => {
      const r = el.parentElement.getBoundingClientRect();
      return { y: r.y, height: r.height, x: r.x, right: r.right };
    });
    const textBox = await text.boundingBox();
    if (own) {
      assert.equal(metadata.height, 44, 'own message has one real 44px metadata row');
      assert(metadata.y >= textBox.y + textBox.height - 1, 'own metadata row is below, not over, the full-width user bubble');
    } else {
      assert(textBox.x + textBox.width <= metadata.x + 1, 'agent more target sits outside the reply text in its reserved gutter');
    }
    const open = async () => {
      await more.scrollIntoViewIfNeeded();
      await settle(page);
      const before = await row.boundingBox(), actions = history.actionRequests.length;
      const retained = await row.elementHandle();
      await more.focus();
      await more.tap();
      await dialog.waitFor();
      await settle(page);
      await instantOverlays(f, dialog);
      assert(await retained.evaluate(el => el.isConnected), 'opening actions retains the original message DOM');
      const after = await row.boundingBox();
      for (const dimension of ['x', 'y', 'width', 'height']) assert(Math.abs(before[dimension] - after[dimension]) < 1,
        `opening the portalled menu cannot reflow/move ${own ? 'own' : 'agent'} message ${dimension}: ${JSON.stringify({ before, after })}`);
      assert.equal(history.actionRequests.length, actions, 'merely opening Message actions sends/rewinds/branches nothing');
    };
    const close = async action => {
      const actions = history.actionRequests.length;
      await action();
      await dialog.waitFor({ state: 'hidden' });
      await settle(page);
      assert(await more.evaluate(el => document.activeElement === el && !el.closest('[inert]')), 'Message actions returns focus/interactivity to its own more button');
      assert.equal(history.actionRequests.length, actions, 'dismissing Message actions performs no message action');
    };
    await open();
    step(f, `${who}: modal containment and dismissal`);
    await modalFocus(page, dialog, more, 'Message actions');
    assert.equal(await dialog.getByRole('button', { name: 'Edit', exact: true }).count(), own ? 1 : 0);
    assert.equal(await dialog.getByRole('button', { name: /^Rewind/ }).count(), own ? 1 : 0);
    assert.equal(await dialog.getByRole('button', { name: 'Ask in a side chat', exact: true }).count(), 1);
    await save(own ? 'own-actions' : 'agent-actions');
    await close(() => page.keyboard.press('Escape'));
    await open();
    await close(() => dialog.getByRole('button', { name: 'Close', exact: true }).tap());
    await open();
    await close(() => page.touchscreen.tap(4, 4));

    if (own) {
      for (const edit of [true, false]) {
        step(f, `${who}: ${edit ? 'Edit' : 'Rewind'} confirmation entry`);
        const actions = history.actionRequests.length;
        await open();
        await dialog.getByRole('button', { name: edit ? 'Edit' : 'Rewind…', exact: true }).tap();
        await dialog.waitFor({ state: 'hidden' });
        const confirm = page.getByRole('button', { name: edit ? 'Resend' : 'Rewind', exact: true });
        await confirm.waitFor();
        const panel = confirm.locator('..').locator('..');
        if (edit) assert.equal(await panel.locator('textarea').inputValue(), ownText, 'Edit opens the existing draft, not an immediate resend');
        else assert.match(await panel.textContent(), /Rewind the conversation to before this message\?/);
        assert.equal(await panel.getByRole('checkbox').isChecked(), false, 'file restoration is never selected automatically');
        assert.equal(history.actionRequests.length, actions, 'Edit/Rewind entry requires a separate confirmation before any mutation');
        await save(edit ? 'edit-entry' : 'rewind-entry');
        await panel.getByRole('button', { name: 'Cancel', exact: true }).tap();
        await confirm.waitFor({ state: 'hidden' });
      }
    }
    await open();
    step(f, `${who}: open side-chat form`);
    if (process.env.PICKER_DIAGNOSTICS) console.log(`  ASK TARGET ${who}: ${JSON.stringify(await dialog.getByRole('button', { name: 'Ask in a side chat', exact: true }).evaluate(el => {
      const r = el.getBoundingClientRect();
      return { bounds: r.toJSON(), inert: !!el.closest('[inert]'), hit: document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2)?.outerHTML.slice(0, 600) };
    }))}`);
    await dialog.getByRole('button', { name: 'Ask in a side chat', exact: true }).tap();
    step(f, `${who}: Ask tapped; waiting for sheet close`);
    await dialog.waitFor({ state: 'hidden' });
    step(f, `${who}: sheet closed; waiting for side-chat textarea`);
    if (process.env.PICKER_DIAGNOSTICS) {
      const state = await page.evaluate(() => ({
        coarse: matchMedia('(pointer: coarse)').matches, active: document.activeElement?.outerHTML.slice(0, 600),
        textareas: [...document.querySelectorAll('textarea')].map(el => ({ placeholder: el.placeholder,
          inert: !!el.closest('[inert]'), bounds: el.getBoundingClientRect().toJSON(), parent: el.parentElement.className })),
        dialogs: [...document.querySelectorAll('[role="dialog"]')].map(el => ({ label: el.getAttribute('aria-label'), open: el.dataset.open, inert: el.inert })),
      }));
      console.log(`  ASK DOM ${who}: ${JSON.stringify(state)}`);
      fs.writeFileSync(path.join(shots, `${f.label}-${own ? 'own' : 'agent'}-ask-pending.json`), JSON.stringify(state, null, 2));
      await save(own ? 'own-ask-pending' : 'agent-ask-pending');
    }
    const prompt = page.getByPlaceholder('Ask about this in a side chat', { exact: true });
    await prompt.waitFor();
    step(f, `${who}: natural side-chat focus`);
    await settle(page);
    const form = prompt.locator('..');
    assert(await prompt.evaluate(el => document.activeElement === el && !el.closest('[inert]')),
      'Ask naturally hands focus to the side-chat textarea before any test fill/focus action');
    await prompt.fill(`Ask about ${own ? 'my message' : 'the agent reply'}`);
    const request = page.waitForRequest(r => new URL(r.url()).pathname === '/api/v1/sessions/parent/side-chats' && r.method() === 'POST');
    step(f, `${who}: submit side-chat Ask`);
    await form.getByRole('button', { name: 'Ask', exact: true }).tap();
    const sent = (await request).postDataJSON();
    step(f, `${who}: wait for fake Ask acknowledgement`);
    assert.equal(sent.anchor_seq, seq, 'Ask routes to the selected message, not the newest message');
    assert.equal(sent.prompt, `Ask about ${own ? 'my message' : 'the agent reply'}`);
    await form.getByText('Fixture Ask route reached', { exact: true }).waitFor();
    await save(own ? 'own-ask-route' : 'agent-ask-route');
    await form.getByRole('button', { name: 'Cancel', exact: true }).tap();
    await prompt.waitFor({ state: 'hidden' });
  }
  if (phone && !baseline) assert.deepEqual(history.actionRequests.map(r => r.path), [
    '/api/v1/sessions/parent/side-chats', '/api/v1/sessions/parent/side-chats',
  ], 'only the two explicitly submitted Ask requests mutate anything; confirmations were cancelled');
}

async function breakpoint(f, opener, save) {
  step(f, 'modal: leaving phone breakpoint');
  const { page } = f;
  await opener.focus();
  await opener.tap();
  await page.getByRole('dialog', { name: 'Sessions', exact: true }).waitFor();
  await page.setViewportSize(sizes.desktop);
  await page.getByRole('dialog', { name: 'Sessions', exact: true }).waitFor({ state: 'hidden' });
  await settle(page);
  const composer = page.locator('textarea').first();
  assert(await composer.evaluate(el => !el.closest('[inert]')), 'leaving the phone breakpoint releases modal background inert');
  await composer.focus();
  assert(await composer.evaluate(el => document.activeElement === el), 'desktop composer remains focusable after dismissing the phone modal');
  await save('breakpoint-desktop');
  await page.setViewportSize(f.viewport);
  await opener.waitFor();
  assert.equal(await page.getByRole('dialog', { name: 'Sessions', exact: true }).count(), 0, 'returning to phone does not resurrect the modal');
}

async function headerActions(f, opener, save) {
  step(f, 'picker header: nested bell');
  const { page } = f;
  await page.route('**/api/v1/attention', route => route.fulfill({ json: {
    waiting: [], elsewhere: [], worth: ['one', 'two'].map(name => ({
      key: `session:bell-${name}`, kind: 'turn_ended', title: `Bell fixture ${name}`,
      detail: `Bell fixture ${name}`, at: 1, session_id: 'parent',
    })),
  } }));
  await opener.focus();
  await opener.tap();
  const picker = page.getByRole('dialog', { name: 'Sessions', exact: true });
  await picker.waitFor();
  await settle(page);
  const pickerElement = await picker.elementHandle();
  // Refresh a retained NeedsYou as well as a newly mounted one; its fake API
  // completion, not a sleep, supplies a nontrivial three-control Tab cycle.
  await f.setState('idle');
  const bell = picker.getByRole('button', { name: /^What needs you:/ });
  await page.waitForFunction(el => el.getAttribute('aria-label') === 'What needs you: 2 worth a look', await bell.elementHandle());
  await bell.tap();
  const needs = page.getByRole('dialog', { name: 'What needs you', exact: true });
  await needs.waitFor();
  await needs.getByText('Worth a look · 2', { exact: true }).waitFor();
  await settle(page);
  assert(await needs.evaluate(el => el.contains(document.activeElement)), 'tapping the bell moves focus into its own nested modal');
  assert(await pickerElement.evaluate(el => !!el.closest('[inert]')), 'the parent Sessions picker is natively inert behind the bell');
  await pickerElement.evaluate(el => el.querySelector('input').focus());
  assert(await needs.evaluate(el => el.contains(document.activeElement)), 'native inert rejects focus back into the parent Search');
  const count = await needs.locator('button, a[href], input, [tabindex="0"]').count();
  assert(count >= 3, 'nested bell exercises Mark all seen and both real fixture links');
  for (const key of ['Tab', 'Shift+Tab']) for (let n = 0; n < count + 2; n++) {
    await page.keyboard.press(key);
    assert(await needs.evaluate(el => el.contains(document.activeElement)), `${key} remains in the nested bell at step ${n}`);
  }
  await save('picker-bell');
  await page.keyboard.press('Escape');
  await needs.waitFor({ state: 'hidden' });
  await settle(page);
  assert.equal(await picker.getAttribute('data-open'), 'true', 'Escape dismisses only the nested bell, not its parent Sessions drawer');
  assert.equal(await bell.getAttribute('aria-expanded'), 'false');
  assert(await bell.evaluate(el => document.activeElement === el), 'nested bell Escape returns focus to the actual tapped bell');
  assert(await pickerElement.evaluate(el => !el.closest('[inert]')), 'closing the bell restores the parent picker');
  assert(await opener.evaluate(el => !!el.closest('[inert]')), 'closing the nested bell does not release the parent modal background');
  await picker.getByRole('button', { name: 'Ask', exact: true }).tap();
  step(f, 'picker header: Ask form');
  const ask = page.getByRole('dialog', { name: 'Ask carbon6', exact: true });
  await ask.waitFor();
  await settle(page);
  assert(await ask.locator('textarea').evaluate(el => document.activeElement === el && !el.closest('[inert]')), 'picker Ask naturally hands focus to its assistant textarea before test input');
  await ask.locator('textarea').fill('Picker Ask entry only; do not submit');
  assert(await ask.locator('textarea').evaluate(el => document.activeElement === el && !el.closest('[inert]')), 'picker Ask opens its existing usable assistant form');
  await save('picker-ask');
  await ask.getByRole('button', { name: 'Cancel', exact: true }).tap();
  await ask.waitFor({ state: 'hidden' });
  // The existing owner-popup policy may already dismiss Sessions for Ask.
  // Whichever path ran, close the remaining picker and prove native inert is
  // released, rather than requiring a new parent-retention policy.
  if (await picker.count()) await page.keyboard.press('Escape');
  await page.waitForFunction(() => !document.querySelector('[role="dialog"][aria-label="Sessions"][data-open="true"]'));
  await settle(page);
  assert(await opener.evaluate(el => !el.closest('[inert]')), 'closing Ask and its owner leaves the app interactive');
}

async function check(browser, origin, engine, name, theme, motionName) {
  let progress;
  const label = `${phase}-${engine}-${name}-${theme}-${motionName}`;
  const phone = name !== 'desktop';
  const save = async suffix => {
    if (process.env.PICKER_DIAGNOSTICS && !phone) {
      const details = await progress.page.locator('#parent-e72, #parent-e73').evaluateAll(rows => rows.map(row => ({
        id: row.id, bounds: row.getBoundingClientRect().toJSON(), hovered: row.matches(':hover'),
        buttons: [...row.querySelectorAll('button')].map(button => ({ text: button.textContent, bounds: button.getBoundingClientRect().toJSON(),
          opacity: getComputedStyle(button.parentElement).opacity, display: getComputedStyle(button.parentElement).display,
          hover: button.matches(':hover'), parentClass: button.parentElement.className })),
      })));
      fs.writeFileSync(path.join(shots, `${label}-${suffix}.json`), JSON.stringify(details, null, 2));
    }
    await progress.page.screenshot({ path: path.join(shots, `${label}-${suffix}.png`) });
  };
  const history = historyFixture();
  try {
    const f = await fixture(browser, origin, sizes[name], theme, motions[motionName], p => { progress = p; }, {},
      { layout, events, sessions: [{ ...session('recent'), title: 'Recent Docs follow-up' },
        { ...session('head'), title: 'Head task', on_head: true }], setup: history.setup });
    f.label = label;
    step(f, 'fixture hydrated');
    assert.deepEqual(f.errors, [], 'fixture events must parse cleanly before interaction');
    const { page } = f;
    const navigation = await page.evaluate(() => ({ url: location.href, length: history.length }));
    const opener = page.getByRole('button', { name: /^Switch sessions and pages, 2 open$/ });
    if (phone) {
      await opener.focus();
      await opener.tap();
      let dialog = page.getByRole('dialog', { name: 'Sessions', exact: true });
      await dialog.waitFor();
      await settle(page);
      await save('sessions');
      if (!baseline) {
        step(f, 'picker: geometry and modal containment');
        assert.equal(await dialog.getAttribute('aria-modal'), 'true');
        assert.equal(await dialog.getByRole('button', { name: 'Close Sessions', exact: true }).count(), 0, 'no top X or draggable handle button');
        await drawerGeometry(f, dialog);
        await modalFocus(page, dialog, opener);
        await search(f, dialog, history, save);
      }
      await page.keyboard.press('Escape');
      if (!baseline) await closed(page, opener);
      else { await dialog.waitFor({ state: 'hidden' }); await settle(page); }
      if (!baseline) {
        step(f, 'picker: scrim and native swipe');
        await opener.tap();
        dialog = page.getByRole('dialog', { name: 'Sessions', exact: true });
        await dialog.waitFor(); await settle(page);
        await page.touchscreen.tap(4, 100);
        await closed(page, opener);
        await opener.tap();
        dialog = page.getByRole('dialog', { name: 'Sessions', exact: true });
        await dialog.waitFor(); await settle(page);
        if (engine === 'chromium') await swipe(f, dialog, opener);
        else {
          // Playwright has no native WebKit drag API. Do not replace the
          // production capture/cancellation path with fake PointerEvents.
          console.log('    native swipe: exercised by Chromium CDP only, not claimed for WebKit');
          await page.keyboard.press('Escape');
          await closed(page, opener);
        }
      }
    } else {
      assert.equal(await opener.count(), 0, 'desktop has no phone list opener');
      assert.equal(await page.getByRole('dialog', { name: 'Sessions', exact: true }).count(), 0);
    }
    if (phone && !baseline) {
      if (engine === 'chromium' && f.motion.effective === 'full') {
        step(f, 'picker: interrupted percentage entrance');
        await interruptedEntrance(f, opener, save);
      }
      await headerActions(f, opener, save);
    }
    await messageActions(f, phone, history, save);
    if (phone && !baseline) {
      await breakpoint(f, opener, save);
      assert.deepEqual(await page.evaluate(() => ({ url: location.href, length: history.length })), navigation,
        'opening/dismissing phone modals never inserts fake session-navigation history');
      await openIn(f, history, opener, save);
    }
    assert.deepEqual(f.errors, [], 'no browser crashes, UI error reports, or external requests');
    assert(!f.requests.some(r => /^(PUT|POST|DELETE) .*\/settings\//.test(r)), 'browser regression does not mutate settings');
    console.log(`PASS ${label}`);
  } catch (error) {
    await save('failure').catch(() => {});
    if (progress) fs.writeFileSync(path.join(shots, `${label}-failure.json`), JSON.stringify({
      phase: progress.phase, error: error.stack, errors: progress.errors, requests: progress.requests, console: progress.consoleMessages,
    }, null, 2));
    throw new Error(`${label} [${progress?.phase}]: ${error.message}`, { cause: error });
  } finally { await progress?.context.close(); }
}

async function main() {
  fs.mkdirSync(shots, { recursive: true });
  const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
  const engine = process.env.ENGINE || 'chromium';
  assert(['chromium', 'webkit', 'both'].includes(engine), 'ENGINE=chromium,webkit,both');
  const names = process.env.VIEWPORT ? process.env.VIEWPORT.split(',') : Object.keys(sizes);
  const themes = process.env.THEME ? process.env.THEME.split(',') : ['light', 'dark'];
  const choices = process.env.MOTION_CASE ? process.env.MOTION_CASE.split(',') : Object.keys(motions);
  for (const name of names) assert(Object.hasOwn(sizes, name), `Unknown VIEWPORT ${name}`);
  for (const theme of themes) assert(['light', 'dark'].includes(theme), `Unknown THEME ${theme}`);
  for (const choice of choices) assert(Object.hasOwn(motions, choice), `Unknown MOTION_CASE ${choice}`);
  const { server, origin } = await serve();
  try {
    for (const name of engine === 'both' ? ['chromium', 'webkit'] : [engine]) {
      const browser = await launchBrowser(playwright, name);
      try {
        for (const viewport of names) for (const theme of themes) for (const choice of choices) {
          await within(`${name}/${viewport}/${theme}/${choice}`, () => check(browser, origin, name, viewport, theme, choice));
        }
      } finally { await browser.close(); }
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
}

main().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
