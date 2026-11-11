#!/usr/bin/env node
'use strict';

// Real production WASM and loopback fixtures; no builds, services or DOM stand-ins.
// BEFORE=1 records the baseline geometry/ancestor diagnosis instead of demanding a fix.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');
const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
const before = process.env.BEFORE === '1';
const shots = process.env.SCREENSHOTS || '/tmp/q2-screenshots';
const DOC = '00000000-0000-0000-0000-000000000042';
const svg = '<svg xmlns="http://www.w3.org/2000/svg" width="960" height="600" viewBox="0 0 960 600"><rect width="960" height="600" fill="#dbeafe"/><circle cx="480" cy="280" r="180" fill="#2563eb"/><text x="480" y="540" text-anchor="middle" font-size="48">Full-window preview fixture</text></svg>';
const image = Buffer.from(svg);
const doc = { id: DOC, realm: 'home', kind: 'doc', title: 'Preview drawing', labels: [], rev: 1,
  created_by: 'you', created_at: 1, updated_by: 'you', updated_at: 1, pending: 0,
  tabs: [{ id: 'tab.1', title: 'Drawing', blocks: [{ id: 'drawing.1', kind: 'embed', text: '',
    attrs: { type: 'diagram', title: 'Document drawing', svg }, hash: 'drawing-hash', by: 'you', rev: 1 }] }] };
const views = {
  'desktop-wide': { viewport: { width: 1440, height: 1000 }, tiles: 1 },
  'desktop-narrow': { viewport: { width: 1440, height: 1000 }, tiles: 3 },
  'phone': { viewport: { width: 390, height: 844 }, tiles: 1 },
  'phone-narrow': { viewport: { width: 320, height: 568 }, tiles: 1 },
};
function layout(count, drawing = false) {
  return { tiles: Array.from({ length: count }, (_, i) => ({ key: 2 + i * 2,
    columns: [{ key: 3 + i * 2, view: i ? { view: 'sessions' } : drawing ? { view: 'docs', id: DOC } : { view: 'session', id: 'parent' } }],
    cursor: 0, active: 3 + i * 2, used: 3 + i * 2, back: [], forward: [], stack: null, minimized: false })),
  focus: 0, next: 2 + count * 2, full: false };
}

async function geometry(overlay) {
  return overlay.evaluate(el => {
    const box = node => { const r = node.getBoundingClientRect(); return { x: r.x, y: r.y, width: r.width, height: r.height }; };
    const ancestors = [];
    for (let p = el.parentElement; p; p = p.parentElement) {
      const s = getComputedStyle(p);
      ancestors.push({ tag: p.tagName, classes: p.className, box: box(p), contain: s.contain,
        contentVisibility: s.contentVisibility, containerType: s.containerType, transform: s.transform,
        filter: s.filter, willChange: s.willChange, overflow: s.overflow });
    }
    const points = [[2, 2], [innerWidth - 3, 2], [2, innerHeight - 3], [innerWidth - 3, innerHeight - 3]];
    return { box: box(el), viewport: { width: innerWidth, height: innerHeight },
      inTile: !!el.closest('[data-tile-motion]'), ancestors,
      hits: points.map(([x, y]) => el.contains(document.elementFromPoint(x, y))),
      image: box(el.querySelector('img')), captionPx: getComputedStyle(el.querySelector('span')).fontSize,
      buttonPx: getComputedStyle(el.querySelector('button')).fontSize };
  });
}

async function check(browser, origin, engine, name, theme) {
  let f;
  const label = `${engine}-${name}-${theme}`;
  const phase = before ? 'before' : 'after';
  const results = [];
  try {
    let page;
    const phaseLog = phase => { f.phase = phase; console.log(`  ${label}: ${phase}`); };
    const seed = async drawing => {
      f = await fixture(browser, origin, views[name].viewport, theme,
        { choice: 'off', effective: 'off', os: 'no-preference' }, value => { f = value; }, {}, {
          layout: layout(views[name].tiles, drawing),
          path: drawing ? `/docs/${DOC}` : '/sessions/parent',
          ready: drawing ? 'img[alt="Document drawing"]' : 'textarea',
          setup: async ({ context }) => context.route('**/api/**', async route => {
            const p = new URL(route.request().url()).pathname;
            if (p.includes('/attachments/')) return route.fulfill({ contentType: 'image/svg+xml', body: svg });
            if (p.endsWith(`/docs/${DOC}`)) return route.fulfill({ json: doc });
            if (p.endsWith('/docs')) return route.fulfill({ json: [{ ...doc, tabs: undefined }] });
            return route.fallback();
          }),
        });
      page = f.page;
      phaseLog(`mounted ${drawing ? 'drawing' : 'session'}`);
      if (!drawing) {
        await page.locator('#col-3 textarea').waitFor();
        await page.locator('[id="parent-e64"]').waitFor({ state: 'attached' });
      }
      await settle(page);
    };
    await seed(false);
    phaseLog('transcript event');
    f.send({ type: 'user', command_id: 'preview-image', text: 'A transcript image', attachments: [{ name: 'Transcript image', mime: 'image/svg+xml', size: image.length }] });
    const transcript = page.getByRole('link').filter({ has: page.getByRole('img', { name: 'Transcript image', exact: true }) });
    await transcript.waitFor();
    const overlay = () => page.locator('div.fixed.inset-0').filter({ has: page.getByRole('button', { name: 'Close', exact: true }) }).filter({ has: page.locator('img') });
    const preview = async (caller, opener) => {
      phaseLog(`${caller} preview`);
      if (!before && caller === 'drawing') {
        await opener.focus();
        await opener.press('Enter');
      } else await opener.click();
      const modal = overlay();
      await modal.waitFor();
      await page.waitForFunction(() => [...document.querySelectorAll('div.fixed.inset-0 img')].every(i => i.complete && i.naturalWidth > 0));
      const metrics = { caller, ...await geometry(modal) };
      await page.screenshot({ path: path.join(shots, `${phase}-${label}-${caller}.png`) });
      if (before) {
        // One-property-at-a-time experiment on the real ancestors. Restore each
        // immediately: screenshots and subsequent callers use untouched CSS.
        metrics.experiments = await modal.evaluate(async el => {
          const out = [], rect = () => { const r = el.getBoundingClientRect(); return [r.x, r.y, r.width, r.height]; };
          const paint = () => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
          for (let p = el.parentElement; p && p !== document.body; p = p.parentElement) {
            const s = getComputedStyle(p);
            for (const [prop, reset] of [['content-visibility', 'visible'], ['container-type', 'normal'], ['contain', 'none'], ['transform', 'none']]) {
              const original = s.getPropertyValue(prop);
              if (original === reset) continue;
              const saved = p.getAttribute('style');
              const previous = rect();
              p.style.setProperty(prop, reset, 'important');
              await paint();
              const next = rect();
              if (JSON.stringify(previous) !== JSON.stringify(next)) out.push({ classes: p.className, prop, original, previous, next });
              if (saved === null) p.removeAttribute('style'); else p.setAttribute('style', saved);
              await paint();
            }
          }
          return out;
        });
        assert(metrics.inTile, `${caller}: baseline still renders within a tile`);
        if (caller === 'transcript') {
          assert(metrics.box.height < metrics.viewport.height, 'baseline preview is constrained to its transcript row, including wide tiles');
          assert(metrics.experiments.some(e => e.prop === 'content-visibility'), 'removing row content-visibility changes preview geometry');
        }
      } else {
        assert(!metrics.inTile, `${caller}: viewport overlay is outside every tile`);
        for (const [key, expected] of Object.entries({ x: 0, y: 0, ...metrics.viewport })) {
          assert(Math.abs(metrics.box[key] - expected) < 1, `${caller}: ${key} fills window: ${JSON.stringify(metrics.box)}`);
        }
        assert(metrics.hits.every(Boolean), `${caller}: scrim receives input in all window corners`);
        assert.equal(metrics.captionPx, '14px', 'caption keeps explicit text-sm outside tile');
        assert.equal(metrics.buttonPx, '12px', 'actions keep explicit text-xs outside tile');
        assert(metrics.image.x >= 0 && metrics.image.y >= 0 && metrics.image.x + metrics.image.width <= metrics.viewport.width + 1
          && metrics.image.y + metrics.image.height <= metrics.viewport.height + 1,
          `${caller}: image fits the window`);
        await modal.locator('img').click();
        await modal.locator('span').first().click();
        assert.equal(await modal.count(), 1, 'image and caption clicks do not dismiss the preview');
      }
      results.push(metrics);
      // All dismissal paths on all three real callsites, with focus retained.
      await page.keyboard.press('Escape');
      await modal.waitFor({ state: 'detached' });
      if (before) return;
      if (!before) await assertFocus(opener);
      await opener.click(); await modal.waitFor();
      await modal.getByRole('button', { name: 'Close', exact: true }).click();
      await modal.waitFor({ state: 'detached' });
      if (!before) await assertFocus(opener);
      if (caller === 'drawing') {
        await opener.press('Space');
        await modal.waitFor();
        await page.keyboard.press('Escape');
        await modal.waitFor({ state: 'detached' });
        await assertFocus(opener);
      }
      await opener.click(); await modal.waitFor();
      await modal.click({ position: { x: 2, y: 2 } });
      await modal.waitFor({ state: 'detached' });
      if (!before) await assertFocus(opener);
    };
    await preview('transcript', transcript);
    await page.locator('#col-3 input[type=file]').setInputFiles({ name: 'Pending image.svg', mimeType: 'image/svg+xml', buffer: image });
    const attachment = page.getByRole('button', { name: 'See it full size', exact: true });
    await attachment.waitFor();
    await preview('composer', attachment);
    assert.equal(await attachment.count(), 1, 'preview dismissal does not remove the attachment');
    if (!before) {
      await attachment.click();
      await overlay().getByRole('button', { name: 'Remove', exact: true }).click();
      await overlay().waitFor({ state: 'detached' });
      assert.equal(await attachment.count(), 0, 'Remove still removes the previewed attachment');
      assert(await page.locator('#col-3 textarea').evaluate(el => document.activeElement === el), 'removed opener falls back to its composer');
      // Audit representatives outside FullImage too: the tile's shared
      // Popover and the session-details viewport modal escape together.
      await page.locator('[data-tile-motion="2"]').getByRole('button', { name: 'Actions', exact: true }).click();
      const menu = page.locator('.c6-pop:visible').last();
      await assertEscaped(menu, 'tile actions');
      await menu.getByRole('button', { name: /^Session details…/ }).click();
      const details = page.locator('div.fixed.inset-0').filter({ has: page.getByRole('heading', { name: 'Session details', exact: true }) });
      await assertEscaped(details, 'session details');
      await details.getByRole('button', { name: 'Close', exact: true }).click();
      await details.waitFor({ state: 'detached' });
    }
    assert.deepEqual(f.errors, [], 'session has no browser crashes or external requests');
    await f.context.close();
    await seed(true);
    const drawing = page.getByRole('img', { name: 'Document drawing', exact: true }).locator('..');
    await drawing.waitFor();
    await preview('drawing', drawing);
    assert.equal(await page.locator('[contenteditable=true]').count(), 0, 'drawing preview does not start editing the block');
    if (!before && name.startsWith('phone')) {
      await page.locator('[data-tile-bar]:visible').getByRole('button', { name: 'Actions', exact: true }).click();
      await page.locator('.phone-popover:visible').getByRole('menuitem', { name: 'Show comments', exact: true }).click();
      const sheet = page.locator('.phone-sheet:visible');
      await assertEscaped(sheet, 'document comments sheet', true);
      await page.keyboard.press('Escape');
      await sheet.waitFor({ state: 'detached' });
    }
    assert.deepEqual(f.errors, [], 'no browser crashes or external requests');
    fs.writeFileSync(path.join(shots, `${phase}-${label}.json`), JSON.stringify(results, null, 2) + '\n');
    console.log(JSON.stringify({ label, phase, results: results.map(({ caller, box, inTile, hits, experiments }) => ({ caller, box, inTile, hits, experiments })) }));
  } catch (error) {
    await f?.page?.screenshot({ path: path.join(shots, `${phase}-${label}-failure.png`) }).catch(() => {});
    console.error(`FAIL ${label} during ${f?.phase}`, f?.errors, f?.consoleMessages);
    throw error;
  } finally { await f?.context?.close(); }
}
async function assertFocus(opener) {
  assert(await opener.evaluate(el => document.activeElement === el), 'dismissal returns focus to the opener');
}
async function assertEscaped(surface, label, tileLocal = false) {
  await surface.waitFor();
  const result = await surface.evaluate(el => {
    const r = el.getBoundingClientRect(), s = getComputedStyle(el);
    // c6-sheet deliberately paints a padded overshoot tail below the window;
    // its negative margin cancels that tail, not any interactive content.
    const tail = Math.min(0, parseFloat(s.marginBottom) || 0);
    const tile = el.closest('[data-tile-surface]')?.getBoundingClientRect();
    return { inTile: !!el.closest('[data-tile-motion]'), tile: tile && { left: tile.left, right: tile.right, top: tile.top, bottom: tile.bottom }, left: r.left, right: r.right,
      top: r.top, bottom: r.bottom + tail, width: innerWidth, height: innerHeight };
  });
  if (tileLocal) {
    assert(result.inTile && result.tile, `${label}: comments deliberately mount in the owning tile`);
    assert(result.left >= result.tile.left - 1 && result.right <= result.tile.right + 1
      && result.top >= result.tile.top - 1 && result.bottom <= result.tile.bottom + 1,
      `${label}: sheet is contained by the tile: ${JSON.stringify(result)}`);
  } else assert(!result.inTile, `${label}: fixed surface escapes the tile`);
  assert(result.left >= -1 && result.right <= result.width + 1 && result.top >= -1 && result.bottom <= result.height + 1,
    `${label}: surface stays within the window: ${JSON.stringify(result)}`);
}
async function main() {
  fs.mkdirSync(shots, { recursive: true });
  const { server, origin } = await serve();
  try {
    const engines = process.env.ENGINE === 'both' ? ['chromium', 'webkit'] : [process.env.ENGINE || 'chromium'];
    for (const engine of engines) {
      assert(['chromium', 'webkit'].includes(engine), `Unknown ENGINE=${engine}`);
      for (const name of (process.env.VIEWPORT || Object.keys(views).join(',')).split(',')) {
        assert(views[name], `Unknown VIEWPORT=${name}`);
        for (const theme of (process.env.THEME || 'light,dark').split(',')) {
          assert(['light', 'dark'].includes(theme), `Unknown THEME=${theme}`);
          console.log(`RUN ${engine}-${name}-${theme}`);
          // Isolate browser/WASM state as well as storage. This is not a
          // retry: any WPE out-of-bounds trap still fails the scenario.
          const browser = await launchBrowser(playwright, engine);
          try {
            console.log(`  browser ${browser.version()}`);
            await within(`${engine}-${name}-${theme}`, () => check(browser, origin, engine, name, theme));
          } finally { await browser.close(); }
        }
      }
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
}
main().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
