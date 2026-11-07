#!/usr/bin/env node
'use strict';

// Actual clicks against production WASM, with an owned loopback fixture.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');
const shots = process.env.SCREENSHOTS || '/tmp/q1-screenshots/panes';
const motion = { choice: 'off', effective: 'off', os: 'no-preference' };
const stored = page => page.evaluate(() => JSON.parse(localStorage.getItem('c6.home.workspace.v2')));

async function check(browser, origin, theme, width, phone) {
  console.log(`panes ${theme} ${phone ? 'phone' : 'tile'} ${width}`);
  const f = await fixture(browser, origin, phone ? { width, height: 844 } : { width: 1600, height: 1000 }, theme, motion, () => {});
  const { page, context } = f;
  try {
    // Fix the fixture tile's actual content width, including across reactive
    // layout updates. The desktop viewport never turns into phone mode.
    if (!phone) await page.addStyleTag({ content: `[data-tile-motion] { width: ${width + 10}px !important; }` });
    await settle(page);
    const initial = await stored(page), tile = initial.tiles[0].key;
    const session = page.locator('[data-pane-motion]').first();
    const transcript = await session.locator('textarea').elementHandle();
    const open = async name => {
      if (phone) {
        await page.getByRole('button', { name: /Open session sheet/ }).click();
        await page.getByRole('button', { name, exact: true }).last().click();
      } else if (width < 480 || ['Files', 'Outputs'].includes(name)) {
        await page.getByRole('button', { name: 'Actions', exact: true }).click();
        await page.locator('.c6-pop:visible').getByRole('button', { name, exact: true }).click();
      } else {
        await page.getByRole('button', { name, exact: true }).click();
      }
      await page.waitForFunction(({ tile, view }) => {
        const l = JSON.parse(localStorage.getItem('c6.home.workspace.v2'));
        return l.tiles.find(t => t.key === tile)?.columns.some(c => c.view.view === view);
      }, { tile, view: `session_${name.toLowerCase()}` });
      await settle(page);
      const l = await stored(page), t = l.tiles.find(t => t.key === tile);
      assert.equal(l.tiles.length, initial.tiles.length, `${name} never makes a tile`);
      assert.equal(t.columns.length, 2);
      assert.equal(t.columns[0].view.view, 'session', 'transcript stays beside its pane');
      assert.equal(t.columns[1].view.view, `session_${name.toLowerCase()}`);
      assert(await transcript.evaluate(el => el.isConnected), 'opening a pane retains the composer');
      const pane = page.locator(`[data-pane-motion="${t.columns[1].key}"]`);
      assert(await pane.isVisible(), `${name} is shown`);
      await page.screenshot({ path: path.join(shots, `${theme}-${phone ? 'phone' : 'tile'}-${width}-${name.toLowerCase()}.png`) });
      if (phone || width < 640) {
        assert(!(await session.isVisible()), 'narrow tile shows one retained pane');
        await page.getByRole('button', { name: 'Back to the list', exact: true }).click();
        assert(await session.isVisible(), 'single-pane Back returns to the transcript');
        assert.equal(await pane.count(), 1, 'Back does not discard the pane');
      }
    };
    for (const name of ['Changes', 'Terminal', 'Files', 'Outputs']) {
      await open(name);
      // An existing pane hidden by narrow Back is revealed, never toggled
      // closed or opened again in another tile.
      if (phone || width < 640) await open(name);
    }
    if (!phone && width >= 480) {
      const n = (await stored(page)).tiles.length;
      await page.getByRole('button', { name: 'Terminal', exact: true }).click({ modifiers: ['Meta'] });
      await page.waitForFunction(n => JSON.parse(localStorage.getItem('c6.home.workspace.v2')).tiles.length === n + 1, n);
      assert.equal((await stored(page)).tiles.at(-1).columns[0].view.view, 'session_terminal', 'explicit new-tile click still separates');
    }
    assert.deepEqual(f.errors, [], 'no browser crashes');
  } finally { await context.close(); }
}

async function main() {
  fs.mkdirSync(shots, { recursive: true });
  const { server, origin } = await serve();
  const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
  const browser = await launchBrowser(playwright, process.env.ENGINE || 'chromium');
  try {
    for (const theme of ['light', 'dark']) {
      for (const width of [1200, 800, 640, 480, 320, 240]) await check(browser, origin, theme, width, false);
      for (const width of [320, 390]) await check(browser, origin, theme, width, true);
    }
  } finally { await browser.close(); await new Promise(resolve => server.close(resolve)); }
}
within('same-tile session panes', main).catch(error => { console.error(error); process.exitCode = 1; });
