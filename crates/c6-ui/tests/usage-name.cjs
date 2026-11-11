#!/usr/bin/env node
'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');
const shots = process.env.SCREENSHOTS || '/tmp/n3-ui/item7';
const before = process.env.BEFORE === '1';
const motion = { choice: 'off', effective: 'off', os: 'no-preference' };
const layout = { tiles: [{ key: 2, columns: [{ key: 3, view: { view: 'usage' } }], cursor: 0, active: 3, used: 3, back: [], forward: [], stack: 0, minimized: false }], focus: 0, next: 4, full: false };

async function check(browser, origin, theme, width, phone, consolePage) {
  const label = `${before ? 'before' : 'after'}-${theme}-${consolePage ? 'console' : 'realm'}-${phone ? 'phone' : 'tile'}-${width}`;
  const destination = before ? (consolePage ? 'Spending' : 'Usage and budgets') : 'Usage';
  console.log(label);
  const f = await fixture(browser, origin, phone ? { width, height: 844 } : { width: 1600, height: 1000 }, theme, motion, () => {}, {}, {
    path: '/usage', layout, ready: 'main select',
    setup: async ({ context }) => {
      if (consolePage) await context.addInitScript(layout => {
        localStorage.setItem('c6.admin.workspace.v2', JSON.stringify(layout));
        if (location.pathname === '/r/home/usage') history.replaceState(null, '', '/admin/usage');
      }, layout);
      await context.route('**/api/**', route => {
        const p = new URL(route.request().url()).pathname;
        if (p.endsWith('/usage')) return route.fulfill({ json: {
          since_ms: 1, total: { key: '', name: '', calls: 12, input: 12000, output: 1500, cache_read: 500, cache_write: 0, cost: 1.25, unpriced_calls: 0 },
          by_day: [], by_model: [], by_connection: [], by_label: [], top_sessions: [], unpriced_models: [], budgets: [],
        } });
        return route.fallback();
      });
    },
  });
  const { page } = f;
  try {
    if (!phone) await page.addStyleTag({ content: `[data-tile-motion] { width: ${width + 10}px !important; }` });
    await settle(page);
    await page.getByText('Model calls', { exact: true }).waitFor();
    const base = consolePage ? '/admin' : '/r/home';
    const usageLinks = page.locator(`a[href="${base}/usage"]`);
    assert(await usageLinks.count() > 0);
    for (const text of await usageLinks.allTextContents()) {
      if (before) assert(['Spending', 'Usage and budgets'].includes(text.trim()), 'baseline records both old navigation names');
      else assert.equal(text.trim(), destination, 'navigation names match this build');
    }
    if (!before) assert.equal(await page.getByText(/^(Spending|Usage and budgets)$/).count(), 0);
    if (consolePage && !phone) assert(await page.locator('header').getByRole('link', { name: destination, exact: true }).isVisible());
    await page.screenshot({ path: path.join(shots, `${label}-page.png`) });
    // Ample-space headers hide their overflow menu; exercise its real live
    // drawer at a desktop width where navigation overflows, as header-fit does.
    if (!phone && !await page.locator('button[aria-label^="Menu,"]:visible').count()) {
      await page.setViewportSize({ width: 1024, height: 1000 });
      await settle(page);
    }
    const menuButton = page.locator('button[aria-label^="Menu,"]:visible').first();
    await menuButton.click();
    const drawer = page.locator('.phone-drawer[data-open="true"]').first();
    const usage = drawer.getByRole('link', { name: destination, exact: true });
    await usage.waitFor();
    const geometry = await usage.evaluate(el => {
      const r = el.getBoundingClientRect();
      return { x: r.x, y: r.y, width: r.width, height: r.height, right: r.right, scrollWidth: el.scrollWidth, clientWidth: el.clientWidth };
    });
    assert(geometry.x >= 0 && geometry.right <= page.viewportSize().width + 1 && geometry.width > 0);
    // The inherited desktop drawer is max-w-full inside its narrow button
    // wrapper. Record that unrelated overflow, rather than silently claiming
    // this naming-only change fixed it. Phone links must fit normally.
    if (phone) assert(geometry.scrollWidth <= geometry.clientWidth + 1, JSON.stringify(geometry));
    else if (geometry.scrollWidth > geometry.clientWidth + 1) console.log('inherited desktop drawer overflow', JSON.stringify(geometry));
    fs.writeFileSync(path.join(shots, `${label}.json`), JSON.stringify(geometry, null, 2) + '\n');
    await page.screenshot({ path: path.join(shots, `${label}-menu.png`) });
    await usage.click();
    await page.waitForFunction(() => !document.querySelector('.phone-drawer[data-open="true"]'));
    assert.equal(new URL(page.url()).pathname, `${base}/usage`);
    if (consolePage) {
      await menuButton.click();
      await drawer.getByRole('link', { name: 'Settings', exact: true }).click();
      const settings = page.locator('main:has(select option[value="spending"])');
      await settings.waitFor();
      const select = settings.locator('select').first();
      if (await select.isVisible()) await select.selectOption('spending');
      else await settings.getByRole('button', { name: 'Prices', exact: true }).click();
      await page.getByText(before ? /What model calls cost, in USD per million tokens, for Usage and budgets\./ : /What model calls cost, in USD per million tokens, for Usage\./).waitFor();
      if (!before) assert.equal(await page.getByText(/Usage and budgets/).count(), 0);
      await page.screenshot({ path: path.join(shots, `${label}-prices.png`) });
    }
    assert.deepEqual(f.errors, []);
  } finally { await f.context.close(); }
}

async function main() {
  fs.mkdirSync(shots, { recursive: true });
  const { server, origin } = await serve();
  const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
  const run = async (theme, width, phone, consolePage) => {
    const browser = await launchBrowser(playwright, process.env.ENGINE || 'chromium');
    try { await check(browser, origin, theme, width, phone, consolePage); }
    finally { await browser.close(); }
  };
  try {
    for (const theme of ['light', 'dark']) for (const consolePage of [false, true]) {
      for (const width of [1200, 320]) await run(theme, width, false, consolePage);
      for (const width of [320, 390]) await run(theme, width, true, consolePage);
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
}
within('Usage navigation naming', main).catch(error => { console.error(error); process.exitCode = 1; });
