#!/usr/bin/env node
'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');
const shots = process.env.SCREENSHOTS || '/tmp/n3-ui/item8';
const before = process.env.BEFORE === '1';
const motion = { choice: 'off', effective: 'off', os: 'no-preference' };
const DOC = 'owner-header-doc';
const observerDelivery = 'ResizeObserver loop completed with undelivered notifications.';
const block = (id, text, kind = 'paragraph', children = []) => ({ id, text, kind, attrs: {}, hash: `hash-${id}`, by: 'you', rev: 1, children });
const layout = { tiles: [{ key: 2, columns: [{ key: 3, view: { view: 'docs', id: DOC } }], cursor: 0, active: 3, used: 3, back: [], forward: [], stack: 0, minimized: false }], focus: 0, next: 4, full: false };
const stored = page => page.evaluate(() => JSON.parse(localStorage.getItem('c6.home.workspace.v2')));

// Phones intentionally have no document toolbar or dot-only owner link. The
// same ownership destination and live mark now live in workspace Actions.
async function phoneOwner(f, label, creator) {
  const { page } = f;
  const step = phase => { f.phase = `phone owner: ${phase}`; console.log(`  ${f.phase}`); };
  step('header');
  const header = page.locator('[data-tile-bar]:visible');
  const reading = page.locator('[data-doc-reading]:visible');
  const root = await reading.elementHandle();
  const hasOwner = !['you', 'token'].includes(creator);
  const openMenu = async () => {
    step('open Actions');
    await header.getByRole('button', { name: 'Actions', exact: true }).click();
    await page.locator('.phone-popover:visible').getByRole('menuitem', { name: 'Export…', exact: true }).waitFor();
  };
  const owner = () => page.locator('.phone-popover:visible').getByRole('menuitem', { name: 'Open the session that made it', exact: true });
  assert.equal(await page.locator('[data-doc-header]:visible').count(), 0, 'phone document toolbar is replaced, not stacked');
  const geometry = await header.boundingBox();
  const title = await header.locator('[data-phone-title]').boundingBox();
  assert(geometry.height <= 44 && title.width > 40, 'single-line phone title retains usable room');
  assert(geometry.x >= 0 && geometry.x + geometry.width <= page.viewportSize().width + 1);
  await page.screenshot({ path: path.join(shots, `${label}-header.png`) });
  await openMenu();
  if (!hasOwner) {
    assert.equal(await owner().count(), 0, 'a later session editor is not the creator');
    await page.screenshot({ path: path.join(shots, `${label}-menu.png`) });
    await page.keyboard.press('Escape');
    return;
  }
  const entry = owner();
  step('owner identity and geometry');
  await entry.waitFor();
  const glyph = entry.locator('[style]').last();
  const mark = await glyph.boundingBox();
  const menu = await entry.boundingBox();
  assert(mark.width > 0 && mark.width <= 10 && mark.height > 0 && mark.height <= 10, 'phone owner entry retains its dot glyph');
  assert(menu.height >= 44, 'owning-session destination keeps a full touch target');
  assert(menu.x >= geometry.x - 1 && menu.x + menu.width <= geometry.x + geometry.width + 1, 'owner action fits its tile');
  if (creator === 'parent') {
    step('live owner mark');
    await f.setState('idle', { title: 'Renamed owning session', colour: 'rose/diagonal' });
    await page.waitForFunction(el => el.getAttribute('style')?.includes('repeating-linear-gradient'), await glyph.elementHandle());
    assert(await root.evaluate(el => el.isConnected), 'live owner update retains the document body');
  } else if (creator === 'earlier-owner') {
    step('owner mark outside the live feed');
    // The fixture's detail endpoint supplies emerald/diagonal for this owner;
    // its live-feed entry is deliberately absent. Preserve desktop parity:
    // use fetched ownership metadata, not a generic hashed-colour substitute.
    await page.waitForFunction(el => {
      const style = el.getAttribute('style') || '';
      return style.includes('repeating-linear-gradient') && style.includes('#10b981');
    }, await glyph.elementHandle());
    assert(await root.evaluate(el => el.isConnected), 'owner detail fallback retains the document body');
  }
  await page.screenshot({ path: path.join(shots, `${label}-menu.png`) });
  await page.keyboard.press('Escape');
  step('scroll');
  const scroll = await reading.evaluate(el => {
    const ancestors = [];
    for (let p = el.parentElement; p; p = p.parentElement) {
      ancestors.push(p);
    }
    // A deck's sorter is a real nested scroller (max-height: 70vh). With
    // one phone bar the complete deck editor can fit, while its slides still
    // overflow that sorter. Match the desktop check's nested-scroll coverage.
    const candidates = [...ancestors, ...el.querySelectorAll('[data-deck-view] *')]
      .filter(p => /(auto|scroll)/.test(getComputedStyle(p).overflowY));
    const describe = p => ({ tag: p.tagName, classes: p.className,
      scope: ancestors.includes(p) ? 'ancestor' : 'deck-descendant',
      clientHeight: p.clientHeight, scrollHeight: p.scrollHeight,
      slides: p.querySelectorAll('[data-slide]').length });
    const containers = candidates.map(describe);
    const target = candidates.find(p => p.scrollHeight > p.clientHeight);
    if (!target) return { scrolled: 0, containers, target: null };
    target.scrollTop = 300;
    return { scrolled: target.scrollTop, containers, target: describe(target) };
  });
  fs.writeFileSync(path.join(shots, `${label}-scroll.json`), JSON.stringify(scroll, null, 2) + '\n');
  const scrolled = scroll.scrolled;
  assert(scrolled > 0, 'real document/deck content scrolls');
  await settle(page);
  assert(Math.abs((await header.boundingBox()).y - geometry.y) < 1, 'owner Actions stay reachable while reading scrolls');
  await page.screenshot({ path: path.join(shots, `${label}-scrolled.png`) });
  await openMenu();
  step('open owner');
  await owner().click();
  fs.writeFileSync(path.join(shots, `${label}-owner-click.json`), JSON.stringify({ layout: await stored(page),
    browserErrors: f.errors }, null, 2) + '\n');
  step('wait for owner destination');
  await page.waitForFunction(id => JSON.parse(localStorage.getItem('c6.home.workspace.v2')).tiles.some(t =>
    t.columns.some(c => c.view.view === 'session' && c.view.id === id)), creator);
  assert(await root.evaluate(el => el.isConnected), 'opening the owner retains the document');
  await settle(page);
  fs.writeFileSync(path.join(shots, `${label}-owner-open.json`), JSON.stringify({ layout: await stored(page),
    buttons: await header.getByRole('button').evaluateAll(els => els.map(el => ({ label: el.getAttribute('aria-label'), text: el.textContent }))),
    popovers: await page.locator('.phone-popover:visible').allTextContents(),
    browserErrors: f.errors }, null, 2) + '\n');
  await page.screenshot({ path: path.join(shots, `${label}-owner-open.png`) });
  assert.equal(await page.locator('.phone-popover:visible').count(), 0, 'owner navigation dismisses its Actions popover');
  step('back to retained document');
  await header.getByRole('button', { name: 'Back to the previous pane', exact: true }).click();
  step('wait for retained document');
  fs.writeFileSync(path.join(shots, `${label}-back-click.json`), JSON.stringify({ layout: await stored(page),
    browserErrors: f.errors }, null, 2) + '\n');
  await reading.waitFor();
  await openMenu();
  step('repeat owner navigation');
  await owner().click();
  await settle(page);
  assert.equal((await stored(page)).tiles.flatMap(t => t.columns).filter(c => c.view.view === 'session' && c.view.id === creator).length, 1,
    'repeated owner navigation shares the same correct session destination');
  fs.writeFileSync(path.join(shots, `${label}.json`), JSON.stringify({ header: geometry, title, mark, menu, scrolled,
    browserErrors: f.errors }, null, 2) + '\n');
}

async function check(browser, origin, theme, width, phone, kind, creator = 'parent') {
  const label = `${before ? 'before' : 'after'}-${theme}-${kind}-${phone ? 'phone' : 'tile'}-${width}-${creator}`;
  console.log(label);
  const knownDeckDelivery = kind === 'deck' && phone && width === 390;
  let startupDeliveries = 0;
  const d = { id: DOC, realm: 'home', kind, title: 'Release review — a deliberately long title for narrow document and deck headers', labels: [], rev: 1,
    created_by: creator === 'you' ? 'you' : creator === 'token' ? 'token:fixture' : `session:${creator}`,
    updated_by: 'session:last-editor-not-the-creator', created_at: 1, updated_at: 1, pending: 0,
    tabs: [{ id: 'reading', title: 'Reading', blocks: Array.from({ length: 20 }, (_, i) => kind === 'deck'
      ? { ...block(`slide-${i}`, '', 'slide', [block(`title-${i}`, `Review slide ${i + 1}`, 'heading'), block(`body-${i}`, 'The owning-session mark stays beside the title while you read and scroll.')]), attrs: { layout: 'body' } }
      : block(`paragraph-${i}`, `Section ${i + 1}. ${'The owning-session mark stays beside the title while you read and scroll. '.repeat(6)}`)) }] };
  let progress;
  const f = await fixture(browser, origin, phone ? { width, height: 844 } : { width: 1600, height: 1000 }, theme, motion, p => { progress = p; }, {}, {
    path: `/docs/${DOC}`, layout, ready: '[data-doc-reading]', instrumentation: false,
    setup: async ({ page, context }) => {
      if (knownDeckDelivery) {
        const crashHandler = page.listeners('pageerror')[0];
        page.off('pageerror', crashHandler);
        page.on('pageerror', error => {
          if (String(error.stack || error).includes(observerDelivery) && progress.phase === 'initial animation completion') {
            startupDeliveries++;
            progress.errors.push(String(error.stack || error));
            console.error('KNOWN PRISTINE DECK STARTUP DIAGNOSTIC:', String(error.stack || error));
          } else crashHandler(error);
        });
      }
      await context.route('**/api/**', route => {
        const p = new URL(route.request().url()).pathname;
        if (p.endsWith(`/docs/${DOC}`)) return route.fulfill({ json: d });
        if (p.endsWith(`/docs/${DOC}/threads`)) return route.fulfill({ json: [] });
        if (p.endsWith('/sessions/earlier-owner')) return route.fulfill({ json: {
          session: { id: 'earlier-owner', realm: 'home', worker_id: '00000000-0000-0000-0000-000000000000', title: 'Earlier owning session outside the live feed', colour: 'emerald/diagonal', cwd: '/tmp', connection_id: 'test', model: 'test', status: 'idle', created_at: 1, updated_at: 1, last_seq: 70 },
          worker_name: 'Motion fixture', worker_online: true, queued: [],
        } });
        return route.fallback();
      });
    },
  });
  const { page } = f;
  f.phase = 'ownership assertions';
  try {
    if (!phone) await page.addStyleTag({ content: `[data-tile-motion] { width: ${width + 10}px !important; }` });
    await settle(page);
    if (phone && !before) {
      await phoneOwner(f, label, creator);
      assert.deepEqual(f.errors.filter(e => !(knownDeckDelivery && e.includes(observerDelivery))), []);
      assert.equal(f.errors.length, startupDeliveries, 'no errors after startup');
      assert(startupDeliveries <= 1, 'no repeated observer errors');
      return;
    }
    await page.getByRole('button', { name: 'Document actions', exact: true }).waitFor();
    const header = before
      ? page.getByRole('button', { name: 'Document actions', exact: true }).locator('xpath=ancestor::div[contains(@class,"sticky")][1]')
      : page.locator('[data-doc-header]');
    if (before) {
      const baseline = { ownerMarks: await header.locator('[data-doc-owner]').count(),
        header: await header.boundingBox(), title: await header.locator('h1').boundingBox(), icons: await header.locator('svg').count() };
      await page.screenshot({ path: path.join(shots, `${label}-header.png`) });
      baseline.scrolled = await header.evaluate(el => {
        for (let p = el.parentElement; p; p = p.parentElement) {
          if (/(auto|scroll)/.test(getComputedStyle(p).overflowY) && p.scrollHeight > p.clientHeight) {
            p.scrollTop = 300; return p.scrollTop;
          }
        }
        for (const p of el.parentElement.querySelectorAll('main *')) {
          if (/(auto|scroll)/.test(getComputedStyle(p).overflowY) && p.scrollHeight > p.clientHeight) {
            p.scrollTop = 300; return p.scrollTop;
          }
        }
        return 0;
      });
      await settle(page);
      await page.screenshot({ path: path.join(shots, `${label}-scrolled.png`) });
      await header.getByRole('button', { name: 'Document actions', exact: true }).click();
      baseline.ownerEntries = await header.getByRole('menuitem', { name: 'Open the session that made it', exact: true }).count();
      await page.screenshot({ path: path.join(shots, `${label}-menu.png`) });
      baseline.browserErrors = f.errors;
      fs.writeFileSync(path.join(shots, `${label}.json`), JSON.stringify(baseline, null, 2) + '\n');
      assert.deepEqual(f.errors.filter(e => !(knownDeckDelivery && e.includes(observerDelivery))), []);
      assert.equal(f.errors.length, startupDeliveries);
      assert(startupDeliveries <= 1);
      return;
    }
    const root = await header.elementHandle();
    const owner = header.locator('[data-doc-owner]');
    const hasOwner = !['you', 'token'].includes(creator);
    if (!hasOwner) {
      assert.equal(await owner.count(), 0, 'a later session editor is not the creator');
      await header.getByRole('button', { name: 'Document actions', exact: true }).click();
      assert.equal(await page.getByRole('menuitem', { name: 'Open the session that made it', exact: true }).count(), 0);
      await page.screenshot({ path: path.join(shots, `${label}.png`) });
      assert.deepEqual(f.errors, []);
      return;
    }
    const name = creator === 'parent' ? 'Motion fixture parent' : 'Earlier owning session outside the live feed';
    await page.waitForFunction(({ name }) => [...document.querySelectorAll('[data-doc-owner]')].some(el => el.title.includes(name)), { name });
    assert.equal(await owner.locator('..').getAttribute('href'), `/r/home/sessions/${creator}`);
    assert((await owner.textContent()).includes(name), 'accessible name identifies the owning session');
    assert((await owner.getAttribute('title')).includes(name));
    const geometry = await header.evaluate(el => {
      const rect = el => { const r = el.getBoundingClientRect(); return { x: r.x, y: r.y, width: r.width, height: r.height, right: r.right, bottom: r.bottom }; };
      return { header: rect(el), heading: rect(el.querySelector('[data-doc-heading]')), title: rect(el.querySelector('h1')), owner: rect(el.querySelector('[data-doc-owner]')),
        iconCount: el.querySelectorAll('svg').length, titleScrollWidth: el.querySelector('h1').scrollWidth, titleClientWidth: el.querySelector('h1').clientWidth };
    });
    assert(geometry.owner.width > 0 && geometry.owner.width <= 44 && geometry.owner.height === 44, 'dot-only ownership target');
    assert(geometry.heading.height <= 44.5, 'title and owner stay on one line, including phones');
    assert(Math.abs((geometry.owner.y + geometry.owner.height / 2) - (geometry.title.y + geometry.title.height / 2)) < 1, 'owner sits beside the title');
    assert(geometry.title.width > 40 && geometry.owner.right <= geometry.header.right + 1);
    assert(geometry.iconCount >= (kind === 'doc' ? 5 : 4), 'existing document and action icons remain');
    assert(geometry.header.x >= 0 && geometry.header.right <= page.viewportSize().width + 1);
    await page.screenshot({ path: path.join(shots, `${label}-header.png`) });
    const scrolled = await header.evaluate(el => {
      for (let p = el.parentElement; p; p = p.parentElement) {
        if (/(auto|scroll)/.test(getComputedStyle(p).overflowY) && p.scrollHeight > p.clientHeight) {
          p.scrollTop = 300; return p.scrollTop;
        }
      }
      for (const p of el.parentElement.querySelectorAll('main *')) {
        if (/(auto|scroll)/.test(getComputedStyle(p).overflowY) && p.scrollHeight > p.clientHeight) {
          p.scrollTop = 300; return p.scrollTop;
        }
      }
      return 0;
    });
    assert(scrolled > 0, 'real document/deck content scrolls');
    await settle(page);
    const after = await header.boundingBox();
    assert(Math.abs(after.y - geometry.header.y) < 1, 'owning-session affordance is sticky, not scrolling metadata');
    await page.screenshot({ path: path.join(shots, `${label}-scrolled.png`) });
    if (creator === 'parent') {
      await f.setState('idle', { title: 'Renamed owning session', colour: 'rose/diagonal' });
      await page.waitForFunction(() => document.querySelector('[data-doc-owner]')?.title.includes('Renamed owning session'));
      assert(await root.evaluate(el => el.isConnected), 'session updates retain the document header');
      assert((await owner.locator('[style]').last().getAttribute('style')).includes('repeating-linear-gradient'), 'live session mark keeps its pattern');
    }
    await header.getByRole('button', { name: 'Document actions', exact: true }).click();
    const open = header.getByRole('menuitem', { name: 'Open the session that made it', exact: true });
    await open.waitFor();
    const menu = await open.boundingBox();
    await page.screenshot({ path: path.join(shots, `${label}-menu.png`) });
    assert(menu.x >= geometry.header.x - 1 && menu.x + menu.width <= geometry.header.right + 1, `owner menu entry stays inside its tile: ${JSON.stringify(menu)}`);
    await open.click();
    await page.waitForFunction(id => JSON.parse(localStorage.getItem('c6.home.workspace.v2')).tiles.some(t => t.columns.some(c => c.view.view === 'session' && c.view.id === id)), creator);
    assert(await root.evaluate(el => el.isConnected), 'opening the owner retains the document');
    if (!await header.isVisible()) await page.getByRole('button', { name: 'Back to the list', exact: true }).click();
    await owner.click();
    await settle(page);
    assert.equal((await stored(page)).tiles.flatMap(t => t.columns).filter(c => c.view.view === 'session' && c.view.id === creator).length, 1, 'dot and menu share the same owning destination');
    fs.writeFileSync(path.join(shots, `${label}.json`), JSON.stringify({ ...geometry, scrolled, menu, browserErrors: f.errors }, null, 2) + '\n');
    assert.deepEqual(f.errors.filter(e => !(knownDeckDelivery && e.includes(observerDelivery))), []);
    assert.equal(f.errors.length, startupDeliveries, 'no errors after startup');
    assert(startupDeliveries <= 1, 'no repeated observer errors');
  } catch (error) {
    await page.screenshot({ path: path.join(shots, `${label}-failure.png`) }).catch(() => {});
    fs.writeFileSync(path.join(shots, `${label}-failure.json`), JSON.stringify({ phase: f.phase, error: String(error.stack || error),
      layout: await stored(page).catch(() => null), popovers: await page.locator('.phone-popover:visible').allTextContents().catch(() => []),
      browserErrors: f.errors, console: f.consoleMessages }, null, 2) + '\n');
    throw error;
  } finally { await f.context.close(); }
}

async function main() {
  fs.mkdirSync(shots, { recursive: true });
  const { server, origin } = await serve();
  const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
  const run = async (theme, width, phone, kind, creator = 'parent') => {
    if (process.env.CASE && !`${theme}-${kind}-${phone ? 'phone' : 'tile'}-${width}-${creator}`.includes(process.env.CASE)) return;
    const browser = await launchBrowser(playwright, process.env.ENGINE || 'chromium');
    try { await check(browser, origin, theme, width, phone, kind, creator); }
    finally { await browser.close(); }
  };
  try {
    for (const theme of ['light', 'dark']) {
      for (const kind of ['doc', 'deck']) {
        for (const width of [1200, 320]) await run(theme, width, false, kind);
        for (const width of [320, 390]) await run(theme, width, true, kind);
      }
      await run(theme, 320, true, 'doc', 'earlier-owner');
      await run(theme, 1200, false, 'doc', 'you');
      await run(theme, 320, true, 'deck', 'token');
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
}
within('sticky document ownership', main).catch(error => { console.error(error); process.exitCode = 1; });
