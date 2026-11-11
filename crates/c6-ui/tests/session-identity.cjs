#!/usr/bin/env node
'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');
const shots = process.env.SCREENSHOTS || '/tmp/n3-ui/item6';
const before = process.env.BEFORE === '1';
const id = '01a1b4ba-e2bb-7444-8962-fb70504fcd1d';
const motion = { choice: 'off', effective: 'off', os: 'no-preference' };

async function check(browser, origin, theme, width, phone) {
  const label = `${before ? 'before-' : ''}${theme}-${phone ? 'phone' : 'tile'}-${width}`;
  console.log(label);
  const f = await fixture(browser, origin, phone ? { width, height: 844 } : { width: 1600, height: 1000 }, theme, motion, () => {}, {}, {
    path: `/sessions/${id}`,
    layout: { tiles: [{ key: 2, columns: [{ key: 3, view: { view: 'session', id } }], cursor: 0, active: 3, used: 3, back: [], forward: [], stack: 0, minimized: false }], focus: 0, next: 4, full: false },
    setup: async ({ context }) => {
      await context.addInitScript(() => {
        window.__copies = [];
        Object.defineProperty(navigator, 'clipboard', { value: { writeText: async text => {
          if (window.__copyFails) throw new Error('Clipboard denied');
          window.__copies.push(text);
        } } });
      });
      await context.route(`**/api/v1/sessions/${id}`, route => route.fulfill({ json: {
        session: { id, realm: 'home', worker_id: '00000000-0000-0000-0000-000000000000', title: 'Session identity browser verification', cwd: '/tmp', connection_id: 'test', model: 'test', status: 'idle', created_at: 1, updated_at: 1, last_seq: 70 },
        worker_name: 'Motion fixture', worker_online: true, queued: [],
      } }));
      await context.route(`**/api/v1/sessions/${id}/view`, route => route.fulfill({ json: { catchup: false, fold: false } }));
      await context.route(`**/api/v1/sessions/${id}/catchup`, route => route.fulfill({ status: 204 }));
    },
  });
  const { page } = f;
  const geometry = {};
  const capture = async (name, controls) => {
    geometry[name] = await controls.evaluateAll(els => els.map(el => {
      const r = el.getBoundingClientRect();
      return { label: el.getAttribute('aria-label') || el.textContent, x: r.x, y: r.y, width: r.width, height: r.height, right: r.right };
    }));
    for (const r of geometry[name]) assert(r.width > 0 && r.x >= 0 && r.right <= page.viewportSize().width + 1, `${name}: contained control ${JSON.stringify(r)}`);
    await page.screenshot({ path: path.join(shots, `${label}-${name}.png`) });
  };
  try {
    if (!phone) await page.addStyleTag({ content: `[data-tile-motion] { width: ${width + 10}px !important; }` });
    await settle(page);
    if (before) {
      const runs = page.getByText('Runs on', { exact: true }).first();
      if (!await runs.isVisible()) await page.getByRole('button', { name: /Open session sheet/ }).first().click();
      await runs.waitFor();
      geometry.baseline = { glanceCopyIds: await page.getByRole('button', { name: 'Copy session ID', exact: true }).count() };
      await capture('glance', runs.locator('..'));
      await page.keyboard.press('Escape');
      await page.getByRole('button', { name: 'Actions', exact: true }).first().click();
      const menu = page.locator('.c6-pop:visible').last();
      geometry.baseline.menuCopyOptions = await menu.getByRole('button', { name: /^Copy session/ }).count();
      await capture('menu', menu.getByRole('button'));
      await menu.getByRole('button', { name: 'Session details…', exact: false }).click();
      const dialog = page.getByRole('dialog', { name: 'Session details', exact: true });
      await dialog.waitFor();
      geometry.baseline.detailsCopyIds = await dialog.getByRole('button', { name: 'Copy session ID', exact: true }).count();
      await capture('details', dialog.getByRole('button', { name: 'Close', exact: true }));
      fs.writeFileSync(path.join(shots, `${label}.json`), JSON.stringify(geometry, null, 2) + '\n');
      assert.deepEqual(f.errors, []);
      return;
    }
    // Glance is beside a wide transcript, otherwise opened from its ring.
    if (!await page.getByRole('button', { name: 'Copy session ID', exact: true }).first().isVisible()) {
      await page.getByRole('button', { name: /Open session sheet|Open session glance/ }).first().click();
    }
    const short = page.getByRole('button', { name: 'Copy session ID', exact: true }).filter({ hasText: id.slice(0, 8) }).first();
    await short.waitFor();
    assert.equal(await short.getAttribute('title'), id, 'hover exposes full ID');
    assert.equal((await short.textContent()).trim(), id.slice(0, 8));
    await short.click();
    await short.getByRole('status').filter({ hasText: 'Copied' }).waitFor();
    assert.equal(await page.evaluate(() => window.__copies.at(-1)), id);
    await capture('glance', short);
    await page.keyboard.press('Escape');
    await page.getByRole('button', { name: 'Actions', exact: true }).first().click();
    const menu = page.locator('.c6-pop:visible').last();
    await menu.getByRole('button', { name: 'Copy session ID', exact: true }).click();
    await menu.getByRole('button', { name: 'Copy session ID', exact: true }).getByRole('status').filter({ hasText: 'Copied' }).waitFor();
    assert.equal(await page.evaluate(() => window.__copies.at(-1)), id);
    await menu.getByRole('button', { name: 'Copy session link', exact: true }).click();
    await menu.getByRole('button', { name: 'Copy session link', exact: true }).getByRole('status').filter({ hasText: 'Copied' }).waitFor();
    assert.equal(await page.evaluate(() => window.__copies.at(-1)), `${origin}/r/home/sessions/${id}`);
    await capture('menu', menu.getByRole('button', { name: /^Copy session/ }));
    await menu.getByRole('button', { name: 'Session details…', exact: false }).click();
    const dialog = page.getByRole('dialog', { name: 'Session details', exact: true });
    const detailId = dialog.getByRole('button', { name: 'Copy session ID', exact: true });
    assert.equal(await detailId.getAttribute('title'), id);
    assert.equal((await detailId.textContent()).trim(), id.slice(0, 8));
    await page.evaluate(() => { window.__copyFails = true; });
    await detailId.click();
    await detailId.getByRole('status').filter({ hasText: "Couldn't copy" }).waitFor();
    await page.evaluate(() => { window.__copyFails = false; });
    await detailId.click();
    await detailId.getByRole('status').filter({ hasText: 'Copied' }).waitFor();
    assert.equal(await page.evaluate(() => window.__copies.at(-1)), id);
    const bounds = await detailId.boundingBox();
    assert(bounds.x >= 0 && bounds.x + bounds.width <= page.viewportSize().width, 'ID fits the viewport');
    await capture('details', detailId);
    fs.writeFileSync(path.join(shots, `${label}.json`), JSON.stringify(geometry, null, 2) + '\n');
    assert.deepEqual(f.errors, []);
  } finally { await f.context.close(); }
}

async function main() {
  fs.mkdirSync(shots, { recursive: true });
  const { server, origin } = await serve();
  const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
  const browser = await launchBrowser(playwright, process.env.ENGINE || 'chromium');
  try {
    for (const theme of ['light', 'dark']) {
      for (const width of [1200, 320]) await check(browser, origin, theme, width, false);
      for (const width of [320, 390]) await check(browser, origin, theme, width, true);
    }
  } finally { await browser.close(); await new Promise(resolve => server.close(resolve)); }
}
within('session identity', main).catch(error => { console.error(error); process.exitCode = 1; });
