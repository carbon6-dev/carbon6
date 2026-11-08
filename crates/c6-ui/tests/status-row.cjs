#!/usr/bin/env node
'use strict';

// Production UI, private loopback APIs only. BEFORE records the same inputs
// without asking the old layout to satisfy the new fixed-place contract.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const playwright = require('playwright-core');
const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');
const before = process.env.BEFORE === '1';
const shots = process.env.SCREENSHOTS || '/tmp/s12-row';
const views = {
  wide: { width: 1600, height: 1000 },
  narrow: { width: 1600, height: 1000 },
  'phone-320': { width: 320, height: 740 },
  'phone-360': { width: 360, height: 800 },
  'phone-390': { width: 390, height: 844 },
  'phone-430': { width: 430, height: 932 },
};

async function check(browser, origin, engine, theme, view) {
  let f;
  const name = `${engine}-${theme}-${view}`;
  const phone = view.startsWith('phone');
  try {
    const now = Date.now();
    f = await fixture(browser, origin, views[view], theme,
      { choice: 'off', effective: 'off', os: 'reduce' }, progress => { f = progress; }, {}, {
        instrumentation: false,
        setup: async ({ context }) => context.addInitScript(now => {
          window.__statusNow = now;
          Date.now = () => window.__statusNow;
        }, now),
      });
    const { page } = f;
    // Keep a desktop viewport but constrain the actual tile/reading column.
    if (view === 'narrow') {
      await page.addStyleTag({ content: '[data-tile-motion] { width: 338px !important; }' });
      await settle(page);
    }
    f.send({ type: 'turn_started', turn: 99, ts: now - 12000, context_window: 1000000 });
    for (let i = 0; i < 3; i++) f.send({ type: 'usage', model: 'test', input_tokens: i === 2 ? 322400 : 0, output_tokens: i === 2 ? 32600 : 0,
      cache_read_tokens: 0, cache_write_tokens: 0, reasoning_tokens: 0, context_window: 1000000 });
    const row = page.locator(phone ? '[data-phone-status]' : '[data-transcript-status]');
    const task = index => ({ type: 'background_task', task_id: `row-task-${index}`, kind: 'shell',
      description: `Background command ${index}`, state: 'started', exit_code: 0,
      elapsed_ms: 0, notify: true, native_type: '', foreground: false });
    const geometry = () => page.evaluate(phone => {
      const row = document.querySelector(phone ? '[data-phone-status-slot]' : '[data-transcript-status]');
      const visible = document.querySelector(phone ? '[data-phone-status]' : '[data-transcript-status]');
      const rect = el => { if (!el) return null; const r = el.getBoundingClientRect(); return { x: r.x, y: r.y, width: r.width, height: r.height }; };
      const tasks = visible?.querySelector('[data-status-tasks]') || (phone
        ? [...visible?.querySelectorAll('span') || []].find(el => /^\d+ background tasks?$/.test(el.textContent.trim()))
        : visible?.querySelector('button'));
      const stats = visible?.querySelector('[data-turn-stats]');
      let elapsedWidth = null;
      if (stats) {
        const css = getComputedStyle(stats), canvas = document.createElement('canvas').getContext('2d');
        canvas.font = `${css.fontWeight} ${css.fontSize} ${css.fontFamily}`;
        elapsedWidth = canvas.measureText(`${stats.textContent.split(' · ')[0]}…`).width;
      }
      const label = visible?.querySelector('[data-working-label]');
      const scroller = [...document.querySelectorAll('div')].filter(el => el.clientHeight > 100 &&
        el.scrollHeight > el.clientHeight + 300 && getComputedStyle(el).overflowY === 'auto')
        .sort((a, b) => b.scrollHeight - a.scrollHeight)[0];
      return { row: rect(row || visible?.parentElement?.parentElement), tasks: rect(tasks), stats: rect(stats),
        label: rect(label), text: visible?.textContent, statsText: stats?.textContent,
        statsWidth: stats?.scrollWidth, elapsedWidth, composer: rect(document.querySelector('textarea')),
        transcript: rect(scroller), scrollTop: scroller?.scrollTop,
        overflow: document.documentElement.scrollWidth > innerWidth };
    }, phone);
    let zeroIdle;
    for (const count of [0, 1, 12]) {
      if (count === 1) f.send(task(0));
      if (count === 12) for (let i = 1; i < 12; i++) f.send(task(i));
      if (!before && count) await row.getByText(`${count} background task${count === 1 ? '' : 's'}`, { exact: true }).waitFor();
      const states = {};
      for (const state of ['working', 'idle']) {
        if (!before) await page.evaluate(phone => {
          const samples = window.__statusSamples = [];
          window.__sampleStatus = true;
          const sample = () => {
            if (!window.__sampleStatus) return;
            const slot = document.querySelector(phone ? '[data-phone-status-slot]' : '[data-transcript-status]');
            const tasks = document.querySelector(`${phone ? '[data-phone-status]' : '[data-transcript-status]'} [data-status-tasks]`);
            samples.push({ height: slot.getBoundingClientRect().height, x: tasks?.getBoundingClientRect().x ?? null });
            requestAnimationFrame(sample);
          };
          requestAnimationFrame(sample);
        }, phone);
        f.stream({ type: 'activity', running: state === 'working', status: '' });
        await f.setState(state);
        if (state === 'working') await row.getByText('Working…', { exact: true }).waitFor();
        else await page.waitForFunction(phone => !document.querySelector(phone ? '[data-phone-status]' : '[data-transcript-status]')?.textContent.includes('Working…'), phone);
        if (count && (!before || view !== 'wide')) await row.getByText(`${count} background task${count === 1 ? '' : 's'}`, { exact: true }).waitFor();
        await settle(page);
        states[state] = await geometry();
        await page.screenshot({ path: path.join(shots, `${before ? 'before' : 'after'}-${name}-${count}-${state}.png`) });
        if (!before) {
          const g = states[state];
          const samples = await page.evaluate(() => { window.__sampleStatus = false; return window.__statusSamples; });
          assert(samples.length > 0, `${name}: sampled rendered transition frames`);
          assert(samples.every(s => s.height === g.row.height && (!count || s.x === g.tasks.x)),
            `${name}: no transient row height/task x change: ${JSON.stringify(samples)}`);
          assert(!g.overflow, `${name}: no horizontal overflow`);
          assert.equal(g.row.height, phone ? 44 : 24, `${name}: fixed row height`);
          if (state === 'working') {
            assert.match(g.statsText, /^12s · 3 model calls ·/, `${name}: live turn_line, including elapsed time`);
            assert.match(g.statsText, /322\.4k in.*context 355\.0k/, `${name}: live tokens and context are not an elapsed-only substitute`);
            assert(g.stats.width > 0, `${name}: stats never disappear`);
            assert.equal(await row.locator('[role="status"] [data-turn-stats]').count(), 0,
              `${name}: ticking elapsed stats must not repeatedly announce the whole status row`);
            if (view === 'phone-320' && !count) assert(g.stats.width > 100, `${name}: without tasks, stats use the available space`);
            if (count) {
              assert.equal(g.label.y + g.label.height / 2, g.tasks.y + g.tasks.height / 2, `${name}: single baseline`);
              assert.equal(g.stats.y + g.stats.height / 2, g.tasks.y + g.tasks.height / 2, `${name}: stats share the same line`);
              assert(g.tasks.height <= 17, `${name}: task label never wraps`);
              assert(g.stats.x + g.stats.width <= g.tasks.x, `${name}: stats yield before tasks`);
              if (phone || view === 'narrow') assert(g.statsWidth > g.stats.width, `${name}: stats ellipsize first`);
              if (view === 'phone-320') {
                assert(g.stats.width >= g.elapsedWidth && g.stats.width <= g.elapsedWidth + 1 + 1 / 64,
                  `${name}: the complete elapsed-time prefix plus ellipsis fits, including fractional font rounding`);
              }
            }
            const full = await row.locator('[data-turn-stats]').getAttribute('title');
            assert(full.includes(g.statsText), `${name}: full stats on hover`);
            if (!phone) {
              await row.locator('[data-turn-stats]').focus();
              assert.equal(await row.locator('[data-turn-stats]').getAttribute('aria-label'), full, 'full stats on focus');
            } else assert((await row.getByRole('button').getAttribute('title')).includes(g.statsText), 'whole-row opener exposes full stats');
          } else assert(!g.stats, `${name}: idle left is empty`);
        }
      }
      if (!before) {
        const { working: w, idle: i } = states;
        assert.deepEqual(w.row, i.row, `${name}/${count}: status bounds never change`);
        assert.deepEqual(w.composer, i.composer, `${name}/${count}: composer never shifts`);
        assert.deepEqual(w.transcript, i.transcript, `${name}/${count}: transcript viewport never shifts`);
        assert.equal(w.scrollTop, i.scrollTop, `${name}/${count}: transcript position never shifts`);
        if (count) assert.deepEqual(w.tasks, i.tasks, `${name}/${count}: tasks keep exactly the same x/y/size`);
        if (!count) zeroIdle = i.row;
        else assert.deepEqual(i.row, zeroIdle, `${name}: task arrivals do not change row height`);
      }
      fs.writeFileSync(path.join(shots, `${before ? 'before' : 'after'}-${name}-${count}.json`), JSON.stringify(states, null, 2));
    }
    if (!before) {
      const idle = await geometry();
      const status = 'Waiting to retry a model call after a temporary connection failure';
      f.stream({ type: 'activity', running: true, status });
      await f.setState('working');
      await row.getByText(status, { exact: true }).waitFor();
      await settle(page);
      const retry = await geometry();
      assert.deepEqual(retry.tasks, idle.tasks, `${name}: long stream status cannot move/wrap tasks`);
      assert.equal(retry.row.height, idle.row.height);
      assert(retry.stats.width > 0, `${name}: long stream status still leaves live stats`);
      await page.screenshot({ path: path.join(shots, `after-${name}-stream-status.png`) });
      const retainedOpener = await row.getByRole('button').elementHandle();
      await retainedOpener.focus();
      await page.evaluate(() => { window.__statusNow += 1000; });
      await page.waitForFunction(phone => document.querySelector(`${phone ? '[data-phone-status]' : '[data-transcript-status]'} [data-turn-stats]`)?.textContent.startsWith('13s'), phone);
      assert(await retainedOpener.evaluate(el => el.isConnected && el === document.activeElement),
        `${name}: elapsed ticks do not remount the opener or lose focus`);
      f.stream({ type: 'activity', running: false, status: '' });
      await f.setState('idle');
      await page.waitForFunction(phone => !document.querySelector(phone ? '[data-phone-status]' : '[data-transcript-status]')?.querySelector('[data-turn-stats]'), phone);
      // The same opener works while idle; on phones its entire 44px surface is
      // the button, including the otherwise empty left side.
      const opener = row.getByRole('button');
      if (phone) {
        const bounds = await opener.boundingBox();
        assert.equal(bounds.height, 44);
        assert.equal(bounds.width, views[view].width);
        await opener.click({ position: { x: 8, y: 22 } });
      } else await opener.click();
      if (phone) await page.getByRole('heading', { name: 'Session status', exact: true }).waitFor();
      else await page.locator('[data-session-sheet]:visible').waitFor();
      await page.keyboard.press('Escape');
    }
    assert.deepEqual(f.errors, [], `${name}: browser errors`);
    console.log(`PASS ${name}${before ? ' (baseline capture)' : ''}`);
  } finally { if (f) await f.context.close(); }
}

async function main() {
  fs.mkdirSync(shots, { recursive: true });
  const { server, origin } = await serve();
  try {
    for (const engine of (!process.env.ENGINE || process.env.ENGINE === 'both' ? ['chromium', 'webkit'] : process.env.ENGINE.split(','))) {
      const browser = await launchBrowser(playwright, engine);
      try {
        for (const theme of ['light', 'dark']) for (const view of Object.keys(views))
          await within(`${engine}/${theme}/${view}`, () => check(browser, origin, engine, theme, view));
      } finally { await browser.close(); }
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
}
main().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
