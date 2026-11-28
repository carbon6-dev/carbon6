#!/usr/bin/env node
'use strict';

// Production WASM; private loopback fixtures only. All async boundaries are
// explicit response gates or rendered conditions, not sleeps or live workers.
const assert = require('node:assert/strict');
const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');
const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
const minute = 60000;

async function check(browser, origin, engine, width) {
  const base = Date.now() - 310 * 10 * minute;
  const inputs = new Map([[10, 'Oldest prompt\nComplete original body'], [70, 'Second prompt\nSearch-only NEEDLE'],
    [140, 'Yesterday prompt'], [210, 'Follow-up prompt'], [280, 'Latest prompt']]);
  const log = [{ seq: 1, ts: base, type: 'started', connection_id: 'test', model: 'test',
    cwd: '/tmp', system_prompt: '', tools: [], context_window: 128000 }];
  let turn = 0;
  for (let seq = 2; seq <= 310; seq++) {
    const ts = base + seq * 10 * minute;
    if (inputs.has(seq)) {
      turn = seq;
      log.push({ seq, ts, type: 'user', command_id: `input-${seq}`, text: inputs.get(seq),
        ...(seq === 140 ? { from_session_id: 'sender', from_title: 'Release helper' } : {}) });
    } else if (inputs.has(seq - 1)) log.push({ seq, ts, type: 'turn_started', turn, context_window: 128000 });
    else if (inputs.has(seq + 1) || seq === 310) log.push({ seq, ts, type: 'turn_ended', turn, reason: 'completed' });
    else log.push({ seq, ts, type: 'assistant', turn,
      blocks: [{ type: 'text', text: `Reply ${seq}. ${'Readable history fixture. '.repeat(22)}` }], stop_reason: 'end_turn' });
  }
  const prompts = [...inputs].map(([seq, text]) => ({ seq, ts: base + seq * 10 * minute, command_id: `input-${seq}`,
    text, first_line: text.split('\n')[0], origin: seq === 140 ? 'session' : seq === 210 ? 'follow_up' : 'user',
    from_session_id: seq === 140 ? 'sender' : '', from_title: seq === 140 ? 'Release helper' : '', provenance: null,
    queued: false, turn: seq, start: base + (seq + 1) * 10 * minute, end: base + (seq + 60) * 10 * minute,
    duration_ms: 59 * 10 * minute, reply: `Extracted reply for ${seq}.` }));
  const queued = { ...prompts.at(-1), seq: 0, ts: base + 311 * 10 * minute, command_id: 'queued', text: 'Queued follow-up',
    first_line: 'Queued follow-up', origin: 'follow_up', queued: true, turn: null, start: null, end: null, duration_ms: null, reply: '' };
  let generation = 0, hold = false, held, release;
  let arrivals = 0;
  const requests = [], older = [];
  const f = await fixture(browser, origin, { width, height: 1000 }, 'light',
    { choice: 'off', effective: 'off', os: 'no-preference' }, () => {}, { timezoneId: 'America/Los_Angeles' }, {
      instrumentation: false, ready: '#parent-e280',
      setup: async ({ context }) => {
        await context.routeWebSocket('**/sessions/*/stream?*', socket => {
          socket.send(JSON.stringify({ type: 'window', from: 140 }));
          for (const event of log.filter(e => e.seq === 1 || e.seq >= 140)) socket.send(JSON.stringify({ type: 'event', event }));
          socket.send(JSON.stringify({ type: 'synced' }));
        });
        await context.route('**/api/v1/sessions/parent/events?*', route => {
          const q = new URL(route.request().url()).searchParams;
          older.push(Object.fromEntries(q));
          const before = Number(q.get('before'));
          return route.fulfill({ json: log.filter(e => e.seq >= 10 && e.seq < before) });
        });
        await context.route('**/api/v1/sessions/parent/prompts?*', async route => {
          const q = new URL(route.request().url()).searchParams;
          requests.push(Object.fromEntries(q));
          if (hold) { held?.(); await new Promise(resolve => { release = resolve; }); }
          const after = Number(q.get('after_seq') || 0);
          const rows = prompts.filter(p => p.seq > after).slice(0, 2).map(p => ({ ...p,
            ...(generation && p.seq === 280 ? { first_line: 'Latest prompt refreshed', reply: 'Updated extracted reply.' } : {}) }));
          const more = rows.at(-1).seq < prompts.at(-1).seq;
          await route.fulfill({ json: { prompts: rows, queued: after ? [] : [{ ...queued, first_line: generation ? 'Queued follow-up refreshed' : queued.first_line }],
            first_ts: base, last_ts: base + 310 * 10 * minute,
            next_ts: more ? rows.at(-1).ts : null, next_seq: more ? rows.at(-1).seq : null } });
          arrivals++;
        });
      },
    });
  const { page } = f;
  try {
    const rail = page.getByRole('slider', { name: 'Session prompt history' });
    await rail.waitFor();
    await page.waitForFunction(() => document.querySelectorAll('.c6-history-mark').length > 0);
    assert(requests.some(q => q.after_seq === '70'), 'index pages independently of the loaded event window');
    assert(await page.locator('.c6-history-mark').count() <= 180);
    assert(await page.locator('.c6-history-tick').count() > 0, 'ruler labels calendar days and today hours');
    assert.equal(await page.locator('#parent-e10').count(), 0, 'oldest prompt starts outside the loaded window');
    if (width > 390) {
      const box = await rail.boundingBox();
      const x = box.x + box.width / 2, y = box.y + box.height * (10 / 310);
      await page.mouse.move(x, y);
      await page.getByRole('tooltip').filter({ hasText: 'Oldest prompt' }).waitFor();
      assert.match(await page.getByRole('tooltip').innerText(), /ran/);
      await page.mouse.click(x, y);
      await page.locator('#parent-e10.c6-flash').waitFor();
      assert(older.some(q => q.at === '10'), 'jump uses the shared older-events loader');
      // Keyboard navigation is native and platform-independent on macOS/Linux.
      await rail.focus();
      await rail.press('ArrowDown');
      await page.locator('#parent-e70.c6-flash').waitFor();
      await rail.press('End');
      await page.locator('#parent-e280.c6-flash').waitFor();
    } else {
      await rail.tap();
      const sheet = page.getByRole('dialog', { name: 'Your prompts', exact: true });
      await sheet.waitFor();
      await sheet.locator('[data-prompt-seq="280"]').getByRole('button', { name: 'Jump to it', exact: true }).click();
      await page.locator('#parent-e280.c6-flash').waitFor();
      await sheet.waitFor({ state: 'hidden' });
      await rail.tap();
      await sheet.waitFor();
    }
    // Wide glance, narrower desktop popover, or the phone's escaped sheet.
    let list = page.getByRole('region', { name: 'Your prompts', exact: true }).filter({ visible: true }).first();
    if (!await list.isVisible()) {
      await page.getByRole('button', { name: /Open session sheet|Open session glance/ }).first().click();
      await list.waitFor();
    }
    assert.match(await list.innerText(), /Up next/);
    assert.match(await list.innerText(), /From Release helper/);
    const toggle = list.getByRole('checkbox', { name: "Show reply's first sentence" });
    assert.equal(await toggle.isChecked(), false);
    assert.equal(await list.getByText('Extracted reply for 280.', { exact: true }).isVisible(), false);
    await toggle.check();
    await list.getByText('Extracted reply for 280.', { exact: true }).waitFor();
    await list.getByRole('searchbox', { name: 'Filter your prompts' }).fill('needle');
    assert.equal(await list.locator('[data-prompt-seq]').count(), 1, 'filter matches complete prompt text only');
    const copy = list.getByRole('button', { name: 'Copy prompt', exact: true });
    assert.equal(JSON.parse(await copy.getAttribute('data-copy-json')), inputs.get(70), 'shared Copy receives the complete source');
    await list.getByRole('searchbox', { name: 'Filter your prompts' }).fill('');
    if (width > 390) {
      // Refresh while deliberately away from the bottom. Record every frame,
      // not just the final geometry (the Machines scroll regression's contract).
      await page.locator('#parent-e210').scrollIntoViewIfNeeded();
      await settle(page);
      await page.evaluate(() => {
        const prompt = document.querySelector('#parent-e210');
        let scroller = prompt.parentElement;
        while (scroller && !/auto|scroll/.test(getComputedStyle(scroller).overflowY)) scroller = scroller.parentElement;
        if (!scroller) throw new Error('No transcript scroller');
        const snapshot = window.__historySnapshot = { prompt, scroller, top: scroller.scrollTop, height: scroller.scrollHeight,
          samples: [], watching: true };
        const frame = () => { if (!snapshot.watching) return;
          snapshot.samples.push([scroller.scrollTop, scroller.scrollHeight]); requestAnimationFrame(frame); };
        requestAnimationFrame(frame);
      });
      hold = true;
      const waiting = new Promise(resolve => { held = resolve; });
      await list.getByRole('button', { name: 'Refresh prompts', exact: true }).click();
      await waiting;
      await page.evaluate(async () => { for (let i = 0; i < 3; i++) await new Promise(requestAnimationFrame); });
      generation++;
      hold = false;
      release();
      await list.getByText('Latest prompt refreshed', { exact: true }).waitFor();
      await list.getByText('Queued follow-up refreshed', { exact: true }).waitFor();
      await settle(page);
      const stable = await page.evaluate(() => {
        const s = window.__historySnapshot; s.watching = false;
        return { same: document.querySelector('#parent-e210') === s.prompt, top: s.top, height: s.height, samples: s.samples };
      });
      assert(stable.same, 'refresh retains transcript DOM and owner');
      assert(stable.samples.length >= 3);
      for (const [top, height] of stable.samples) {
        assert(Math.abs(top - stable.top) <= 1, `refresh moved scrollTop ${stable.top} -> ${top}`);
        assert.equal(height, stable.height, 'refresh cannot alter transcript scroll height');
      }
    } else {
      await list.getByRole('searchbox', { name: 'Filter your prompts' }).fill('Oldest');
      await list.getByRole('button', { name: 'Jump to it', exact: true }).click();
      await page.locator('#parent-e10.c6-flash').waitFor();
      assert(older.some(q => q.at === '10'), 'phone sheet also loads older events');
    }
    assert(arrivals >= 3);
    assert.deepEqual(f.errors, []);
    console.log(`PASS ${engine} history ${width}: paged index, preview/jump/flash, prompt-only filter, copy, refresh/phone`);
  } finally {
    release?.();
    await page.evaluate(() => { if (window.__historySnapshot) window.__historySnapshot.watching = false; }).catch(() => {});
    await f.context.close();
  }
}

if (require.main === module) (async () => {
  const engines = !process.env.ENGINE ? ['chromium'] : process.env.ENGINE === 'both' ? ['chromium', 'webkit'] : process.env.ENGINE.split(',');
  assert(engines.every(e => ['chromium', 'webkit'].includes(e)));
  const { server, origin } = await serve();
  try {
    for (const engine of engines) {
      const browser = await launchBrowser(playwright, engine);
      try { for (const width of [1920, 390]) await within(`${engine} session history ${width}`, () => check(browser, origin, engine, width)); }
      finally { await browser.close(); }
    }
  } finally { server.close(); }
})().catch(error => { console.error(error); process.exitCode = 1; });
