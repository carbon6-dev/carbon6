#!/usr/bin/env node
'use strict';

// Production WASM, owned loopback fake head, real content-dependent layout.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');
const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
const shots = process.env.SCREENSHOTS || '/tmp/q2-screenshots';
const before = process.env.BEFORE === '1';
const phase = before ? 'before' : 'after';
const motion = { choice: 'off', effective: 'off', os: 'no-preference' };
const widths = [768, 1024, 1200, 1280, 1600, 2000];

async function geometry(page) {
  return page.locator('header').evaluate(header => {
    const shown = el => {
      if (el.closest('.phone-drawer, .c6-pop')) return false;
      const closed = el.closest('details:not([open])');
      if (closed && !closed.querySelector('summary')?.contains(el)) return false;
      return el.getClientRects().length && getComputedStyle(el).visibility !== 'hidden' && !el.closest('[inert]');
    };
    const controls = [...header.querySelectorAll('a, button, input, summary')].filter(shown).map(el => {
      const r = el.getBoundingClientRect();
      return { label: el.getAttribute('aria-label') || el.textContent.trim() || el.getAttribute('placeholder'),
        x: r.x, y: r.y, right: r.right, bottom: r.bottom, width: r.width, height: r.height };
    });
    const overlaps = controls.flatMap((a, i) => controls.slice(i + 1).filter(b =>
      Math.min(a.right, b.right) - Math.max(a.x, b.x) > .5 &&
      Math.min(a.bottom, b.bottom) - Math.max(a.y, b.y) > .5).map(b => [a.label, b.label]));
    const bounds = header.getBoundingClientRect();
    const overflow = controls.filter(r => r.x < bounds.x - .5 || r.right > bounds.right + .5 || r.y < bounds.y - .5 || r.bottom > bounds.bottom + .5);
    const realm = [...header.querySelectorAll('summary span.font-medium')].find(shown);
    return { controls, overlaps, overflow, realm: realm && { text: realm.textContent, width: realm.getBoundingClientRect().width,
      clipped: realm.scrollWidth > realm.clientWidth + 1 }, moved: [...header.querySelectorAll('[data-header-overflow]')].map(el => el.dataset.headerItem),
      menu: !!header.querySelector('[data-header-menu]') && !!shown(header.querySelector('[data-header-menu]')) };
  });
}

async function ready(page) {
  if (!before) await page.locator('header[data-header-ready]').waitFor();
  await settle(page);
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
}

async function main() {
  fs.mkdirSync(shots, { recursive: true });
  const { server, origin } = await serve();
  const engines = (process.env.ENGINE || 'both') === 'both' ? ['chromium', 'webkit'] : [process.env.ENGINE];
  try {
    for (const engine of engines) for (const theme of ['light', 'dark']) for (const large of [false, true]) {
      if (process.env.THEME && !process.env.THEME.split(',').includes(theme)) continue;
      if (process.env.COUNTS && process.env.COUNTS !== (large ? 'large' : 'short')) continue;
      const browser = await launchBrowser(playwright, engine);
      let f;
      try {
        f = await fixture(browser, origin, { width: 1600, height: 1000 }, theme, motion, () => {}, {}, {
          setup: async ({ context }) => {
            await context.addInitScript(large => {
              localStorage.setItem('c6.home.attention', JSON.stringify(large ? Array.from({ length: 20 }, (_, i) => `header-unseen-${i}`) : []));
            }, large);
            const row = { id: 'header', realm: 'home', version: 1, title: 'Header attention fixture', state: 'needs_you', status: 'open',
              labels: [], created_at: 1, updated_at: 1 };
            await context.route('**/api/**', route => {
              const p = new URL(route.request().url()).pathname;
              let json;
              if (p.endsWith('/elsewhere')) json = [{ id: 'home', name: large ? 'Carbon6 research and development workspace' : 'Home', waiting: 0 },
                { id: 'other', name: 'Other', waiting: large ? 99999 : 0 }];
              else if (p.endsWith('/version')) json = { version: large ? 'motion-test-long-preview' : 'test' };
              else if (p.endsWith('/decisions/count')) json = { open: large ? 99999 : 0 };
              else if (p.endsWith('/attention')) json = { waiting: large ? Array.from({ length: 1234 }, (_, i) =>
                ({ key: `decision:${i}`, kind: 'decision', title: `Header waiting ${i}`, at: 1 })) : [], worth: [], elsewhere: [] };
              else if (p.endsWith('/projects') || p.endsWith('/work')) json = large ? Array.from({ length: 123 }, (_, i) => ({ ...row, id: `header-${i}` })) : [];
              if (json !== undefined) return route.fulfill({ json });
              return route.fallback();
            });
          },
        });
        const { page } = f;
        await page.locator('header summary > span.font-medium:visible').filter({ hasText: large ? 'Carbon6 research and development workspace' : 'Home' }).waitFor();
        if (large) for (const label of [/^Decisions99999$/, /^Projects123$/, /^Work123$/]) {
          await page.locator('header a').filter({ hasText: label }).first().waitFor({ state: 'attached' });
        }
        if (large) await page.locator('header button[aria-label^="What needs you: 1234 waiting"]:visible').waitFor();
        const report = [];
        const retained = await page.locator('header nav a').first().elementHandle();
        const icons = await page.locator('header svg').count();
        for (const width of widths) {
          await page.setViewportSize({ width, height: 1000 });
          await ready(page);
          const result = await geometry(page);
          report.push({ width, ...result });
          const label = `${engine}-${theme}-${large ? 'large' : 'short'}-${width}`;
          await page.screenshot({ path: path.join(shots, `${phase}-header-${label}.png`) });
          if (!before) {
            assert.deepEqual(result.overlaps, [], `${label}: intersecting header controls`);
            assert.deepEqual(result.overflow, [], `${label}: overflowing header controls`);
            assert(result.realm.width >= (large ? 175 : 30), `${label}: realm name collapsed: ${JSON.stringify(result.realm)}`);
            if (!large) assert(!result.realm.clipped, `${label}: short realm clipped`);
            if (result.moved.length) assert(result.menu, `${label}: moved items need a menu`);
            assert(await retained.evaluate(el => el.isConnected), 'resizing retains navigation nodes');
            assert.equal(await page.locator('header svg').count(), icons, 'fitting never drops an icon');
          }
          console.log(`${phase} header ${label}: ${result.overlaps.length} overlaps, ${result.overflow.length} overflows, moved ${result.moved.join(', ')}`);
          // The search's old focus:w-64 must not consume unreserved space.
          const search = page.locator('header input[type=search]:visible');
          if (await search.count()) {
            await search.focus(); await ready(page);
            if (!before) {
              const focused = await geometry(page);
              assert.deepEqual(focused.overlaps, [], `${label}: focused search overlaps`);
              assert.deepEqual(focused.overflow, [], `${label}: focused search overflows`);
            }
            await search.blur();
          }
        }
        if (!before) {
          // Resize back down and open the actual live navigation drawer.
          // Its matching destinations preserve counts; no nodes are reparented.
          await page.setViewportSize({ width: 768, height: 1000 }); await ready(page);
          const moved = await page.locator('[data-header-overflow]').evaluateAll(els => els.map(el => ({ key: el.dataset.headerItem,
            count: el.querySelector('[data-header-count]')?.textContent.trim() })));
          const menuButton = page.locator('[data-header-menu] > button');
          await menuButton.click();
          const drawer = page.locator('[data-header-menu] .phone-drawer');
          for (const item of moved.filter(item => item.count)) {
            const target = drawer.locator(`[data-header-destination="${item.key}"]`);
            assert.equal(await target.locator('[data-header-count]').textContent(), item.count, `${item.key} keeps its count in the menu`);
          }
          const counted = moved.some(item => item.count);
          assert.equal(await page.locator('[data-header-menu]').getAttribute('data-header-counted'), counted ? 'true' : null);
          assert.equal(await page.locator('[data-header-overflow-badge]').isVisible(), counted, 'visible dot reports moved counts');
          if (counted) {
            const total = moved.reduce((sum, item) => sum + (Number(item.count) || 0), 0);
            const description = `${total} waiting on navigation items in this menu`;
            assert.match(await menuButton.getAttribute('aria-describedby'), /c6-header-overflow-state/);
            assert.equal(await menuButton.getAttribute('title'), description);
            assert.equal(await page.locator('[data-header-overflow-note]').textContent(), description);
          }
          await page.screenshot({ path: path.join(shots, `${phase}-header-${engine}-${theme}-${large ? 'large' : 'short'}-menu.png`) });
          await page.setViewportSize({ width: 2000, height: 1000 }); await ready(page);
          assert(await drawer.isVisible(), 'widening must not hide an open drawer');
          await page.keyboard.press('Escape');
          await ready(page);
          if (!large) await page.locator('[data-header-item=search] input').evaluate(el => {
            if (document.activeElement !== el) throw Error('closing a no-longer-needed menu should return focus to visible search');
          });
          await menuButton.focus();
          // Test the actual app command on both browser platform identities,
          // including when the ordinary header input is inert offscreen.
          for (const platform of ['MacIntel', 'Linux x86_64']) {
            await page.evaluate(platform => Object.defineProperty(navigator, 'platform', { configurable: true, get: () => platform }), platform);
            for (const width of [768, 2000]) {
              await page.setViewportSize({ width, height: 1000 }); await ready(page);
              await page.keyboard.press(platform === 'MacIntel' ? 'Meta+k' : 'Control+k');
              await page.waitForFunction(() => document.activeElement?.matches('input[type=search]') && !document.activeElement.closest('[inert]'));
              const active = await page.evaluate(() => ({ drawer: !!document.activeElement.closest('.phone-drawer'),
                hidden: !!document.querySelector('[data-header-item=search][inert]') }));
              assert.equal(active.drawer, active.hidden, 'command focuses the visible search, opening its drawer only if needed');
              await page.keyboard.press('Escape');
              if (active.drawer) await menuButton.evaluate(el => { if (document.activeElement !== el) throw Error('Escape should focus the menu opener'); });
            }
          }
          await page.setViewportSize({ width: 2000, height: 1000 }); await ready(page);
          const narrowMoved = report.find(row => row.width === 768).moved;
          const focusTarget = page.locator(narrowMoved.map(key => `[data-header-item="${key}"]:not([inert]) a`).join(', ')).first();
          await focusTarget.focus();
          await focusTarget.evaluate(el => { if (document.activeElement !== el) throw Error('resize regression must start on a focused visible link'); });
          await page.setViewportSize({ width: 768, height: 1000 }); await ready(page);
          await menuButton.evaluate(el => { if (document.activeElement !== el) throw Error('overflowed focus should move to the menu'); });
          // All-space state must actually use it. Hidden items remain measurable,
          // so growing is independent of the previous compressed state.
          await page.setViewportSize({ width: 2000, height: 1000 }); await ready(page);
          if (!large) assert.equal((await geometry(page)).moved.length, 0, 'short header uses ample space');
          const notification = page.locator('header button[aria-label^="Notifications"]:visible');
          assert.equal(await notification.locator('svg').count(), 1, 'notification icon retained');
          assert.equal((await notification.textContent()).trim(), '', 'notifications are icon-only');
          assert.equal(await notification.getAttribute('aria-label'), await notification.getAttribute('title'));
          // No polling/continuous measurement after everything is quiescent.
          const mutations = await page.evaluate(async () => {
            const records = [], header = document.querySelector('header');
            const observer = new MutationObserver(rs => records.push(...rs));
            observer.observe(header, { attributes: true, subtree: true });
            for (let i = 0; i < 12; i++) await new Promise(requestAnimationFrame);
            observer.disconnect();
            return records.map(r => r.attributeName);
          });
          assert.deepEqual(mutations, [], 'settled header does not cycle layout attributes');
        }
        fs.writeFileSync(path.join(shots, `${phase}-header-${engine}-${theme}-${large ? 'large' : 'short'}-geometry.json`), JSON.stringify(report, null, 2));
        assert.deepEqual(f.errors, [], 'no WASM errors or observer loops');
      } finally { await f?.context.close(); await browser.close(); }
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
}
within('adaptive header regression', main).catch(error => { console.error(error.stack || error); process.exitCode = 1; });
