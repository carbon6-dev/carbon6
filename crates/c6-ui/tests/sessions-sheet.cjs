#!/usr/bin/env node
'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, launchBrowser } = require('./motion.cjs');
const output = process.env.SCREENSHOTS || '/tmp/n3-ui/item4';
const before = process.env.BEFORE === '1';
const phase = before ? 'before' : 'after';

async function main() {
  fs.mkdirSync(output, { recursive: true });
  const { server, origin } = await serve();
  const playwright = require(process.env.PLAYWRIGHT_CORE || '/home/zheilbron/.bun/install/cache/playwright-core@1.50.1@@@1');
  try {
    for (const theme of ['light', 'dark']) for (const view of ['desktop', 'tile', 'phone']) {
      const browser = await launchBrowser(playwright, process.env.ENGINE || 'chromium');
      try {
        const phone = view === 'phone';
        const f = await fixture(browser, origin, phone ? { width: 390, height: 844 } : { width: 1440, height: 1000 }, theme,
          { choice: 'off', effective: 'off', os: 'no-preference' }, () => {}, {}, {
            path: '/sessions', ready: 'input[aria-label="Search sessions"]:visible, input[placeholder="Search titles, labels, workers, folders"]:visible',
            layout: { tiles: [{ key: 2, columns: [{ key: 3, view: { view: 'sessions' } }],
              cursor: 0, active: 3, used: 3, back: [], forward: [], stack: 0, minimized: false }], focus: 0, next: 4, full: false },
          });
        const { page } = f, tile = page.locator('[data-tile-surface]').first();
        if (view === 'desktop') {
          await page.getByRole('button', { name: /^Sort:/ }).click();
          await page.locator('.c6-pop:visible').last().waitFor();
          await page.screenshot({ path: path.join(output, `${phase}-${theme}-${view}.png`) });
          await page.keyboard.press('Escape');
        } else {
          if (!phone) {
            await page.locator('[data-tile-motion]').first().evaluate(el => { el.style.width = '408px'; el.style.height = '540px'; });
            await settle(page);
          }
          const button = page.getByRole('button', { name: 'Filter, sort and group', exact: true });
          await button.click();
          const sheet = page.getByRole('dialog', { name: 'Filter, sort and group', exact: true });
          await sheet.waitFor();
          const check = async () => {
            await settle(page);
            const geometry = await sheet.evaluate(el => {
              const box = el.getBoundingClientRect();
              const tile = document.querySelector('[data-tile-surface]'), t = tile.getBoundingClientRect();
              const parent = el.parentElement;
              const scrim = el.previousElementSibling?.getBoundingClientRect();
              return { tileLocal: tile.contains(el), left: box.left, right: box.right, top: box.top, bottom: box.bottom,
                tile: { left: t.left, right: t.right, top: t.top, bottom: t.bottom },
                height: visualViewport.height + visualViewport.offsetTop,
                parent: parent.tagName, scrim: scrim && { left: scrim.left, right: scrim.right, top: scrim.top, bottom: scrim.bottom } };
            });
            if (before) console.log('baseline geometry:', JSON.stringify(geometry));
            if (!before) {
              assert(geometry.tileLocal, 'sheet and scrim portal into their own tile');
              assert(geometry.left >= geometry.tile.left - 1 && geometry.right <= geometry.tile.right + 1,
                `horizontal tile bounds: ${JSON.stringify(geometry)}`);
              assert(geometry.top >= geometry.tile.top - 1 && geometry.bottom <= Math.min(geometry.tile.bottom, geometry.height) + 1,
                `vertical tile/keyboard bounds: ${JSON.stringify(geometry)}`);
              assert(Math.abs(geometry.scrim.left - geometry.tile.left) <= 1 && Math.abs(geometry.scrim.right - geometry.tile.right) <= 1,
                'scrim does not dim other tiles');
            }
          };
          await check();
          await page.screenshot({ path: path.join(output, `${phase}-${theme}-${view}.png`) });
          if (phone) {
            await page.evaluate(() => __keyboard(innerHeight - 300));
            await check();
            await page.screenshot({ path: path.join(output, `${phase}-${theme}-${view}-keyboard.png`) });
          }
          if (!before) {
            const done = sheet.getByRole('button', { name: 'Done', exact: true });
            await done.scrollIntoViewIfNeeded();
            await done.click();
            await sheet.waitFor({ state: 'detached' });
            await button.click();
            await sheet.waitFor();
            await page.keyboard.press('Escape');
            await sheet.waitFor({ state: 'detached' });
            await button.click();
            await sheet.waitFor();
            await page.locator('.c6-session-filter-scrim').click({ position: { x: 8, y: 8 } });
            await sheet.waitFor({ state: 'detached' });
          }
        }
        assert.deepEqual(f.errors, []);
        console.log(`${phase}: ${theme}-${view}`);
      } finally { await browser.close(); }
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
}

main().catch(error => { console.error(error); process.exitCode = 1; });
