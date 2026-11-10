#!/usr/bin/env node
'use strict';

// Production WASM, real saved workspace/reload, private fake head. The head's
// stream contract includes the complete suffix even when opened at an anchor.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, within, launchBrowser, session } = require('./motion.cjs');
const output = process.env.EVIDENCE || path.join(require('node:os').homedir(), '.cache/c6-helper/fix-stale-transcript/stale-transcript');
const motion = { choice: 'off', effective: 'off', os: 'no-preference' };
const query = 'needle';
const anchor = 'parent-e101';
const decision = { id: 41, realm: 'home', kind: 'question', title: 'Pending fixture decision',
  source: 'session:parent', priority: 'normal', state: 'open', created_at: 1, updated_at: 1 };
const deferred = () => {
  let resolve;
  const promise = new Promise(r => { resolve = r; });
  return { promise, resolve };
};
const log = Array.from({ length: 1600 }, (_, i) => {
  const seq = i + 1;
  if (seq === 1) return { seq, ts: seq, type: 'started', connection_id: 'test', model: 'test',
    cwd: '/tmp', system_prompt: '', tools: [], context_window: 128000 };
  if (seq % 10 === 1) return { seq, ts: seq, type: 'user', command_id: `user-${seq}`, text: `Question ${seq}` };
  return { seq, ts: seq, type: 'assistant', turn: seq,
    blocks: [{ type: 'text', text: `Transcript reply ${seq}. ${seq === 102 ? `${query} [Open this session](/sessions/parent)` : 'A readable reply.'}` }], stop_reason: 'end_turn' };
});
const layout = { tiles: [{ key: 2, columns: [{ key: 3, view: { view: 'session', id: 'parent' } }],
  cursor: 0, active: 3, used: 3, back: [], forward: [], stack: 0, minimized: false }], focus: 0, next: 4, full: false };
const savedView = page => page.evaluate(() => JSON.parse(localStorage.getItem('c6.home.workspace.v2')).tiles[0].columns[0].view);
const end = (page, seq = 1200) => page.locator(`[id="parent-e${seq}"]`);
async function atEnd(page, seq = 1200) {
  await end(page, seq).waitFor();
  await settle(page);
  assert(await end(page, seq).evaluate(el => {
    const box = el.closest('[data-follow-content]').parentElement.parentElement;
    const r = el.getBoundingClientRect(), s = box.getBoundingClientRect();
    return r.top >= s.top && r.bottom <= s.bottom + 1;
  }), 'the actual latest reply is inside the transcript viewport');
}

async function check(browser, origin, phone, label, mode) {
  let f, socket, endSeq = 1200;
  let olderGate, matchesGate;
  const streams = [], pages = [];
  const setup = async ({ page, context }) => {
    await context.routeWebSocket('**/sessions/*/stream?*', ws => {
      socket = ws;
      const q = new URL(ws.url()).searchParams, after = Number(q.get('after'));
      const from = after ? after + 1 : Math.min(endSeq - 299, Number(q.get('at') || Infinity));
      streams.push({ after, from, end: endSeq });
      if (!after) {
        ws.send(JSON.stringify({ type: 'window', from }));
        ws.send(JSON.stringify({ type: 'event', event: log[0] }));
      }
      for (const event of log.filter(e => e.seq >= from && e.seq <= endSeq)) ws.send(JSON.stringify({ type: 'event', event }));
      ws.send(JSON.stringify({ type: 'synced' }));
    });
    await page.route('**/api/v1/sessions/parent/events?*', async route => {
      const q = new URL(route.request().url()).searchParams;
      pages.push(Object.fromEntries(q));
      const before = Number(q.get('before'));
      const from = Math.max(1, Math.min(before - Number(q.get('limit') || 300), Number(q.get('at') || Infinity)));
      if (olderGate) { olderGate.started.resolve(); await olderGate.release.promise; }
      return route.fulfill({ json: log.filter(e => e.seq >= from && e.seq < before) });
    });
    await page.route('**/api/v1/sessions/parent/decisions', route => route.fulfill({ json: [decision] }));
    await page.route('**/api/v1/sessions/recent', route => route.fulfill({ json: [{ session_id: 'parent', at: 2 }] }));
    await page.route('**/api/v1/sessions/parent/matches?*', async route => {
      if (matchesGate) { matchesGate.started.resolve(); await matchesGate.release.promise; }
      return route.fulfill({ json: {
        matches: [{ anchor, at: 101, seq: 101, open: [], kind: 'user' }], terms: [query], ignored: [], truncated: false,
      } });
    });
    await page.route('**/api/v1/search?*', route => route.fulfill({ json: { results: [{ session: session(), worker_name: 'Fixture',
      hits: [{ anchor, seq: 101, part: 0, kind: 'user', ts: 101, snippet: [{ text: query, hit: true }] }], total_hits: 1 }],
      terms: [query], ignored: [], truncated: false } }));
  };
  try {
    f = await fixture(browser, origin, phone ? { width: 390, height: 844 } : { width: 1440, height: 1000 },
      'light', motion, value => { f = value; }, {}, { layout, setup });
    const { page } = f;
    await f.setState('working', { last_seq: endSeq });
    const step = name => { f.phase = name; console.log(`  ${label}: ${name}`); };
    step('ordinary picker selection');
    if (phone) {
      await page.getByRole('button', { name: /^Switch sessions and pages/ }).click();
      await page.getByRole('dialog', { name: 'Sessions', exact: true }).getByRole('button', { name: /Motion fixture parent/ }).click();
    }
    await atEnd(page);
    assert(!((await savedView(page)).target), 'ordinary selection saves no historical target');
    step('deliberate saved-message jump');
    if (phone) {
      await page.getByRole('button', { name: /^Switch sessions and pages/ }).click();
      const picker = page.getByRole('dialog', { name: 'Sessions', exact: true });
      await picker.getByRole('searchbox', { name: 'Search', exact: true }).fill(query);
      await picker.getByText(query, { exact: true }).waitFor();
      await picker.getByRole('button', { name: /Motion fixture parent/ }).click();
    } else {
      await page.goto(`${origin}/r/home/sessions/parent?q=${query}#${anchor}`, { waitUntil: 'domcontentloaded' });
    }
    await page.locator(`[id="${anchor}"]`).waitFor();
    await settle(page);
    assert((await savedView(page)).target.anchor === anchor, 'a deliberate hit is saved as a historical target');
    step('reload saved historical tile');
    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.locator(`[id="${anchor}"]`).waitFor();
    await settle(page);
    assert.equal(await end(page).count(), 0, 'the historical window, not the full suffix, is drawn');
    assert.equal(streams.at(-1).end, 1200, 'the fake head delivered every later event (no artificial missing suffix)');
    assert.equal(await page.locator('#decision-41').count(), 0, 'a decision must not masquerade as the end of an old window');
    step('an empty reconnect preserves a deliberate historical window');
    socket.close();
    const disconnected = page.getByText('Session stream disconnected. Reconnecting…', { exact: true });
    await disconnected.waitFor();
    await disconnected.waitFor({ state: 'hidden' });
    await settle(page);
    assert.equal(streams.at(-1).after, 1200);
    assert.equal(await end(page).count(), 0, 'reconnect must not silently abandon a deliberate jump');
    const composer = page.locator('textarea').first();
    const keptComposer = await composer.elementHandle();
    const streamCount = streams.length;
    await composer.fill('Keep my unsent draft across Latest and picker navigation');
    if (mode === 'return') {
      step('normal navigation back to the retained historical session');
      if (phone) {
        await page.getByRole('button', { name: /^Switch sessions and pages/ }).click();
        const picker = page.getByRole('dialog', { name: 'Sessions', exact: true });
        await picker.getByRole('button', { name: /Motion fixture parent/ }).click();
      } else {
        await page.locator('[id="parent-e102"]').getByRole('link', { name: 'Open this session', exact: true }).click();
      }
    } else {
      // An in-session Find request is a one-shot navigation, not an invariant
      // allowed to reassert its historical window whenever Latest trims it.
      step('find the historical match again');
      const find = page.getByPlaceholder('Find in this session', { exact: true });
      await page.getByText('1 of 1', { exact: true }).waitFor();
      await find.press('Enter');
      await settle(page);
      await page.locator('[data-follow-content]').evaluate(el => { el.parentElement.parentElement.scrollTop = 200; });
      step('Latest after reload and Find');
      await page.getByTitle('To the latest (End)', { exact: true }).click();
      step('Latest clicked');
    }
    await atEnd(page);
    assert(await composer.evaluate((el, kept) => el === kept, keptComposer), 'Latest/navigation retain the existing composer');
    assert.equal(await composer.inputValue(), 'Keep my unsent draft across Latest and picker navigation');
    assert.equal(streams.length, streamCount, 'consuming a target retains the existing session stream');
    assert(!((await savedView(page)).target), 'Latest consumes the saved historical target');
    assert.equal(new URL(page.url()).hash, '', 'Latest clears the historical URL');
    step('reconnect with more than a window of missed messages');
    endSeq = 1600;
    await f.setState('working', { last_seq: endSeq });
    socket.close();
    await atEnd(page, endSeq);
    assert.equal(streams.at(-1).after, 1200, 'reconnect fetches the entire missing suffix from the last received event');
    // IDs are the production anchors, not test-only hooks. Every expected
    // reply in the visible tail must be present; do not just assert its end.
    const seqs = await page.locator('[id^="parent-e"]').evaluateAll(es => es.map(e => Number(e.id.slice('parent-e'.length))).sort((a, b) => a - b));
    assert.deepEqual(seqs, Array.from({ length: 300 }, (_, i) => 1301 + i), 'the complete 300-event tail has no gap');
    assert(await page.locator('#decision-41').evaluate(el => {
      const rows = [...el.closest('[data-follow-content]').querySelectorAll('[id^="parent-e"]')];
      return !!rows.length && (rows.at(-1).compareDocumentPosition(el) & Node.DOCUMENT_POSITION_FOLLOWING) !== 0;
    }), 'the pending decision belongs after the actual latest reply');
    if (mode === 'latest') {
      for (const kind of ['older', 'matches']) {
        step(`Latest supersedes a delayed ${kind} response`);
        const gate = { started: deferred(), release: deferred() };
        if (kind === 'older') olderGate = gate; else matchesGate = gate;
        const modifier = await page.evaluate(() => navigator.platform.startsWith('Mac') ? 'Meta' : 'Control');
        await page.keyboard.press(`${modifier}+f`);
        await gate.started.promise;
        step(`delayed ${kind} request is gated`);
        // A programmatic offset must not stop following. Use the production
        // keyboard intent path first (mobile WebKit has no mouse wheel), then
        // leave more than a viewport so the floating Latest button is shown.
        await page.locator('[data-follow-content]').evaluate(el => el.parentElement.parentElement.focus());
        await page.keyboard.press('PageUp');
        // Let native keyboard scrolling finish before clicking: continuing a
        // gesture through the tap is outside this cancellation regression.
        await page.locator('[data-follow-content]').evaluate(el => new Promise(resolve => {
          const box = el.parentElement.parentElement;
          let previous = box.scrollTop, stable = 0;
          const frame = () => {
            const top = box.scrollTop;
            stable = top === previous ? stable + 1 : 0;
            previous = top;
            if (stable >= 8) resolve(); else requestAnimationFrame(frame);
          };
          requestAnimationFrame(frame);
        }));
        await page.locator('[data-follow-content]').evaluate(el => { el.parentElement.parentElement.scrollTop -= 2000; });
        await page.getByTitle('To the latest (End)', { exact: true }).click();
        step(`Latest clicked with delayed ${kind} request`);
        const response = page.waitForResponse(r => r.url().includes(`/parent/${kind === 'older' ? 'events' : 'matches'}?`));
        gate.release.resolve();
        await response;
        step(`delayed ${kind} response arrived`);
        await atEnd(page, endSeq);
        assert.equal(await page.locator(`[id="${anchor}"]`).count(), 0, 'an obsolete reply cannot reinstate the historical window');
        olderGate = matchesGate = undefined;
      }
    }
    step('reload after Latest');
    await page.reload({ waitUntil: 'domcontentloaded' });
    await atEnd(page, endSeq);
    assert.equal(streams.at(-1).from, 1301, 'reload starts with the real tail, not the old anchor');
    assert.deepEqual(f.errors, []);
    fs.mkdirSync(output, { recursive: true });
    await page.screenshot({ path: path.join(output, `${label}.png`) });
  } finally {
    fs.mkdirSync(output, { recursive: true });
    const state = await f?.page.evaluate(() => ({ url: location.href,
      layout: JSON.parse(localStorage.getItem('c6.home.workspace.v2')),
      drawn: [...document.querySelectorAll('[id^="parent-e"]')].map(el => el.id),
      decision: !!document.getElementById('decision-41'),
    })).catch(error => ({ diagnosticError: error.message }));
    fs.writeFileSync(path.join(output, `${label}.json`), JSON.stringify({ streams, pages, phase: f?.phase, errors: f?.errors, state }, null, 2));
    await f?.page.screenshot({ path: path.join(output, `${label}-last.png`) }).catch(error => console.error('SCREENSHOT', error.message));
    await f?.context.close();
  }
}

async function main() {
  const playwright = require('playwright-core');
  const { server, origin } = await serve();
  const failures = [];
  try {
    for (const engine of (process.env.ENGINE || 'chromium,webkit').split(',')) for (const phone of [true, false]) for (const mode of ['latest', 'return']) {
      const label = `${engine}-${phone ? 'phone' : 'desktop'}-${mode}`;
      if (process.env.CASE && !label.includes(process.env.CASE)) continue;
      const browser = await launchBrowser(playwright, engine);
      try { await within(label, () => check(browser, origin, phone, label, mode)); console.log(`PASS ${label}`); }
      catch (error) { failures.push(label); console.error(`FAIL ${label}`, error.stack || error); }
      finally { await browser.close(); }
    }
    assert.deepEqual(failures, [], 'all stale transcript cases must pass');
  } finally { await new Promise(resolve => server.close(resolve)); }
}
main().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
