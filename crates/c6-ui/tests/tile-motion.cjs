#!/usr/bin/env node
'use strict';

// Real production workspace, six long-content tiles. BEFORE=1 records the
// pinned build; final assertions inspect native WAAPI effects, not mock motion.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, launchBrowser } = require('./motion.cjs');
const { install } = require('./narrow-fixtures.cjs');
const shots = process.env.SCREENSHOTS || '/tmp/m2-screenshots';
const before = process.env.BEFORE === '1';
const phase = before ? 'before' : 'after';

async function seed(f, columns) {
  await install(f.context);
  await f.page.addInitScript(() => {
    window.__startupShifts = [];
    new PerformanceObserver(list => {
      for (const e of list.getEntries()) __startupShifts.push({ time: e.startTime, value: e.value, recent: e.hadRecentInput,
        sources: e.sources.map(s => ({ node: s.node?.tagName, class: String(s.node?.className).slice(0, 160),
          ghost: !!s.node?.closest('body > [inert][aria-hidden="true"]'),
          text: s.node?.textContent?.slice(0, 60), before: s.previousRect.toJSON(), after: s.currentRect.toJSON() })) });
    }).observe({ type: 'layout-shift', buffered: true });
  });
  await f.page.evaluate(columns => {
    const views = [{ view: 'session', id: 'parent' }, { view: 'settings', section: 'appearance' },
      { view: 'docs', id: 'narrow-doc' }, { view: 'workers' }, { view: 'sessions' }, { view: 'docs', id: '' }];
    const tiles = views.map((view, i) => ({ key: i + 10, columns: [{ key: i + 20, view }],
      cursor: 0, active: i + 20, used: i + 20, back: [], forward: [], stack: i % columns, minimized: false }));
    localStorage.setItem('c6.home.workspace.v2', JSON.stringify({ tiles, focus: 0, next: 40, full: false }));
  }, columns);
  await f.page.goto(new URL('/r/home/sessions/parent', f.page.url()).href);
  await f.page.locator('[data-tile-motion="15"]').waitFor();
  await f.page.locator('textarea').first().waitFor();
  await settle(f.page);
  await f.page.evaluate(() => {
    window.__shifts = [];
    window.__shiftObserver = new PerformanceObserver(list => {
      for (const e of list.getEntries()) __shifts.push({ time: e.startTime, value: e.value, recent: e.hadRecentInput,
        sources: e.sources.map(s => ({ node: s.node?.tagName, class: String(s.node?.className).slice(0, 160),
          ghost: !!s.node?.closest('body > [inert][aria-hidden="true"]'),
          text: s.node?.textContent?.slice(0, 60), before: s.previousRect.toJSON(), after: s.currentRect.toJSON() })) });
    });
    __shiftObserver.observe({ type: 'layout-shift' });
    window.__tileCapture = [];
    const animate = Element.prototype.animate;
    Element.prototype.animate = function (...args) {
      const animation = animate.apply(this, args);
      if (window.__pauseTiles && (this.matches('[data-tile-motion], [data-pane-motion], [data-tile-reveal]') ||
          this.matches('[inert][aria-hidden="true"]'))) {
        animation.pause(); animation.currentTime = 0; __tileCapture.push(animation);
      }
      return animation;
    };
  });
}

async function capture(page, label, action, mode) {
  await page.evaluate(() => { __tileCapture = []; __pauseTiles = true; });
  await action();
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  const effects = await page.evaluate(() => __tileCapture.map(a => ({
    target: a.effect.target.outerHTML.slice(0, 190), frames: a.effect.getKeyframes(), duration: a.effect.getTiming().duration })));
  if (!before) {
    assert.equal(await page.locator('body > [inert][aria-hidden="true"]').count(), 0, 'no cloned outgoing tile over the destination');
    if (mode === 'off') assert.equal(effects.length, 0, 'Off applies layout without any effect');
    else assert(effects.length, `${label} must exercise production animation`);
    for (const effect of effects) for (const frame of effect.frames) {
      assert(!frame.transform || /^(none|translate\([^)]*\))$/.test(frame.transform), `tile contents never scale: ${JSON.stringify(effect)}`);
      if ('opacity' in frame) assert(effect.target.includes('data-tile-reveal'), 'only an opaque destination veil fades, never a tile/pane');
    }
  }
  for (const fraction of [0, .25, .5, .75, 1]) {
    await page.evaluate(fraction => {
      for (const a of __tileCapture) a.currentTime = Number(a.effect.getTiming().duration) * fraction;
    }, fraction);
    await page.evaluate(() => new Promise(resolve => requestAnimationFrame(resolve)));
    if (!before) {
      const roots = await page.locator('[data-tile-motion], [data-pane-motion]').evaluateAll(els => els.filter(el => el.getClientRects().length).map(el => {
        const s = getComputedStyle(el), m = new DOMMatrix(s.transform);
        return { scaleX: m.a, scaleY: m.d, opacity: s.opacity };
      }));
      for (const root of roots) {
        assert.equal(root.scaleX, 1); assert.equal(root.scaleY, 1); assert.equal(root.opacity, '1');
      }
      const opaque = await page.locator('[data-tile-motion] > section').evaluateAll(els => {
        const canvas = document.createElement('canvas'); canvas.width = canvas.height = 1;
        const context = canvas.getContext('2d');
        return els.filter(el => el.getClientRects().length).every(el => {
          context.clearRect(0, 0, 1, 1); context.fillStyle = getComputedStyle(el).backgroundColor; context.fillRect(0, 0, 1, 1);
          return context.getImageData(0, 0, 1, 1).data[3] === 255 && getComputedStyle(el).opacity === '1';
        });
      });
      assert(opaque, 'destination backgrounds remain fully opaque at every captured frame');
    }
    await page.screenshot({ path: path.join(shots, `${phase}-${label}-${Math.round(fraction * 100)}.png`) });
  }
  await page.evaluate(() => { __pauseTiles = false; for (const a of __tileCapture) if (a.playState !== 'idle') a.finish(); });
  await settle(page);
  if (!before) assert.equal(await page.locator('[data-tile-reveal]').count(), 0, 'completed reveals clean up their veils');
  return effects;
}

async function main() {
  fs.mkdirSync(shots, { recursive: true });
  const playwright = require(process.env.PLAYWRIGHT_CORE || '/home/zheilbron/.bun/install/cache/playwright-core@1.50.1@@@1');
  const { server, origin } = await serve();
  const browser = await launchBrowser(playwright, 'chromium');
  const results = [];
  try {
    for (const columns of [3, 4]) for (const mode of ['full', 'reduced', 'off']) {
      const motion = { choice: mode === 'full' ? null : mode, effective: mode, os: 'no-preference' };
      const f = await fixture(browser, origin, { width: 1600, height: 1000 }, 'dark', motion, () => {});
      try {
        await seed(f, columns);
        const page = f.page;
        const startup = await page.evaluate(() => __startupShifts);
        const tile = page.locator('[data-tile-motion="10"]');
        const label = `${columns}col-${mode}`;
        const maximize = await capture(page, `${label}-maximize`, () => tile.getByRole('button', { name: 'Focus', exact: true }).click(), mode);
        const restore = await capture(page, `${label}-restore`, () => tile.getByRole('button', { name: 'Defocus', exact: true }).click(), mode);
        // An unpaused, normal-speed run is measured separately from captures.
        await page.evaluate(() => { __shifts.length = 0; });
        await tile.getByRole('button', { name: 'Focus', exact: true }).click(); await settle(page);
        await tile.getByRole('button', { name: 'Defocus', exact: true }).click(); await settle(page);
        const shifts = await page.evaluate(() => __shifts);
        results.push({ label, maximize, restore, startup,
          startupCLS: startup.filter(e => !e.recent).reduce((sum, e) => sum + e.value, 0), shifts,
          totalShift: shifts.reduce((sum, e) => sum + e.value, 0),
          cls: shifts.filter(e => !e.recent).reduce((sum, e) => sum + e.value, 0) });
        if (!before) {
          const neighbour = page.locator(`[data-tile-motion="${10 + columns}"]`);
          await capture(page, `${label}-neighbour-close`, () => neighbour.getByRole('button', { name: 'Close this tile', exact: true }).click(), mode);
          await capture(page, `${label}-open`, async () => {
            await page.getByRole('link', { name: 'Docs', exact: true }).first().click({ modifiers: ['Control'] });
            await page.waitForFunction(() => document.querySelectorAll('[data-tile-motion]').length === 6);
          }, mode);
          await page.evaluate(() => { __pauseTiles = true; __tileCapture = []; });
          await tile.getByRole('button', { name: 'Focus', exact: true }).click();
          await page.evaluate(() => new Promise(resolve => requestAnimationFrame(resolve)));
          // Interrupt a real, paused reveal before it finishes, then switch Off.
          await tile.getByRole('button', { name: 'Defocus', exact: true }).click();
          await page.evaluate(() => { c6Motion.set('off'); __pauseTiles = false; });
          await settle(page);
          assert.equal(await page.locator('[data-tile-reveal]').count(), 0, 'interruption/Off leaves no stale cover');
        }
        assert.deepEqual(f.errors, []);
        console.log(`PASS ${label}; shift=${results.at(-1).totalShift}, CLS=${results.at(-1).cls}`);
      } finally { await f.context.close(); }
    }
  } finally { await browser.close(); await new Promise(resolve => server.close(resolve)); }
  fs.writeFileSync(path.join(shots, `${phase}-tile-motion.json`), JSON.stringify(results, null, 2));
}
if (require.main === module) main().catch(error => { console.error(error); process.exitCode = 1; });
