#!/usr/bin/env node
'use strict';

// Production WASM and the owned loopback fixture; no head, live settings,
// product CSS overrides, or test-authored application DOM. See docs/testing.md.
// ENGINE=both ROOT=/tmp/q9-comments/before-dist BEFORE=1 node this-file
// ENGINE=both node this-file
// Both runs keep their screenshots and geometry JSON in /tmp/q9-comments.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');
const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
const before = process.env.BEFORE === '1';
const phase = before ? 'before' : 'after';
const evidence = process.env.EVIDENCE || '/tmp/q9-comments';
const DOC = 'comments-design-doc', OTHER_DOC = 'comments-design-other';
const PRIMARY = 'thread-start', SIBLING = 'thread-same-block', LATER = 'thread-later';
const cases = {
  'margin-fit': { width: 1800, height: 1000, count: 1, sheet: false },
  'tight-wide': { width: 1000, height: 900, count: 1, sheet: false },
  'narrow-tiles': { width: 1600, height: 1000, count: 4, sheet: true },
  phone: { width: 390, height: 844, count: 1, sheet: true },
  'phone-small': { width: 320, height: 568, count: 1, sheet: true },
};
const block = (id, text, extra = {}) => ({ id, kind: 'paragraph', text,
  attrs: {}, hash: `hash-${id}`, by: 'you', rev: 1, ...extra });
const doc = { id: DOC, realm: 'home', kind: 'doc', title: 'A quieter way to start work',
  labels: [], rev: 1, created_by: 'you', created_at: 1, updated_by: 'you', updated_at: 1, pending: 0,
  tabs: [{ id: 'reading', title: 'Reading', blocks: [
    block('intro', 'Keep the task and the start action together.', { kind: 'heading' }),
    block('start', 'Keep Start session visible. Pick a worker, write a task, and start the session without losing your place.'),
    ...Array.from({ length: 14 }, (_, i) => block(`paragraph-${i}`,
      `Section ${i + 1}. The document stays still when a conversation opens beside its block. Other tiles remain usable, and the reply controls stay within reach.`)),
    block('later', 'The history in the tile belongs after the start action.'),
  ] }, { id: 'appendix', title: 'Appendix', blocks: [
    // A slide is a real block tree in the document model. These panel-only
    // anchors distinguish tree order from API order, age, and lexical IDs.
    block('appendix-slide', '', { kind: 'slide', attrs: { layout: 'body' }, children: [
      block('appendix-child', 'A child block precedes the following root block.'),
    ] }),
    block('appendix-last', 'The final appendix block.'),
  ] }] };
function thread(id, anchor, about, count = 2, extra = {}) {
  return { id, doc: DOC, anchor: { tab: 'reading', ...anchor }, about,
    state: 'open', created_by: 'you', created_at: 1, needs_you: false, to_agent: false, unsent: true,
    comments: Array.from({ length: count }, (_, i) => ({ id: `${id}-comment-${i}`, at: i + 1,
      by: i % 2 ? 'session:parent' : 'you', editable: i === 0,
      text: i === 0 ? `Keep ${about.toLowerCase()} in view.`
        : `Reply ${i + 1}: The block stays visible, the conversation keeps its place, and the controls remain reachable.` })),
    ...extra };
}
function initialThreads(id = DOC) {
  // Deliberately NOT document order. Same-block ties keep their API order;
  // everything else must follow tab order and the block tree, not timestamps.
  return [
    thread('thread-appendix-last', { tab: 'appendix', block: 'appendix-last' }, 'Appendix ending'),
    thread(PRIMARY, { block: 'start', quote: 'Keep Start session visible.' }, 'Start session', 14, { needs_you: true }),
    thread(LATER, { block: 'later' }, 'History in the tile', 10),
    thread('thread-child', { tab: 'appendix', block: 'appendix-child' }, 'Appendix child'),
    thread(SIBLING, { block: 'start' }, 'Another discussion on the same block', 14),
    thread('thread-tab', {}, 'The whole reading tab'),
    thread('thread-resolved', { block: 'paragraph-0' }, 'Already resolved', 2,
      { state: 'resolved', resolved_by: 'you', resolved_at: 20, unsent: false }),
  ].map(t => ({ ...t, doc: id }));
}
const ordered = ['thread-tab', PRIMARY, SIBLING, LATER, 'thread-child', 'thread-appendix-last'];
function layout(count) {
  const index = count === 1 ? 0 : 1;
  return { tiles: Array.from({ length: count }, (_, i) => ({ key: 2 + i * 2,
    columns: [{ key: 3 + i * 2, view: i === index ? { view: 'docs', id: DOC }
      : i === count - 1 ? { view: 'docs', id: OTHER_DOC } : { view: 'session', id: 'parent' } }],
    cursor: 0, active: 3 + i * 2, used: 3 + i * 2, back: [], forward: [], stack: i, minimized: false })),
    focus: index, next: 2 + count * 2, full: false };
}
async function rect(locator) {
  return locator.evaluate(el => {
    const r = el.getBoundingClientRect();
    // As in comment-sheet.cjs, exclude the product's blank phone spring
    // overshoot tail, not any controls/content, from its visible bounds.
    const tail = el.matches('.phone-sheet') ? Math.min(0, parseFloat(getComputedStyle(el).marginBottom) || 0) : 0;
    return { x: r.x, y: r.y, width: r.width, height: r.height + tail, right: r.right, bottom: r.bottom + tail, tail };
  });
}
function contained(box, bounds, label) {
  assert(box.width > 0 && box.height > 0, `${label}: visible bounds`);
  assert(box.x >= bounds.x - 1 && box.y >= bounds.y - 1 && box.right <= bounds.right + 1
    && box.bottom <= bounds.bottom + 1, `${label}: contained: ${JSON.stringify({ box, bounds })}`);
}
function unmoved(a, b, label) {
  for (const key of ['x', 'y', 'width', 'height']) {
    assert(Math.abs(a[key] - b[key]) < 1, `${label}: block ${key} changed: ${JSON.stringify({ a, b })}`);
  }
}
function intersects(a, b) {
  return Math.min(a.right, b.right) - Math.max(a.x, b.x) > 1
    && Math.min(a.bottom, b.bottom) - Math.max(a.y, b.y) > 1;
}
async function check(browser, origin, engine, name, theme) {
  const config = cases[name];
  assert(config, `Unknown VIEWPORT ${name}`);
  const { width, height, count, sheet } = config, label = `${phase}-${engine}-${name}-${theme}`;
  const tileKey = count === 1 ? 2 : 4, tileSelector = `[data-tile-motion="${tileKey}"]`;
  const records = [], requests = [], states = new Map([[DOC, initialThreads()], [OTHER_DOC, initialThreads(OTHER_DOC)]]);
  const asyncFlows = !before && name === 'margin-fit' && theme === 'light';
  const deferred = new Map(), held = new Set();
  const deferRequest = key => {
    assert(!deferred.has(key), `Only one pending deferred request for ${key}`);
    let received, resume, responded;
    const request = new Promise(resolve => { received = resolve; });
    const wait = new Promise(resolve => { resume = resolve; });
    const response = new Promise(resolve => { responded = resolve; });
    const gate = { request, wait, response, received, responded,
      release: () => { held.delete(gate); resume(); } };
    deferred.set(key, gate); held.add(gate);
    return gate;
  };
  const fulfillApi = async (route, key, json) => {
    const gate = deferred.get(key);
    if (gate) {
      deferred.delete(key);
      gate.received({ path: new URL(route.request().url()).pathname, body: route.request().postDataJSON() });
      await gate.wait;
    }
    // Deferred GETs read the server state when released. Mutating responses
    // are snapshots of what that particular operation saved, not later edits.
    await route.fulfill({ json: typeof json === 'function' ? json() : json });
    gate?.responded();
  };
  let f;
  const save = () => fs.writeFileSync(path.join(evidence, `${label}.json`), JSON.stringify({
    label, before, asyncFlows, root: path.resolve(process.env.ROOT || path.join(__dirname, '../dist')),
    viewport: { width, height }, records, requests, errors: f?.errors || [], console: f?.consoleMessages || [],
  }, null, 2) + '\n');
  try {
    f = await fixture(browser, origin, { width, height }, theme,
      { choice: 'off', effective: 'off', os: 'no-preference' }, value => { f = value; }, {}, {
        layout: layout(count), path: `/docs/${DOC}`, ready: '[data-block="start"]',
        setup: async ({ context }) => context.route('**/api/**', async route => {
          const request = route.request(), p = new URL(request.url()).pathname;
          const match = p.match(/\/docs\/(comments-design-doc|comments-design-other)(?:\/(.*))?$/);
          if (match) {
            const [, id, tail] = match, threads = states.get(id);
            requests.push({ method: request.method(), path: p, body: request.postDataJSON() });
            if (!tail) return route.fulfill({ json: { ...doc, id, title: id === DOC ? doc.title : 'Another document' } });
            if (tail === 'threads' && request.method() === 'GET') return fulfillApi(route, `threads-get:${id}`, () => threads);
            const reply = tail.match(/^threads\/([^/]+)\/comments$/);
            if (reply && request.method() === 'POST') {
              const t = threads.find(t => t.id === reply[1]), body = request.postDataJSON();
              assert(t && body.text.trim(), `Invalid reply to ${tail}`);
              for (const c of t.comments) c.editable = false;
              t.comments.push({ id: 'deferred-reply-comment', by: 'you', at: 70, text: body.text, editable: true });
              Object.assign(t, { unsent: true, needs_you: false });
              return fulfillApi(route, `reply:${id}:${t.id}`, structuredClone(t));
            }
            const edit = tail.match(/^threads\/([^/]+)\/comments\/([^/]+)$/);
            if (edit && request.method() === 'PATCH') {
              const t = threads.find(t => t.id === edit[1]), c = t?.comments.find(c => c.id === edit[2]);
              assert(c?.editable, `Unknown/noneditable comment ${tail}`);
              c.text = request.postDataJSON().text;
              return fulfillApi(route, `edit:${id}:${t.id}:${c.id}`, structuredClone(t));
            }
            const resolve = tail.match(/^threads\/([^/]+)\/resolve$/);
            if (resolve && request.method() === 'POST') {
              const t = threads.find(t => t.id === resolve[1]);
              assert(t, `Unknown resolved thread ${tail}`);
              Object.assign(t, { state: 'resolved', resolved_by: 'you', resolved_at: 30, unsent: false });
              return route.fulfill({ json: t });
            }
            throw new Error(`Unmocked document API: ${request.method()} ${p}`);
          }
          if (p.endsWith('/docs')) return route.fulfill({ json: [{ ...doc, tabs: undefined }] });
          return route.fallback();
        }),
      });
    const page = f.page, tile = page.locator(tileSelector), anchor = tile.locator('[data-block="start"]');
    const pop = () => tile.locator(sheet ? '.phone-sheet[data-open="true"]'
      : before ? '.c6-pop[data-open="true"]' : '[data-thread-box]');
    const panel = () => tile.locator(before ? (sheet ? '.phone-sheet[data-open="true"]' : '.c6-pane[data-open="true"]') : '[data-comments-panel]');
    const toggle = id => panel().locator(`[data-thread-toggle="${id}"]`);
    const row = id => panel().locator(`[data-panel-thread="${id}"]`);
    const expanded = id => panel().locator(`[data-thread="${id}"]`);
    const editArea = () => expanded(PRIMARY).locator('textarea:not([placeholder])');
    const ownEditDraft = 'An unsaved edit to my own comment\nkeeps its text when its row closes.';
    const capture = async (state, surface) => {
      f.phase = state;
      await settle(page);
      const geometry = { state, viewport: page.viewportSize(), anchor: await rect(anchor), tile: await rect(tile),
        surface: surface ? await rect(surface) : null,
        reading: await tile.locator(before ? 'main' : '[data-doc-reading]').first().evaluate(el => {
          const r = el.getBoundingClientRect(); return { x: r.x, y: r.y, width: r.width, height: r.height, right: r.right, bottom: r.bottom };
        }),
        threads: await tile.locator('[data-thread]').evaluateAll(els => els.map(el => el.dataset.thread)),
        toggles: await tile.locator('[data-thread-toggle]').evaluateAll(els => els.map(el => ({ id: el.dataset.threadToggle, expanded: el.getAttribute('aria-expanded') }))),
      };
      records.push(geometry); save();
      await page.screenshot({ path: path.join(evidence, `${label}-${state}.png`) });
      if (!before && surface) {
        contained(geometry.surface, geometry.tile, `${label}/${state}/tile`);
        contained(geometry.surface, { x: 0, y: 0, right: width, bottom: height }, `${label}/${state}/viewport`);
        const outside = await surface.evaluate(el => {
          const s = el.getBoundingClientRect();
          return [...el.querySelectorAll('button, textarea')].filter(control => {
            const r = control.getBoundingClientRect();
            return r.width > 0 && (r.left < s.left - 1 || r.right > s.right + 1);
          }).map(el => el.getAttribute('aria-label') || el.textContent);
        });
        assert.deepEqual(outside, [], `${label}/${state}: controls do not clip horizontally`);
      }
      return geometry;
    };
    const close = async (surface, buttonName) => {
      const element = await surface.elementHandle();
      await surface.getByRole('button', { name: buttonName, exact: true }).click();
      await page.waitForFunction(el => !el.isConnected, element); await element.dispose(); await settle(page);
    };
    const openBlock = async () => {
      await anchor.scrollIntoViewIfNeeded(); await anchor.hover();
      await anchor.locator('[data-marks] button').click(); await pop().waitFor(); await settle(page);
    };
    const openPanel = async () => {
      if (!before && (width < 768 || height < 512)) {
        await tile.locator('[data-tile-bar]').getByRole('button', { name: 'Actions', exact: true }).click();
        await page.locator('.phone-popover:visible').getByRole('menuitem', { name: /^Show comments(?: \(\d+ open\))?$/ }).click();
      } else await tile.locator('button[data-opens="comments"]').first().click();
      await panel().waitFor(); await settle(page);
    };
    const reloadClosed = async () => {
      await f.reload({ waitUntil: 'domcontentloaded' }); await anchor.waitFor(); await settle(page);
      assert.equal(await panel().count(), 0, 'document reload does not open the comments panel');
    };
    const readPrefs = async id => page.evaluate(({ doc, id }) => {
      // Observe browser persistence; never seed or change application state.
      const key = Object.keys(localStorage).find(key => key.endsWith(`.doc-comments.${doc}`));
      if (!key) throw new Error(`Missing persisted comment preferences for ${doc}`);
      const prefs = JSON.parse(localStorage.getItem(key));
      return { read: prefs.read[id], expanded: prefs.expanded.includes(id) };
    }, { doc: DOC, id });
    const reachablePopControls = async label => {
      const bounds = await rect(pop());
      const controls = await pop().evaluate(el => [...el.querySelectorAll('button, textarea')]
        .filter(control => !control.closest('[data-thread-comments]'))
        .map(control => {
          const r = control.getBoundingClientRect(), hit = document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2);
          return { label: control.getAttribute('aria-label') || control.textContent,
            box: { x: r.x, y: r.y, width: r.width, height: r.height, right: r.right, bottom: r.bottom },
            hit: !!hit && control.contains(hit) };
        }).filter(control => control.box.width > 0 && control.box.height > 0));
      for (const control of controls) {
        contained(control.box, bounds, `${label}/${control.label}`);
        assert(control.hit, `${label}/${control.label}: control center receives pointer input`);
      }
    };
    const avoidAnchor = async (label, geometry) => {
      const measured = await pop().evaluate(el => {
        const comments = el.querySelector('[data-thread-comments]');
        const minimum = Math.max(160, el.offsetHeight - comments.clientHeight + 32);
        let area = el.closest('[data-tile-motion]').querySelector('[data-doc-reading]');
        while (area && !/auto|scroll/.test(getComputedStyle(area).overflowY)) area = area.parentElement;
        if (!area) throw new Error('Document has no reading scrollport');
        const r = area.getBoundingClientRect(), v = visualViewport;
        const x = Math.max(r.left + area.clientLeft, v.offsetLeft), y = Math.max(r.top + area.clientTop, v.offsetTop);
        return { minimum, area: { x, y,
          right: Math.min(r.left + area.clientLeft + area.clientWidth, v.offsetLeft + v.width),
          bottom: Math.min(r.top + area.clientTop + area.clientHeight, v.offsetTop + v.height) } };
      });
      const a = geometry.anchor, s = geometry.surface, t = measured.area;
      const sideFits = a.x - t.x >= s.width + 16 || t.right - a.right >= s.width + 16;
      const verticalRoom = Math.max(a.y - t.y - 16, t.bottom - a.bottom - 16);
      const short160Fits = verticalRoom >= 160;
      const chromeFits = verticalRoom >= measured.minimum;
      records.push({ state: `${label}-anchor-clearance`, ...measured, verticalRoom,
        sideFits, short160Fits, chromeFits, intersects: intersects(a, s) }); save();
      // A full-height box needn't fit: shrinking just its comments to a 160px
      // box is enough. With multiline drafts/menus, real chrome can exceed
      // 160px; require clearance whenever THAT usable shorter box fits.
      if (name === 'margin-fit' || sideFits || (short160Fits && chromeFits)) {
        assert(!intersects(a, s), `${label}: feasible shorter conversation avoids the anchor`);
      }
      if (name === 'margin-fit') {
        assert(s.x >= geometry.reading.right + 7 || s.right <= geometry.reading.x - 7,
          `${label}: a roomy tile places the conversation beside the reading column`);
      }
    };
    const lateReply = async () => {
      const replyText = 'A reply completed while its subsequent thread refresh was delayed.';
      const nextDraft = 'A new reply draft typed while the previous send was pending.';
      const reply = pop().getByPlaceholder('Reply…'), retained = await reply.elementHandle();
      const sent = deferRequest(`reply:${DOC}:${PRIMARY}`), refresh = deferRequest(`threads-get:${DOC}`);
      const sameConversation = async state => {
        assert.deepEqual(await pop().locator('[data-thread]').evaluateAll(els => els.map(el => el.dataset.thread)), [PRIMARY],
          `${state}: successful reply must not select its sibling on the same block`);
        assert(await reply.evaluate((el, previous) => el === previous && el.isConnected, retained),
          `${state}: reply textarea keeps its DOM identity`);
        assert.equal(await reply.inputValue(), nextDraft, `${state}: newer reply draft stays intact`);
      };
      try {
        await reply.fill(replyText);
        await pop().getByRole('button', { name: 'Send the reply', exact: true }).click();
        assert.equal((await sent.request).body.text, replyText, 'deferred reply sends the actual input');
        assert.equal(await reply.inputValue(), '', 'sending clears just the sent reply');
        await reply.fill(nextDraft);
        await sameConversation('reply pending');
        sent.release(); await refresh.request;
        await settle(page); await sameConversation('reply success before GET');
        await pop().getByText(replyText, { exact: true }).waitFor();
        await capture('late-reply-before-get', pop());
        // This extra fixture comment exists ONLY in the GET, providing an
        // observable application-completion barrier, not a network-only wait.
        states.get(DOC).find(t => t.id === PRIMARY).comments.push({ id: 'deferred-reply-refresh', by: 'session:parent', at: 71,
          text: 'The delayed thread refresh has finished.' });
        refresh.release(); await refresh.response;
        await pop().getByText('The delayed thread refresh has finished.', { exact: true }).waitFor();
        await settle(page); await sameConversation('reply after delayed GET');
        await capture('late-reply-after-get', pop());
      } finally { sent.release(); refresh.release(); await retained.dispose(); }
    };
    const lateEdit = async () => {
      const draftA = 'Edit A was saved by a component that has since collapsed.';
      const draftB = 'Edit B belongs to the reopened comment and must survive the old success.';
      const editable = states.get(DOC).find(t => t.id === PRIMARY).comments.find(c => c.editable);
      assert(editable, 'late-edit fixture has one own editable comment');
      await expanded(PRIMARY).getByRole('button', { name: 'Edit or delete your comment', exact: true }).click();
      await expanded(PRIMARY).getByRole('button', { name: 'Edit', exact: true }).click();
      await editArea().fill(draftA);
      const outgoing = await editArea().elementHandle();
      const saved = deferRequest(`edit:${DOC}:${PRIMARY}:${editable.id}`), refresh = deferRequest(`threads-get:${DOC}`);
      try {
        await expanded(PRIMARY).getByRole('button', { name: 'Save', exact: true }).click();
        assert.equal((await saved.request).body.text, draftA, 'deferred edit saves A, not the subsequent draft');
        await toggle(PRIMARY).click();
        assert(await outgoing.evaluate(el => !el.isConnected), 'saving editor really unmounts before its response');
        await toggle(PRIMARY).click();
        assert.equal(await editArea().inputValue(), draftA, 'pending saved edit resumes on reopen');
        assert(await editArea().evaluate((el, old) => el !== old, outgoing), 'reopened editor has a new component/DOM owner');
        await editArea().fill(draftB);
        saved.release(); await refresh.request;
        await settle(page);
        assert.equal(await editArea().inputValue(), draftB, 'late save-A success cannot clear the live edit-B input');
        await capture('late-edit-before-get', panel());
        // Teardown flushes B without a debounce sleep. An old success that
        // forgot the new owner's draft must fail this immediate reopen too.
        await toggle(PRIMARY).click(); await toggle(PRIMARY).click();
        assert.equal(await editArea().inputValue(), draftB, 'edit B survives teardown after late success');
        states.get(DOC).find(t => t.id === PRIMARY).comments.push({ id: 'deferred-edit-refresh', by: 'session:parent', at: 72,
          text: 'The delayed saved-edit refresh has finished.' });
        refresh.release(); await refresh.response;
        await expanded(PRIMARY).getByText('The delayed saved-edit refresh has finished.', { exact: true }).waitFor();
        assert.equal(await editArea().inputValue(), draftB, 'refreshing saved text A still resumes unsaved B');
        await toggle(PRIMARY).click(); await reloadClosed(); await openPanel();
        assert.equal(await toggle(PRIMARY).getAttribute('aria-expanded'), 'false', 'late-edit reload retains collapse state');
        await toggle(PRIMARY).click();
        assert.equal(await editArea().inputValue(), draftB, 'edit B remains after collapse and full browser reload');
        await capture('late-edit-draft-reloaded', panel());
        await expanded(PRIMARY).getByRole('button', { name: 'Cancel', exact: true }).click();
      } finally { saved.release(); refresh.release(); await outgoing.dispose(); }
    };
    const threadMenu = async text => {
      if (!before) {
        await pop().getByRole('button', { name: 'Thread options', exact: true }).click();
        await pop().locator('[data-thread-menu]').waitFor();
      }
      const button = pop().getByRole('button', { name: text, exact: false });
      if (!before) {
        const icon = text === 'All comments' ? ['M7 7h10v10', 'M7 17 17 7'] : ['M5 12h14', 'M12 5v14'];
        for (const d of icon) assert.equal(await button.locator(`svg path[d="${d}"]`).count(), 1, `${text}: standard SVG icon retained`);
      }
      await button.click(); await settle(page);
    };
    const otherTileClick = async () => {
      if (count === 1) return;
      const composer = page.locator('#col-3 textarea');
      await composer.click(); await composer.fill('The other tile still works');
      assert.equal(await composer.inputValue(), 'The other tile still works', 'other tile receives a real click and typing');
      await composer.press('Escape');
      assert.equal(await pop().count(), 1, 'other tile use does not dismiss this block conversation');
    };
    f.phase = 'closed';
    await anchor.scrollIntoViewIfNeeded(); await capture('closed');
    const original = await rect(anchor);
    await openBlock();
    const opened = await capture('block', pop());
    records.push({ state: 'opening-displacement', before: original, after: opened.anchor,
      intersection: intersects(opened.anchor, opened.surface) }); save();
    if (!before) {
      assert.equal(await pop().locator('[data-thread]').count(), 1, 'a block with two threads shows ONE conversation');
      if (!sheet) {
        unmoved(original, opened.anchor, `${label}/open`);
        await avoidAnchor('block', opened);
      } else {
        assert(opened.anchor.y >= opened.tile.y - 1 && opened.anchor.bottom <= opened.surface.y + 1,
          'sheet leaves the commented block visible above it');
      }
      const reply = pop().getByPlaceholder('Reply…'), resolve = pop().getByRole('button', { name: 'Resolve', exact: true });
      const r = await rect(reply), done = await rect(resolve), footer = await rect(pop().getByRole('switch', { name: 'Send to agent', exact: true }));
      contained(r, opened.surface, 'reply stays within the conversation');
      contained(done, opened.surface, 'Resolve stays within the conversation');
      contained(footer, opened.surface, 'Send to agent stays within the conversation');
      assert(Math.max(0, done.y - r.bottom, r.y - done.bottom) <= 24, 'Reply and Resolve stay close together');
      assert(footer.y >= r.bottom - 1, 'Send to agent is below the reply in the quiet footer');
      const comments = pop().locator('[data-thread-comments]');
      assert.equal(await comments.count(), 1, 'single-thread conversation has its own comments region');
      assert(await comments.evaluate(el => el.scrollHeight > el.clientHeight + 1 && /auto|scroll/.test(getComputedStyle(el).overflowY)),
        'long single-thread conversation scrolls without displacing the reply');
      await otherTileClick();
      // The message ellipsis must not become the thread menu.
      const ownMenu = pop().getByRole('button', { name: 'Edit or delete your comment', exact: true }).first();
      assert.equal(await ownMenu.locator('svg').count(), 1, 'own-message ellipsis keeps its SVG icon');
      await ownMenu.scrollIntoViewIfNeeded(); await ownMenu.click();
      assert(await pop().getByRole('button', { name: 'Edit', exact: true }).isVisible(), 'own message keeps Edit');
      assert(await pop().getByRole('button', { name: 'Delete', exact: true }).isVisible(), 'own message keeps Delete');
      await capture('message-menu', pop());
      await ownMenu.click();
    }
    if (name === 'tight-wide') {
      const reply = pop().getByPlaceholder('Reply…').first();
      await reply.fill('A multiline reply keeps the controls reachable.\nLine two keeps its text.\nLine three grows the reply.\nLine four needs room.\nLine five stays visible.\nLine six ends the draft.');
      if (!before) await pop().getByRole('button', { name: 'Thread options', exact: true }).click();
      const multiline = await capture('block-multiline-menu', pop());
      if (!before) {
        await pop().locator('[data-thread-menu]').waitFor();
        unmoved(original, multiline.anchor, `${label}/multiline-menu`);
        await reachablePopControls('multiline-menu');
        await avoidAnchor('multiline-menu', multiline);
        await pop().getByRole('button', { name: 'Thread options', exact: true }).click();
      }
      await reply.fill(''); await settle(page);
    }
    await threadMenu('New thread on this block');
    await pop().getByPlaceholder('Your comment').fill('A new-thread draft beside the same block');
    await capture('new-thread', pop());
    if (!before) assert.equal(await pop().locator('[data-thread]').count(), 0, 'new form hides the thread while writing');
    await pop().getByRole('button', { name: 'Cancel', exact: true }).click(); await settle(page);
    // Cancel historically dismisses the whole popover; the new form may
    // instead return to its conversation. Either way, close before reopening.
    if (await pop().count()) await close(pop(), 'Close');
    if (!before && !sheet) unmoved(original, await rect(anchor), `${label}/closed`);
    await openBlock();
    if (asyncFlows) await lateReply();
    await threadMenu('All comments'); await panel().waitFor();
    const panelDefault = await capture('panel-default', panel());
    if (!before && !sheet) {
      unmoved(original, panelDefault.anchor, `${label}/full-panel-handoff`);
    }
    if (before) {
      await capture('panel-expanded', panel());
    } else {
      assert.deepEqual(await panel().locator('[data-panel-thread]').evaluateAll(els => els.map(el => el.dataset.panelThread)), ordered,
        'panel follows tab and block-tree order, not API order');
      assert.equal(await panel().locator('[data-thread]').count(), 0, 'panel threads are collapsed by default');
      for (const id of ordered) assert.equal(await toggle(id).getAttribute('aria-expanded'), 'false', `${id}: collapsed aria-expanded`);
      await toggle(PRIMARY).click(); await expanded(PRIMARY).waitFor();
      await expanded(PRIMARY).getByPlaceholder('Reply…').fill('A reply draft survives independent collapse and reload');
      await expanded(PRIMARY).getByRole('button', { name: 'Edit or delete your comment', exact: true }).click();
      await expanded(PRIMARY).getByRole('button', { name: 'Edit', exact: true }).click();
      await editArea().fill(ownEditDraft);
      await capture('panel-own-edit', panel());
      await toggle(SIBLING).click(); await expanded(SIBLING).waitFor();
      assert.equal(await toggle(PRIMARY).getAttribute('aria-expanded'), 'true', 'opening a second row does not collapse the first');
      await capture('panel-expanded', panel());
      const scrollOwners = await panel().evaluate(el => [el, ...el.querySelectorAll('*')]
        .filter(e => e.tagName !== 'TEXTAREA' && e.clientHeight > 0 && e.scrollHeight > e.clientHeight + 1
          && /auto|scroll/.test(getComputedStyle(e).overflowY))
        .map(e => e.hasAttribute('data-comments-list') ? 'comments-list'
          : { tag: e.tagName, thread: e.closest('[data-thread]')?.dataset.thread, classes: e.className }));
      assert.deepEqual(scrollOwners, ['comments-list'], 'the long panel has ONE scrolling list, not nested thread scrollbars');
      await toggle(PRIMARY).click();
      assert.equal(await expanded(PRIMARY).count(), 0, 'collapse hides the conversation');
      assert.equal(await toggle(SIBLING).getAttribute('aria-expanded'), 'true', 'collapse is independent');
      await toggle(PRIMARY).click();
      assert.equal(await expanded(PRIMARY).getByPlaceholder('Reply…').inputValue(), 'A reply draft survives independent collapse and reload');
      assert.equal(await editArea().inputValue(), ownEditDraft, 'own-comment editing resumes with its draft after collapse/reopen');
      await capture('panel-draft-restored', panel());
      // About keeps both its quoted-text button and the small jump icon.
      const jump = expanded(PRIMARY).getByRole('button', { name: 'Show it in the doc', exact: true }).last();
      assert.equal(await jump.locator('svg').count(), 1, 'quote jump keeps its SVG icon');
      await jump.click(); await settle(page);
      if (sheet) {
        await panel().waitFor({ state: 'detached' });
        await capture('quote-jump');
        await openPanel();
        assert.equal(await editArea().inputValue(), ownEditDraft, 'own edit survives the sheet’s quote-jump dismissal');
      } else {
        await capture('quote-jump', panel());
      }
      if (count > 1) {
        const other = page.locator(`[data-tile-motion="${2 + (count - 1) * 2}"]`);
        await other.locator('button[data-opens="comments"]').first().click();
        const otherPanel = other.locator('[data-comments-panel]'); await otherPanel.waitFor();
        assert.equal(await otherPanel.locator('[data-thread]').count(), 0, 'another document does not inherit this document’s expanded rows');
        await capture('two-doc-panels', panel());
        await close(otherPanel, 'Close the comments');
      }
      // Reload is deliberate: api.rs receives Doc changes on /live, whereas
      // fixture.send targets the session /stream. Do not send a fake event to
      // the wrong channel and accidentally claim live invalidation coverage.
      await f.reload({ waitUntil: 'domcontentloaded' }); await anchor.waitFor(); await settle(page); await openPanel();
      assert.equal(await toggle(PRIMARY).getAttribute('aria-expanded'), 'true', 'primary expansion persists across browser reload');
      assert.equal(await toggle(SIBLING).getAttribute('aria-expanded'), 'true', 'second expansion persists independently');
      assert.equal(await toggle(LATER).getAttribute('aria-expanded'), 'false', 'unopened rows remain collapsed after reload');
      assert.equal(await expanded(PRIMARY).getByPlaceholder('Reply…').inputValue(), 'A reply draft survives independent collapse and reload', 'draft is restored after browser reload');
      assert.equal(await editArea().inputValue(), ownEditDraft, 'own-comment editing resumes with its draft after browser reload');
      await capture('panel-own-edit-reloaded', panel());
      await expanded(PRIMARY).getByRole('button', { name: 'Cancel', exact: true }).click();
      assert.equal(await editArea().count(), 0, 'explicit Cancel discards the own-comment edit draft');
      await capture('panel-reloaded', panel());
      if (asyncFlows) await lateEdit();
      await toggle(PRIMARY).click(); await toggle(SIBLING).click();
      const changed = states.get(DOC).find(t => t.id === LATER);
      changed.comments.push({ id: 'new-agent-reply', by: 'session:parent', at: 40, text: 'A new reply arrived while this thread stayed collapsed.' });
      changed.needs_you = true;
      await f.reload({ waitUntil: 'domcontentloaded' }); await anchor.waitFor(); await settle(page); await openPanel();
      assert.equal(await toggle(LATER).getAttribute('aria-expanded'), 'false', 'new reply does not auto-expand a collapsed thread');
      const marker = row(LATER).locator('[data-new-reply]');
      await marker.waitFor(); await marker.scrollIntoViewIfNeeded(); await capture('new-reply-collapsed', panel());
      assert.equal(await marker.count(), 1, 'collapsed row retains its marker until opened');
      await toggle(LATER).click(); await expanded(LATER).getByText('A new reply arrived while this thread stayed collapsed.', { exact: true }).waitFor();
      assert.equal(await row(LATER).locator('[data-new-reply]').count(), 0, 'opening the conversation clears its new-reply marker');
      await toggle(LATER).click();
      assert.equal(await row(LATER).locator('[data-new-reply]').count(), 0, 'read marker stays cleared after collapse');
      await capture('new-reply-read', panel());
      // A stored expanded row is not being read when its panel is closed.
      // Observe its read ID while closed, before opening could acknowledge it.
      await toggle(PRIMARY).click();
      const readBeforeClosed = await readPrefs(PRIMARY);
      assert(readBeforeClosed.expanded, 'primary is expanded before closing its panel');
      await close(panel(), 'Close the comments');
      const primary = states.get(DOC).find(t => t.id === PRIMARY);
      primary.comments.push({ id: 'agent-while-panel-closed-expanded', by: 'session:parent', at: 50,
        text: 'This reply arrived with the expanded panel closed.' });
      await reloadClosed();
      assert.deepEqual(await readPrefs(PRIMARY), readBeforeClosed, 'loading replies while an expanded panel is closed does not mark them read');
      await capture('reply-while-panel-closed');
      await openPanel();
      assert.equal(await toggle(PRIMARY).getAttribute('aria-expanded'), 'true', 'closed panel retains its expansion preference');
      await expanded(PRIMARY).getByText('This reply arrived with the expanded panel closed.', { exact: true }).waitFor();
      await toggle(PRIMARY).click(); await close(panel(), 'Close the comments');
      primary.comments.push({ id: 'agent-while-panel-closed-collapsed', by: 'session:parent', at: 60,
        text: 'Read this new reply through the block conversation.' });
      await reloadClosed(); await openPanel();
      assert.equal(await toggle(PRIMARY).getAttribute('aria-expanded'), 'false', 'closed collapsed panel stays collapsed when a reply arrives');
      const primaryMarker = row(PRIMARY).locator('[data-new-reply]');
      await primaryMarker.waitFor(); await primaryMarker.scrollIntoViewIfNeeded();
      await capture('reply-while-panel-closed-collapsed', panel());
      await close(panel(), 'Close the comments'); await openBlock();
      await pop().locator(`[data-thread="${PRIMARY}"]`).getByText('Read this new reply through the block conversation.', { exact: true }).waitFor();
      await capture('new-reply-read-in-pop', pop());
      await close(pop(), 'Close'); await openPanel();
      assert.equal(await toggle(PRIMARY).getAttribute('aria-expanded'), 'false', 'reading a popover does not expand its panel row');
      assert.equal(await row(PRIMARY).locator('[data-new-reply]').count(), 0, 'reading the block conversation acknowledges the panel’s new-reply marker');
      await capture('new-reply-read-via-pop', panel());
      const collapseAll = panel().getByRole('button', { name: 'Collapse all', exact: true });
      const expandAll = panel().getByRole('button', { name: 'Expand all', exact: true });
      assert.equal(await collapseAll.count(), 1, 'Collapse all is available');
      assert.equal(await expandAll.count(), 1, 'Expand all is available');
      await collapseAll.click();
      assert.equal(await panel().locator('[data-thread]').count(), 0, 'all rows start closed for the bulk toggle check');
      await expandAll.click(); assert.equal(await panel().locator('[data-thread]').count(), ordered.length, 'Expand all opens every conversation');
      await capture('panel-expand-all', panel()); await collapseAll.click();
      assert.equal(await panel().locator('[data-thread]').count(), 0, 'Collapse all closes every conversation');
      await toggle(LATER).click();
      await expanded(LATER).getByRole('button', { name: 'Resolve', exact: true }).click();
      await row(LATER).waitFor({ state: 'detached' });
      const resolves = requests.filter(r => r.method === 'POST' && r.path.endsWith('/resolve'));
      assert.deepEqual(resolves.map(r => r.path), [`/api/v1/docs/${DOC}/threads/${LATER}/resolve`], 'only the user’s Resolve action sends a resolve request');
      await panel().getByRole('button', { name: 'Resolved', exact: true }).click();
      await row(LATER).waitFor(); await capture('panel-resolved', panel());
      if (name === 'phone-small') {
        // Use a real narrower viewport, not CSS overrides: both forms must
        // wrap their actions instead of squeezing the switch's label.
        await close(panel(), 'Close the comments');
        await page.setViewportSize({ width: 240, height }); await settle(page);
        await openBlock();
        const wrappedActions = async names => {
          const agent = pop().getByRole('switch', { name: 'Send to agent', exact: true });
          const bounds = await rect(agent.locator('..').locator('..')), s = await rect(agent);
          contained(s, bounds, 'narrow agent switch stays within its action row');
          assert(Math.abs(s.x - bounds.x) < 1 && Math.abs(s.right - bounds.right) < 1,
            'narrow agent switch takes its own full-width line');
          const lines = await agent.locator('span').first().evaluate(el => {
            const range = document.createRange(); range.selectNodeContents(el);
            return range.getClientRects().length;
          });
          assert.equal(lines, 1, 'Send to agent label stays on one line');
          const actions = [];
          for (const name of names) {
            const button = pop().getByRole('button', { name, exact: true }), b = await rect(button);
            contained(b, bounds, `${name} stays within the wrapped action row`);
            assert(b.y >= s.bottom - 1, `${name} wraps below the switch`);
            actions.push(b);
          }
          if (actions.length === 2) {
            assert(Math.abs(actions[0].y - actions[1].y) < 1, 'Cancel and Comment stay together');
            assert(actions[0].right <= actions[1].x, 'Cancel and Comment do not overlap');
          }
          const was = await agent.getAttribute('aria-checked');
          await agent.click();
          assert.equal(await agent.getAttribute('aria-checked'), was === 'true' ? 'false' : 'true',
            'wrapped agent switch remains operable');
          await agent.click();
          await reachablePopControls('narrow-actions');
        };
        await wrappedActions(['Send now']); await capture('narrow-thread-actions', pop());
        await threadMenu('New thread on this block');
        await pop().getByPlaceholder('Your comment').fill('A narrow comment keeps all its actions reachable');
        await wrappedActions(['Cancel', 'Comment']); await capture('narrow-new-comment-actions', pop());
        await pop().getByRole('button', { name: 'Cancel', exact: true }).click();
      }
    }
    assert.equal(deferred.size, 0, 'all deferred API hooks received their intended requests');
    assert.equal(held.size, 0, 'all deferred API responses were explicitly released');
    assert.deepEqual(f.errors, [], 'no browser crashes or external requests');
    save(); console.log(`PASS ${label}`);
  } catch (error) {
    save();
    await f?.page?.screenshot({ path: path.join(evidence, `${label}-failure.png`) }).catch(() => {});
    console.error(`FAIL ${label} during ${f?.phase}`, f?.errors, f?.consoleMessages);
    throw error;
  } finally {
    for (const gate of held) gate.release();
    await f?.context?.close();
  }
}
async function main() {
  fs.mkdirSync(evidence, { recursive: true });
  const { server, origin } = await serve();
  try {
    for (const engine of process.env.ENGINE === 'both' ? ['chromium', 'webkit'] : [process.env.ENGINE || 'chromium']) {
      for (const name of (process.env.VIEWPORT || Object.keys(cases).join(',')).split(',')) {
        for (const theme of (process.env.THEME || 'light,dark').split(',')) {
          const browser = await launchBrowser(playwright, engine);
          try { await within(`${engine}-${name}-${theme}`, () => check(browser, origin, engine, name, theme)); }
          finally { await browser.close(); }
        }
      }
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
}
main().catch(error => { console.error(error); process.exitCode = 1; });
