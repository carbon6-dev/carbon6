#!/usr/bin/env node
'use strict';

// Production WASM, private loopback fixture, no head, services or live settings.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');
const playwright = require(process.env.PLAYWRIGHT_CORE || '/home/zheilbron/.bun/install/cache/playwright-core@1.50.1@@@1');
const before = process.env.BEFORE === '1';
const evidence = process.env.EVIDENCE || '/tmp/q7-sheet';
const DOC = 'sheet-doc';
const OTHER_DOC = 'other-sheet-doc';
const blocks = Array.from({ length: 28 }, (_, i) => ({ id: `paragraph-${i}`, kind: 'paragraph',
  text: i === 27 ? 'This is the final block we are commenting on. Keep it visible above the sheet.'
    : `Section ${i + 1}. A document in a narrow tile, with enough reading to scroll. The other tiles should remain usable.`,
  attrs: {}, hash: `hash-${i}`, by: 'you', rev: 1 }));
const doc = { id: DOC, realm: 'home', kind: 'doc', title: 'Tile-local comments', labels: [], rev: 1,
  created_by: 'you', created_at: 1, updated_by: 'you', updated_at: 1, pending: 0,
  tabs: [{ id: 'tab-1', title: 'Reading', blocks }] };
const threads = [{ id: 'thread-1', doc: DOC, anchor: { tab: 'tab-1', block: 'paragraph-26' },
  state: 'open', about: 'An existing discussion', created_by: 'you', created_at: 1,
  comments: Array.from({ length: 8 }, (_, i) => ({ id: `comment-${i}`, by: 'you', at: 1,
    text: `Reply ${i + 1}: The comment controls and the reply box must stay reachable in a short tile.` })) }];
const cases = {
  wide: { width: 1600, height: 1000, count: 4 },
  short: { width: 1440, height: 560, count: 4 },
  phone: { width: 390, height: 844, count: 1 },
  'phone-small': { width: 320, height: 568, count: 1 },
};
function layout(count) {
  const index = count === 1 ? 0 : 1;
  return { tiles: Array.from({ length: count }, (_, i) => ({ key: 2 + i * 2,
    columns: [{ key: 3 + i * 2, view: i === index ? { view: 'docs', id: DOC }
      : count > 1 && i === count - 1 ? { view: 'docs', id: OTHER_DOC } : { view: 'session', id: 'parent' } }],
    cursor: 0, active: 3 + i * 2, used: 3 + i * 2, back: [], forward: [], stack: i, minimized: false })),
  focus: index, next: 2 + count * 2, full: false };
}
async function geometry(sheet, tile) {
  return sheet.evaluate((el, selector) => {
    const box = e => { const r = e.getBoundingClientRect(); return { left: r.left, top: r.top, right: r.right, bottom: r.bottom, width: r.width, height: r.height }; };
    const s = getComputedStyle(el), tail = Math.min(0, parseFloat(s.marginBottom) || 0);
    const r = box(el); r.bottom += tail; r.height += tail;
    const scrim = document.querySelector('.c6-comment-scrim') || el.previousElementSibling;
    return { sheet: r, tile: box(document.querySelector(selector)), scrim: box(scrim),
      fontSize: s.fontSize, lineHeight: s.lineHeight, inTile: !!el.closest('[data-tile-motion]'), viewport: innerWidth };
  }, tile);
}
function contained(r, label) {
  for (const key of ['sheet', 'scrim']) {
    const b = r[key], t = r.tile;
    assert(b.left >= t.left - 1 && b.right <= t.right + 1 && b.top >= t.top - 1 && b.bottom <= t.bottom + 1,
      `${label}: ${key} is within its tile: ${JSON.stringify(r)}`);
  }
  assert(r.sheet.height <= r.tile.height * .85 + 1, `${label}: maximum height belongs to tile`);
  assert.equal(r.fontSize, '14px', `${label}: explicit portal typography`);
}
async function check(browser, origin, engine, name, theme) {
  const { width, height, count } = cases[name], tileKey = count === 1 ? 2 : 4;
  const tileSelector = `[data-tile-motion="${tileKey}"] > section`;
  const phase = before ? 'before' : 'after', label = `${engine}-${name}-${theme}`;
  let f;
  try {
    f = await fixture(browser, origin, { width, height }, theme,
      { choice: 'off', effective: 'off', os: 'no-preference' }, value => { f = value; }, {}, {
        layout: layout(count), path: `/docs/${DOC}`, ready: '[data-block="paragraph-27"]',
        setup: async ({ context }) => context.route('**/api/**', route => {
          const p = new URL(route.request().url()).pathname;
          if (p.endsWith(`/docs/${OTHER_DOC}/threads`)) return route.fulfill({ json: [] });
          if (p.endsWith(`/docs/${OTHER_DOC}`)) return route.fulfill({ json: { ...doc, id: OTHER_DOC, title: 'Another document' } });
          if (p.endsWith(`/docs/${DOC}/threads`)) return route.fulfill({ json: threads });
          if (p.endsWith(`/docs/${DOC}`)) return route.fulfill({ json: doc });
          if (p.endsWith('/docs')) return route.fulfill({ json: [{ ...doc, tabs: undefined }] });
          return route.fallback();
        }),
      });
    const page = f.page, tile = page.locator(tileSelector);
    const sheet = page.locator('.phone-sheet[data-open="true"]');
    const dismiss = async (action, surface = sheet) => {
      const element = await surface.elementHandle();
      await action();
      // data-open becomes false before retained presence actually unmounts.
      // Focus return belongs to cleanup, not merely to losing that selector.
      await page.waitForFunction(el => !el.isConnected, element);
      await element.dispose();
    };
    const records = [];
    const capture = async caller => {
      await sheet.waitFor(); await settle(page);
      const r = await geometry(sheet, tileSelector);
      records.push({ caller, ...r });
      await page.screenshot({ path: path.join(evidence, `${phase}-${label}-${caller}.png`) });
      if (!before) {
        contained(r, `${label}/${caller}`);
        assert(await sheet.evaluate(el => {
          const s = el.getBoundingClientRect();
          return [...el.querySelectorAll('button, textarea')].every(control => {
            const r = control.getBoundingClientRect();
            return !r.width || (r.left >= s.left - 1 && r.right <= s.right + 1);
          });
        }), `${label}/${caller}: controls fit narrow sheet without horizontal clipping`);
      }
      if (count === 1) assert(Math.abs(r.sheet.width - width) < 1, 'phone sheet retains full width');
      if (!before && count > 1) {
        const other = page.locator('#col-3 textarea');
        await other.click(); await other.fill('The other tile still works');
        assert.equal(await other.inputValue(), 'The other tile still works', 'other tile receives a real click and typing');
        await other.press('Escape');
        assert.equal(await sheet.count(), 1, 'other tile use does not dismiss the sheet');
      }
    };
    const marker = async id => {
      const block = tile.locator(`[data-block="paragraph-${id}"]`);
      await block.scrollIntoViewIfNeeded(); await block.hover();
      const button = block.locator('[data-marks] button');
      await button.click();
      return button;
    };
    f.phase = 'new comment';
    const opener = await marker(27);
    if (!before) {
      await sheet.waitFor(); await settle(page);
      contained(await geometry(sheet, tileSelector), 'immediately opened sheet');
      const b = await tile.locator('[data-block="paragraph-27"]').boundingBox();
      const s = await sheet.boundingBox();
      assert(b.y >= (await tile.boundingBox()).y && b.y + b.height <= s.y + 1,
        'final block is visible immediately on opening, before draft autosizing');
    }
    await sheet.getByPlaceholder('Your comment').fill('A draft that survives closing the sheet');
    await capture('new');
    if (!before) {
      const b = await tile.locator('[data-block="paragraph-27"]').boundingBox();
      const s = await sheet.boundingBox();
      assert(b.y >= (await tile.boundingBox()).y && b.y + b.height <= s.y + 1, 'commented block is visible above sheet');
    }
    // Escape on the textarea and the sheet close affordance both preserve drafts.
    await dismiss(() => sheet.getByPlaceholder('Your comment').press('Escape'));
    if (!before) {
      const focus = await opener.evaluate(el => ({ correct: document.activeElement === el, connected: el.isConnected,
        active: document.activeElement?.outerHTML.slice(0, 500), marker: el.outerHTML }));
      assert(focus.correct, `Escape returns focus to marker: ${JSON.stringify(focus)}`);
    }
    await marker(27); await sheet.waitFor();
    assert.equal(await sheet.getByPlaceholder('Your comment').inputValue(), 'A draft that survives closing the sheet');
    await dismiss(() => sheet.getByRole('button', { name: 'Close', exact: true }).click());
    if (!before) {
      await marker(27); await sheet.waitFor();
      await dismiss(() => tile.locator('.c6-comment-scrim').click({ position: { x: 8, y: 8 } }));
      assert(await opener.evaluate(el => document.activeElement === el), 'scrim dismissal returns focus');
      await marker(27); await sheet.waitFor();
      // Switch blocks in one task, without unmounting the retained sheet.
      // The new block needs fresh placement and a fresh opener.
      await sheet.evaluate(el => {
        el.querySelector('textarea').dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
        el.closest('[data-tile-surface]').querySelector('[data-block="paragraph-25"] [data-marks] button').click();
      });
      await settle(page);
      await dismiss(() => sheet.getByRole('button', { name: 'Close', exact: true }).click());
      assert(await tile.locator('[data-block="paragraph-25"] [data-marks] button').evaluate(el => document.activeElement === el),
        'rapid reopen on a different block returns focus to its own marker');
    }
    f.phase = 'thread';
    await marker(26); await capture('thread');
    if (!before) {
      const b = await tile.locator('[data-block="paragraph-26"]').boundingBox();
      const s = await sheet.boundingBox();
      assert(b.y >= (await tile.boundingBox()).y && b.y + b.height <= s.y + 1,
        'long thread leaves its block visible even in a short tile');
    }
    // Long content scrolls inside the short sheet; reply stays reachable.
    const reply = sheet.locator('textarea');
    await reply.fill('Reply draft'); await reply.scrollIntoViewIfNeeded();
    await dismiss(() => page.keyboard.press('Escape'));
    if (!before) {
      await marker(26); await sheet.waitFor();
      assert.equal(await sheet.locator('textarea').inputValue(), 'Reply draft', 'thread reply survives dismissal');
      const outgoing = await sheet.elementHandle();
      await sheet.getByRole('button', { name: 'Thread options', exact: true }).click();
      await sheet.getByRole('button', { name: /All comments/ }).click();
      await page.waitForFunction(el => !el.isConnected, outgoing);
      await outgoing.dispose();
      await settle(page);
      assert.equal(await sheet.count(), 1, 'All comments hands off to one panel sheet');
      const threadRow = sheet.locator('[data-thread-toggle="thread-1"]');
      assert.equal(await threadRow.getAttribute('aria-expanded'), 'false', 'panel starts with the thread collapsed');
      await threadRow.click();
      assert.equal(await sheet.locator('textarea').inputValue(), 'Reply draft', 'reply draft survives All comments handoff');
      assert(await tile.locator('[style*="padding-bottom"]').first().evaluate(el => parseFloat(el.style.paddingBottom) > 0),
        'exiting block sheet does not erase incoming panel scroll space');
      await dismiss(() => sheet.getByRole('button', { name: 'Close the comments' }).click());
    }
    f.phase = 'comments panel';
    const panelOpener = tile.locator('button[data-opens="comments"]').first();
    await panelOpener.click(); await capture('panel');
    if (!before) {
      if (count > 1) {
        const otherDoc = page.locator(`[data-tile-motion="${2 + (count - 1) * 2}"]`);
        const block = otherDoc.locator('[data-block="paragraph-25"]');
        await block.scrollIntoViewIfNeeded(); await block.hover();
        await block.getByRole('button', { name: 'Comment on this block' }).click();
        const otherSheet = otherDoc.locator('.phone-sheet[data-open="true"]');
        await otherSheet.waitFor();
        assert.equal(await sheet.count(), 2, 'two different documents can have tile-local sheets open');
        await page.screenshot({ path: path.join(evidence, `${phase}-${label}-two-docs.png`) });
        await otherSheet.getByRole('button', { name: 'Close', exact: true }).focus();
        await dismiss(() => page.keyboard.press('Escape'), otherSheet);
        assert.equal(await sheet.count(), 1, 'Escape closes only its own document sheet');
      }
      await page.setViewportSize({ width: width + (count > 1 ? 120 : 0), height: height + 80 });
      await settle(page); contained(await geometry(sheet, tileSelector), 'resized tile');
      if (count > 1) {
        // Move the retained tile's layout box without resizing: no resize
        // observer can substitute for the correct positioning parent.
        const outer = page.locator(`[data-tile-motion="${tileKey}"]`);
        const left = await outer.evaluate(el => {
          const previous = el.style.getPropertyValue('--l'); el.style.setProperty('--l', '40%'); return previous;
        });
        await settle(page); contained(await geometry(sheet, tileSelector), 'moved tile');
        await outer.evaluate((el, left) => el.style.setProperty('--l', left), left);
      }
      await dismiss(() => sheet.getByRole('button', { name: 'Close the comments' }).click());
      assert(await panelOpener.evaluate(el => document.activeElement === el), 'Close returns focus to comments button');
      if (count === 1) {
        await panelOpener.click(); await sheet.waitFor();
        const writing = await sheet.locator('textarea').elementHandle();
        await writing.focus();
        // Visual-viewport-only keyboard shrink/pan, without a window resize.
        await page.evaluate(() => {
          const v = visualViewport;
          Object.defineProperty(v, 'height', { configurable: true, value: 360 });
          Object.defineProperty(v, 'offsetTop', { configurable: true, value: 24 });
          v.dispatchEvent(new Event('resize')); v.dispatchEvent(new Event('scroll'));
        });
        await settle(page);
        const keyboard = await geometry(sheet, tileSelector);
        assert(keyboard.sheet.bottom <= 385 && keyboard.sheet.top >= 24, 'phone controls stay above visual-viewport keyboard');
        assert(keyboard.sheet.height <= 360 * .85 + 1, 'phone maximum follows visible keyboard height');
        assert(await writing.evaluate(el => el.isConnected && document.activeElement === el && el.value === 'Reply draft'),
          'keyboard changes retain the focused reply and its draft');
        await page.evaluate(() => {
          const v = visualViewport;
          delete v.height; delete v.offsetTop;
          v.dispatchEvent(new Event('resize')); v.dispatchEvent(new Event('scroll'));
        });
        await settle(page); contained(await geometry(sheet, tileSelector), 'keyboard dismissed');
        await dismiss(() => page.keyboard.press('Escape'));
      } else {
        // Exercise the real maximize/restore handlers while a draft is open.
        // Keyboard activation can reach the bar above the tile-local scrim.
        await marker(27); await sheet.waitFor();
        const maximize = tile.getByRole('button', { name: 'Focus', exact: true });
        await maximize.focus(); await page.keyboard.press('Enter'); await settle(page);
        assert.equal(await sheet.count(), 0, 'maximized desktop comments leave sheet mode');
        const restore = tile.getByRole('button', { name: 'Defocus', exact: true });
        await restore.focus(); await page.keyboard.press('Enter'); await sheet.waitFor(); await settle(page);
        contained(await geometry(sheet, tileSelector), 'restored tile');
        assert.equal(await sheet.getByPlaceholder('Your comment').inputValue(), 'A draft that survives closing the sheet');
        // The tile can also be closed through its retained keyboard control.
        const closeTile = tile.getByRole('button', { name: 'Close this tile', exact: true });
        await closeTile.focus(); await page.keyboard.press('Enter');
        await tile.waitFor({ state: 'detached' });
        await page.waitForFunction(() => !document.querySelector('.phone-sheet, .c6-comment-scrim'));
      }
    }
    assert.deepEqual(f.errors, [], 'no browser crashes or external requests');
    fs.writeFileSync(path.join(evidence, `${phase}-${label}.json`), JSON.stringify(records, null, 2) + '\n');
    console.log(`PASS ${phase}-${label}`);
  } catch (e) {
    await f?.page?.screenshot({ path: path.join(evidence, `${phase}-${label}-failure.png`) }).catch(() => {});
    console.error(`FAIL ${label} during ${f?.phase}`, f?.errors, f?.consoleMessages);
    throw e;
  } finally { await f?.context?.close(); }
}
async function main() {
  fs.mkdirSync(evidence, { recursive: true });
  const { server, origin } = await serve();
  try {
    for (const engine of (process.env.ENGINE === 'both' ? ['chromium', 'webkit'] : [process.env.ENGINE || 'chromium'])) {
      for (const name of (process.env.VIEWPORT || Object.keys(cases).join(',')).split(',')) {
        for (const theme of (process.env.THEME || 'light,dark').split(',')) {
          const browser = await launchBrowser(playwright, engine);
          try { await within(`${engine}-${name}-${theme}`, () => check(browser, origin, engine, name, theme)); }
          finally { await browser.close(); }
        }
      }
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
}
main().catch(e => { console.error(e); process.exitCode = 1; });
