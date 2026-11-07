#!/usr/bin/env node
'use strict';

// Real production WASM, private fake head, no live settings or services.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, launchBrowser } = require('./motion.cjs');
const output = process.env.SCREENSHOTS || '/tmp/n3-ui/item1';
const before = process.env.BEFORE === '1';
const phase = before ? 'before' : 'after';
const motion = { choice: 'off', effective: 'off', os: 'no-preference' };

async function bounds(page, popup, tile = true) {
  await popup.waitFor();
  const result = await popup.evaluate((el, inTile) => {
    const r = el.getBoundingClientRect();
    // Phone sheets deliberately paint a noninteractive spring-overshoot tail
    // below their layout bottom. Judge reachable content, not that padding.
    const bottomEdge = r.bottom + Math.min(0, parseFloat(getComputedStyle(el).marginBottom) || 0);
    const t = document.querySelector('[data-tile-surface]').getBoundingClientRect();
    const v = visualViewport;
    const left = Math.max(inTile ? t.left : 0, v.offsetLeft), top = Math.max(inTile ? t.top : 0, v.offsetTop);
    const right = Math.min(inTile ? t.right : innerWidth, v.offsetLeft + v.width);
    const bottom = Math.min(inTile ? t.bottom : innerHeight, v.offsetTop + v.height);
    const x = Math.max(left + 1, Math.min(right - 1, r.left + r.width / 2));
    const y = Math.max(top + 1, Math.min(bottom - 1, r.top + Math.min(r.height / 2, 30)));
    return { left: r.left, top: r.top, right: r.right, bottom: bottomEdge,
      fits: r.left >= left - 1 && r.right <= right + 1 && r.top >= top - 1 && bottomEdge <= bottom + 1,
      hit: el.contains(document.elementFromPoint(x, y)) };
  }, tile);
  if (before) console.log('baseline geometry:', JSON.stringify(result));
  if (!before) assert(result.fits && result.hit, `popover bounded and hittable: ${JSON.stringify(result)}`);
  return result;
}

async function keyboard(page, on) {
  // Overlay keyboard, like the existing production model-picker regression.
  await page.evaluate(on => __keyboard(on ? innerHeight - 300 : innerHeight), on);
  await settle(page);
}

async function tasks(f, name, phone) {
  const { page } = f;
  console.log(`${name}: tasks`);
  if (phone) await page.locator('[data-phone-status] button').click();
  else await page.getByTitle('Open the background tasks', { exact: true }).click();
  const popup = phone ? page.getByRole('dialog', { name: 'Session status', exact: true }) : page.locator('.c6-pop:visible').last();
  await bounds(page, popup, !phone);
  await page.screenshot({ path: path.join(output, `${phase}-${name}-tasks.png`) });
  if (!before) {
    const stop = popup.getByRole('button', { name: 'Stop', exact: true }).last();
    await stop.scrollIntoViewIfNeeded();
    await stop.click();
    await page.waitForFunction(() => window.__n3Stops === 1);
    const inspect = popup.getByRole('button', { name: 'Inspect', exact: true }).last();
    await inspect.scrollIntoViewIfNeeded();
    await inspect.click();
    const inspector = page.getByRole('dialog', { name: 'Background task inspector' });
    await inspector.waitFor();
    await page.keyboard.press('Escape');
  }
  await page.keyboard.press('Escape');
  if (phone) {
    await keyboard(page, true);
    await page.locator('[data-phone-status] button').click();
    await bounds(page, popup, false);
    await page.screenshot({ path: path.join(output, `${phase}-${name}-tasks-keyboard.png`) });
    await page.keyboard.press('Escape');
    await keyboard(page, false);
  }
}

async function model(f, name, phone) {
  const { page } = f;
  console.log(`${name}: model`);
  await page.getByRole('button', { name: /^Model:/ }).click();
  const popup = phone ? page.getByRole('dialog', { name: 'Model', exact: true }) : page.locator('[data-pop]').filter({ has: page.getByText('Model', { exact: true }) }).last();
  await popup.getByPlaceholder('model id', { exact: true }).fill('custom-model-id');
  await popup.getByRole('button', { name: 'fixture-model-32', exact: true }).waitFor();
  await settle(page);
  await bounds(page, popup, !phone);
  await page.screenshot({ path: path.join(output, `${phase}-${name}-model.png`) });
  if (!before) {
    const input = await popup.getByPlaceholder('model id', { exact: true }).elementHandle();
    const last = popup.getByRole('button', { name: 'fixture-model-32', exact: true });
    await last.scrollIntoViewIfNeeded();
    await last.click();
    assert.equal(await popup.getByPlaceholder('model id', { exact: true }).inputValue(), 'fixture-model-32');
    if (!phone) {
      // A retained open menu follows a tile resize, without remounting its form.
      await page.locator('[data-tile-motion]').first().evaluate(el => { el.style.width = '288px'; });
      await settle(page);
      await bounds(page, popup);
      assert(await input.evaluate(el => el.isConnected && document.activeElement === el), 'resize retains the focused model input');
    }
  }
  await page.keyboard.press('Escape');
}

async function send(f, name, phone) {
  const { page } = f;
  console.log(`${name}: send`);
  await f.setState('working');
  await page.locator('textarea').fill('Keep this draft');
  if (phone) await keyboard(page, true);
  await page.getByRole('button', { name: 'More ways to send', exact: true }).click();
  const popup = page.getByRole('menu').filter({ hasText: 'Send now' });
  await bounds(page, popup);
  await page.screenshot({ path: path.join(output, `${phase}-${name}-send.png`) });
  if (!before) {
    await popup.getByRole('menuitem', { name: 'Send now' }).click();
    await page.waitForFunction(() => document.querySelector('textarea').value === '');
  } else await page.keyboard.press('Escape');
}

async function main() {
  fs.mkdirSync(output, { recursive: true });
  const playwright = require(process.env.PLAYWRIGHT_CORE || '/home/zheilbron/.bun/install/cache/playwright-core@1.50.1@@@1');
  const { server, origin } = await serve();
  try {
    for (const theme of (process.env.THEMES || 'light,dark').split(',')) for (const view of (process.env.VIEWS || 'desktop,tile,phone').split(',')) {
      const browser = await launchBrowser(playwright, process.env.ENGINE || 'chromium');
      try {
        const phone = view === 'phone';
        const events = Array.from({ length: 18 }, (_, i) => ({ seq: 70 + i, ts: 70 + i, type: 'background_task', task_id: `n3-task-${i}`, kind: 'shell',
          description: `Task ${i}: a long background operation with independently reachable controls`, state: 'started', notify: true }));
        const f = await fixture(browser, origin, phone ? { width: 390, height: 844 } : { width: 1440, height: 1000 }, theme, motion, () => {}, {}, {
          instrumentation: true, events,
          setup: async ({ page, context }) => {
            await context.route('**/api/**/tasks/**', async route => {
              if (route.request().method() === 'POST') {
                await page.evaluate(() => { window.__n3Stops = (window.__n3Stops || 0) + 1; });
                return route.fulfill({ json: {} });
              }
              return route.fulfill({ status: 503, json: { error: 'Fixture worker offline' } });
            });
          },
        });
        if (view === 'tile') {
          await f.page.locator('[data-tile-motion]').first().evaluate(el => { el.style.width = '328px'; el.style.height = '540px'; });
          await settle(f.page);
        }
        const name = `${theme}-${view}`;
        console.log(`${phase}: ${name}`);
        const plus = f.page.getByRole('button', { name: 'Attach images or files', exact: true });
        assert.equal(await plus.locator('svg').count(), 1, 'the attachment control keeps its icon');
        const [chooser] = await Promise.all([f.page.waitForEvent('filechooser'), plus.click()]);
        assert(chooser.isMultiple(), '+ opens the native multiple-file chooser, not a clipped web menu');
        await chooser.setFiles([]);
        await tasks(f, name, phone);
        await model(f, name, phone);
        await send(f, name, phone);
        assert.deepEqual(f.errors, []);
      } finally { await browser.close(); }
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
}

main().catch(error => { console.error(error); process.exitCode = 1; });
