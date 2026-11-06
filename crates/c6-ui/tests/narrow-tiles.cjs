#!/usr/bin/env node
'use strict';

// Production UI, fake loopback head, real container sizes in a desktop window.
// BEFORE=1 records the pinned baseline without enforcing the new invariants.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');
const { install, views } = require('./narrow-fixtures.cjs');
const shots = process.env.SCREENSHOTS || '/tmp/p3-screenshots';
const before = process.env.BEFORE === '1';
const phase = before ? 'before' : 'after';
const motion = { choice: 'off', effective: 'off', os: 'no-preference' };
const selectedViews = () => process.env.VIEWS?.split(',') || [...new Set(['session', 'settings/appearance', 'sessions', ...views, 'inbox', 'mission', 'workers'])];

async function geometry(page, index = 0) {
  return page.evaluate(index => {
    const tile = document.querySelectorAll('[data-tile-motion] section')[index];
    const bounds = tile.getBoundingClientRect();
    const visible = el => el.getClientRects().length && getComputedStyle(el).visibility !== 'hidden';
    const overflow = [...tile.querySelectorAll('*')].filter(el => {
      if (!visible(el) || el.closest('[inert], .fixed, .c6-pop, .truncate, pre, .overflow-x-auto')) return false;
      const r = el.getBoundingClientRect();
      return r.width > 0 && (r.right > bounds.right + 1 || r.left < bounds.left - 1);
    }).slice(0, 20).map(el => ({ tag: el.tagName, text: el.textContent.slice(0, 70), class: String(el.className) }));
    const radios = [...tile.querySelectorAll('input[type=radio]')].filter(visible).map(el => {
      const r = el.getBoundingClientRect(), label = el.nextElementSibling?.getBoundingClientRect();
      return { overlap: label && r.right > label.left + 1, width: r.width };
    });
    const bar = tile.firstElementChild;
    const controls = [...bar.querySelectorAll('button')].filter(visible).map(el => ({
      label: el.getAttribute('aria-label'), width: el.getBoundingClientRect().width,
      left: el.getBoundingClientRect().left, right: el.getBoundingClientRect().right,
    }));
    const overlaps = controls.flatMap((a, i) => controls.slice(i + 1).filter(b => Math.min(a.right, b.right) - Math.max(a.left, b.left) > 1).map(b => [a.label, b.label]));
    return { width: bounds.width, left: bounds.left, right: bounds.right, title: bar.textContent.trim(), content: tile.querySelector('[data-pane-motion]')?.textContent.slice(0, 100), overflow, radios, controls, overlaps, documentOverflow: document.documentElement.scrollWidth > innerWidth };
  }, index);
}

async function overlayBounds(locator) {
  await locator.waitFor();
  const result = await locator.evaluate(el => {
    const r = el.getBoundingClientRect();
    const x = Math.max(1, Math.min(innerWidth - 1, r.left + r.width / 2));
    const y = Math.max(1, Math.min(innerHeight - 1, r.top + Math.min(r.height / 2, 40)));
    return { left: r.left, right: r.right, top: r.top, width: innerWidth, bottom: r.bottom, height: innerHeight,
      hit: el.contains(document.elementFromPoint(x, y)) };
  });
  assert(result.left >= -1 && result.right <= result.width + 1 && result.top >= -1 && result.hit,
    `overlay offscreen or clipped: ${JSON.stringify(result)}`);
}

async function sessionOverlays(page, theme, width) {
  console.log(`  overlays ${theme} ${width}: actions`);
  await page.getByRole('button', { name: 'Actions', exact: true }).click();
  const menu = page.locator('.c6-pop:visible').last();
  await overlayBounds(menu);
  if (width < 480) for (const name of ['Changes', 'Terminal', 'Pin']) {
    const button = menu.getByRole('button', { name, exact: true });
    await button.scrollIntoViewIfNeeded();
    assert(await button.isVisible(), `${name} remains reachable`);
    assert(await button.locator('svg').count(), `${name} keeps its icon`);
  }
  await page.screenshot({ path: path.join(shots, `${phase}-${theme}-session-actions-${width}.png`) });
  await page.keyboard.press('Escape');
  console.log(`  overlays ${theme} ${width}: session sheet`);
  await page.getByRole('button', { name: /Open session sheet/ }).first().click();
  const dialog = page.locator('.c6-pop:visible').last();
  await overlayBounds(dialog);
  await page.keyboard.press('Escape');
  await dialog.waitFor({ state: 'detached' });
  const model = page.getByRole('button', { name: /^Model:/ }).first();
  if (await model.count()) {
    console.log(`  overlays ${theme} ${width}: model`);
    await model.click();
    assert.equal(await page.getByRole('dialog', { name: 'Model', exact: true }).count(), 0, 'narrow desktop tiles keep the viewport portal, not the phone sheet');
    const pop = page.locator('[data-pop]').filter({ has: page.getByText('Model', { exact: true }) }).last();
    await overlayBounds(pop);
    const input = pop.getByPlaceholder('model id', { exact: true });
    await input.fill('custom-model-id');
    await pop.getByRole('button', { name: 'fixture-model-32', exact: true }).click();
    assert.equal(await input.inputValue(), 'fixture-model-32', 'narrow desktop picker keeps its last model choice reachable');
    await page.keyboard.press('Escape');
  }
}

async function naturalColumns(page) {
  await page.evaluate(() => {
    const views = [{ view: 'session', id: 'parent' }, { view: 'docs', id: '' }, { view: 'workers' }, { view: 'settings', section: 'appearance' }];
    const tiles = views.map((view, i) => ({ key: i + 10, columns: [{ key: i + 20, view }], cursor: 0, active: i + 20, used: i + 20, back: [], forward: [], stack: i, minimized: false }));
    localStorage.setItem('c6.home.workspace.v2', JSON.stringify({ tiles, focus: 0, next: 40, full: false }));
  });
  await page.goto(new URL('/r/home/sessions/parent', page.url()).href, { waitUntil: 'domcontentloaded' });
  await page.locator('[data-tile-motion]').nth(3).waitFor();
  for (const width of [1600, 1024, 768]) {
    await page.setViewportSize({ width, height: 1000 });
    await settle(page);
    const canvas = await page.locator('[data-workspace-motion]').evaluate(el => ({
      width: el.getBoundingClientRect().width, parent: el.parentElement.clientWidth,
      scroll: el.parentElement.scrollWidth, min: getComputedStyle(el).minWidth,
    }));
    assert(canvas.width <= canvas.parent + 1 && canvas.scroll <= canvas.parent + 1, `no sideways canvas at ${width}: ${JSON.stringify(canvas)}`);
    assert.equal(canvas.min, '0px');
    for (let index = 0; index < 4; index++) {
      const result = await geometry(page, index);
      assert.deepEqual(result.overlaps, [], `natural narrow bar ${index} at ${width}`);
      assert(result.controls.every(c => c.left >= result.left - 1 && c.right <= result.right + 1), 'controls fit the tile');
    }
  }
}

async function viewForm(page, view, name) {
  if (view === 'sessions') {
    await page.getByRole('button', { name: 'Filter, sort and group', exact: true }).click();
    await overlayBounds(page.getByRole('dialog', { name: 'Filter, sort and group', exact: true }));
    await page.keyboard.press('Escape');
    const change = page.locator('button[title="Change where and how it runs"]');
    await change.click();
    await settle(page);
    assert.deepEqual((await geometry(page)).overflow, [], `${name}: new session fields fit`);
    await change.click();
    return;
  }
  const controls = {
    docs: ['New doc', 'Cancel'], work: ['Add', 'Cancel'], projects: ['New project', 'Cancel'],
    calendar: ['New event', 'Cancel'], mail: ['Compose', 'Discard'],
  }[view];
  if (!controls) return;
  await page.getByRole('button', { name: controls[0], exact: true }).first().click();
  await settle(page);
  const result = await geometry(page);
  await page.locator('[data-tile-motion]').first().screenshot({ path: path.join(shots, `${name}-form.png`) });
  assert.deepEqual(result.overflow, [], `${name}: open form fits`);
  await page.getByRole('button', { name: controls[1], exact: true }).first().click();
  if (view === 'calendar') {
    for (const mode of ['Month', 'Agenda', 'Week']) {
      await page.getByRole('button', { name: mode, exact: true }).click();
      await settle(page);
      assert.deepEqual((await geometry(page)).overflow, [], `${name}: ${mode} fits`);
      await page.locator('[data-tile-motion]').first().screenshot({ path: path.join(shots, `${name}-${mode.toLowerCase()}.png`) });
    }
  }
}

async function splitPanes(page, theme) {
  const pairs = [
    ['settings', { view: 'sessions' }, { view: 'settings', section: 'appearance' }, 'settings/appearance'],
    ['docs', { view: 'docs', id: '' }, { view: 'docs', id: 'narrow-doc' }, 'docs/narrow-doc'],
    ['session', { view: 'sessions' }, { view: 'session', id: 'parent' }, 'sessions/parent'],
  ];
  for (const [name, list, item, route] of pairs) for (const width of [240, 320, 480, 640, 1024]) {
    console.log(`  split ${theme} ${name} ${width}`);
    await page.evaluate(({ list, item }) => {
      localStorage.setItem('c6.home.workspace.v2', JSON.stringify({
        tiles: [{ key: 2, columns: [{ key: 3, view: list }, { key: 4, view: item }], cursor: 1, active: 4, used: 4, back: [], forward: [], stack: 0, minimized: false }],
        focus: 0, next: 5, full: false,
      }));
    }, { list, item });
    await page.goto(new URL(`/r/home/${route}`, page.url()).href, { waitUntil: 'domcontentloaded' });
    await page.locator('[data-pane-motion="4"]').waitFor();
    await page.locator('[data-tile-motion]').evaluate((el, w) => { el.style.width = `${w + 8}px`; }, width);
    await settle(page);
    const panes = await page.locator('[data-pane-motion]').evaluateAll(nodes => nodes.filter(el => el.getBoundingClientRect().width).map(pane => {
      const bounds = pane.getBoundingClientRect();
      const overflow = [...pane.querySelectorAll('*')].filter(el => {
        if (!el.getClientRects().length || el.closest('[inert], .fixed, .c6-pop, .truncate, pre, .overflow-x-auto')) return false;
        const r = el.getBoundingClientRect();
        return r.width && (r.right > bounds.right + 1 || r.left < bounds.left - 1);
      }).slice(0, 10).map(el => ({ tag: el.tagName, text: el.textContent.slice(0, 60) }));
      return { width: bounds.width, overflow };
    }));
    assert.equal(panes.length, width <= 640 ? 1 : 2, `${name} at ${width}: existing single-pane navigation below 40rem`);
    for (const pane of panes) assert.deepEqual(pane.overflow, [], `${name} at ${width}: each pane fits its own bounds`);
    await page.locator('[data-tile-motion]').screenshot({ path: path.join(shots, `${phase}-${theme}-split-${name}-${width}.png`) });
    if (width <= 640) {
      await page.getByRole('button', { name: 'Back to the list', exact: true }).click();
      assert(await page.locator('[data-pane-motion="3"]').isVisible(), 'list remains reachable');
      assert(!(await page.locator('[data-pane-motion="4"]').isVisible()), 'item stays mounted but hidden');
      assert(await page.locator('[data-pane-motion="4"]').count(), 'switching never discards the item');
    }
  }
}

async function main() {
  fs.mkdirSync(shots, { recursive: true });
  const playwright = require(process.env.PLAYWRIGHT_CORE || '/home/zheilbron/.bun/install/cache/playwright-core@1.50.1@@@1');
  const { server, origin } = await serve();
  let browser;
  const newFixture = async (viewport, theme, options = {}) => {
    // Keep the shared-process default so runtime reuse failures stay visible.
    // Like motion.cjs, isolation is explicit and never an automatic retry.
    if (!browser || process.env.ISOLATE_BROWSER === '1') {
      await browser?.close();
      browser = await launchBrowser(playwright, process.env.ENGINE || 'chromium');
    }
    return fixture(browser, origin, viewport, theme, motion, () => {}, options);
  };
  const results = [];
  try {
    for (const theme of ['light', 'dark']) {
      const f = await newFixture({ width: 1600, height: 1000 }, theme);
      await install(f.context);
      try {
        for (const view of selectedViews()) {
          console.log(`${phase} ${theme}: ${view}`);
          await f.page.evaluate(() => localStorage.removeItem('c6.home.workspace.v2'));
          await f.page.goto(`${origin}/r/home/${view === 'session' ? 'sessions/parent' : view}`, { waitUntil: 'domcontentloaded' });
          await f.page.locator('[data-tile-motion] section').first().waitFor();
          assert.equal(await f.page.locator('[data-tile-motion]').count(), 1, `${view}: isolated tile`);
          if (view === 'settings/appearance') await f.page.locator('input[name=theme]').first().waitFor();
          if (view === 'session') await f.page.locator('textarea').first().waitFor();
          await f.page.waitForFunction(() => {
            const pane = document.querySelector('[data-pane-motion]');
            return pane?.textContent.length && !pane.textContent.includes('Loading…');
          });
          for (const width of [240, 320, 400, 480, 640]) {
            console.log(`  width ${width}`);
            // Resize the real tile; leave the window wide. All production
            // container queries and ResizeObservers see its actual width.
            await f.page.locator('[data-tile-motion]').first().evaluate((el, w) => { el.style.width = `${w + 8}px`; }, width);
            await settle(f.page);
            const result = await geometry(f.page);
            const name = `${phase}-${theme}-${view.replaceAll('/', '-')}-${width}`;
            await f.page.locator('[data-tile-motion]').first().screenshot({ path: path.join(shots, `${name}.png`) });
            results.push({ name, ...result });
            if (!before && !process.env.AUDIT) {
              assert(!result.documentOverflow, `${name}: workspace overflow`);
              assert.deepEqual(result.overflow, [], `${name}: elements outside tile`);
              assert.deepEqual(result.overlaps, [], `${name}: header controls overlap`);
              assert.deepEqual(result.controls.slice(-3).map(c => c.label), ['Actions', 'Focus', 'Close this tile'], `${name}: Maximize remains immediately left of Close`);
              assert(result.radios.every(r => !r.overlap && r.width > 0), `${name}: radio overlap`);
              if (view === 'session' && !process.env.SKIP_OVERLAYS) await sessionOverlays(f.page, theme, width);
              if (width === 240 && !process.env.SKIP_OVERLAYS) await viewForm(f.page, view, name);
            }
            if (view === 'docs/narrow-doc') {
              await f.page.locator('button[title="Show the comments"]').click();
              await settle(f.page);
              const panel = f.page.locator('.c6-sheet[data-open="true"], .c6-pop[data-open="true"]').last();
              if (!before && !process.env.AUDIT) await overlayBounds(panel);
              await f.page.screenshot({ path: path.join(shots, `${name}-comments.png`) });
              await f.page.keyboard.press('Escape');
            }
          }
        }
        assert.deepEqual(f.errors, [], 'no browser crashes');
      } finally {
        if (f.errors.length) console.error('Browser console:', JSON.stringify(f.consoleMessages.slice(-20)));
        await f.context.close();
      }
      if (!before && !process.env.AUDIT) {
        const split = await newFixture({ width: 1600, height: 1000 }, theme);
        await install(split.context);
        try {
          await Promise.race([splitPanes(split.page, theme), split.crash]);
          assert.deepEqual(split.errors, [], 'no split-pane browser crashes');
        } finally {
          if (split.errors.length) console.error('Browser console:', JSON.stringify(split.consoleMessages.slice(-20)));
          await split.context.close();
        }
      }
    }
    for (const viewport of [{ width: 320, height: 568 }, { width: 390, height: 844 }, { width: 900, height: 480 }]) {
      for (const theme of ['light', 'dark']) {
        const f = await newFixture(viewport, theme);
        await install(f.context);
        try {
          for (const view of selectedViews()) {
            await f.page.evaluate(() => localStorage.removeItem('c6.home.workspace.v2'));
            await f.page.goto(`${origin}/r/home/${view === 'session' ? 'sessions/parent' : view}`, { waitUntil: 'domcontentloaded' });
            await f.page.locator('[data-tile-motion] section').first().waitFor();
            await f.page.waitForFunction(() => {
              const pane = document.querySelector('[data-pane-motion]');
              return pane?.textContent.length && !pane.textContent.includes('Loading…');
            });
            const result = await geometry(f.page);
            const name = `${phase}-${theme}-phone-${viewport.width}x${viewport.height}-${view.replaceAll('/', '-')}`;
            await f.page.screenshot({ path: path.join(shots, `${name}.png`) });
            results.push({ name, ...result });
            if (!before && !process.env.AUDIT) {
              assert(!result.documentOverflow, name);
              assert.deepEqual(result.overflow, [], name);
              assert.deepEqual(result.overlaps, [], name);
            }
          }
          assert.deepEqual(f.errors, []);
        } finally { await f.context.close(); }
      }
    }
    if (!before && !process.env.AUDIT) for (const theme of ['light', 'dark']) {
      const f = await newFixture({ width: 1600, height: 1000 }, theme, { hasTouch: true, isMobile: false });
      try {
        for (const width of [132, 160, 240, 320, 400, 480, 640]) {
          await f.page.locator('[data-tile-motion]').first().evaluate((el, w) => { el.style.width = `${w + 8}px`; }, width);
          const result = await geometry(f.page);
          assert.deepEqual(result.overlaps, [], `coarse bar at ${width}`);
          assert(result.controls.every(c => c.left >= result.left - 1 && c.right <= result.right + 1), `coarse controls fit at ${width}`);
          assert(result.controls.every(c => c.label === 'Its mark' || c.width >= 44), 'coarse targets stay 44px');
          assert.deepEqual(result.controls.slice(width < 224 ? -1 : -3).map(c => c.label), width < 224 ? ['Actions'] : ['Actions', 'Focus', 'Close this tile'], `coarse control order at ${width}`);
        }
        await naturalColumns(f.page);
        assert.deepEqual(f.errors, []);
      } finally { await f.context.close(); }
    }
  } finally {
    fs.writeFileSync(path.join(shots, `${phase}-geometry.json`), JSON.stringify(results, null, 2));
    await browser?.close(); await new Promise(resolve => server.close(resolve));
  }
  console.log(`${phase}: ${results.length} tile/theme/view cases checked`);
}
within('narrow tile browser matrix', main).catch(error => { console.error(error); process.exitCode = 1; });
