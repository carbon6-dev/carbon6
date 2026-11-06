#!/usr/bin/env node
'use strict';

// Unit/contrast/source guards (no build): node --test crates/c6-ui/tests/theme.cjs
// Production browser: node crates/c6-ui/tests/theme.cjs --browser
// First-paint only, no WASM needed: add --paint-only. Both modes use ROOT=dist.
// Browser options match motion.cjs: ENGINE, PLAYWRIGHT_CORE, CHROMIUM, etc.
// WIDTHS=240,320,400,480,640 THEMES=light,dark,soft,system LAYOUTS=tile,phone
// THEME_DELAY_MS=300 delays the actual bootstrap fetch; WASM is held separately.
// SCREENSHOTS=/tmp/p3-screenshots records the Soft session and four-card picker.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const UI = path.resolve(__dirname, '..');
const source = fs.readFileSync(path.join(UI, 'js/theme.js'), 'utf8');
const pages = { light: '#fafafa', dark: '#141417', soft: '#1c1d21' };
const darkTokens = ['#141417', '#1c1c20', '#2a2a2f'];
const softTokens = ['#1c1d21', '#25262b', '#33343a', '#4a4b52'];
const accentTokens = ['zinc-400', 'red-400', 'white', 'sky-400', 'amber-400', 'emerald-400', 'blue-400'];
const rgb = hex => `rgb(${hex.slice(1).match(/../g).map(c => parseInt(c, 16)).join(', ')})`;

function bootstrap(choice, osDark = false, denied = false, withMeta = true) {
  const values = new Map(choice === null ? [] : [['c6.theme', choice]]);
  const classes = new Set(), root = { style: {}, classList: {
    toggle(name, on) { if (on) classes.add(name); else classes.delete(name); },
    contains: name => classes.has(name),
  } };
  const storage = {
    getItem(key) { if (denied) throw Error('denied'); return values.get(key) ?? null; },
    setItem(key, value) { if (denied) throw Error('denied'); values.set(key, value); },
  };
  const media = new EventTarget();
  media.matches = osDark;
  const window = new EventTarget();
  window.matchMedia = query => { assert.equal(query, '(prefers-color-scheme: dark)'); return media; };
  let colour, events = 0;
  window.addEventListener('c6-theme', () => events++);
  vm.runInNewContext(source, { window, localStorage: storage, Event, document: {
    documentElement: root,
    querySelector: selector => {
      assert.equal(selector, 'meta[name="theme-color"]');
      return withMeta ? { setAttribute(name, value) { assert.equal(name, 'content'); colour = value; } } : null;
    },
  } });
  return { theme: window.c6Theme, root, values,
    snapshot: () => ({ choice: window.c6Theme.get(), dark: window.c6Theme.isDark(),
      soft: classes.has('soft'), scheme: root.style.colorScheme, colour }),
    os(dark) { media.matches = dark; media.dispatchEvent(new Event('change')); },
    storage(choice, key = 'c6.theme') {
      if (key === null) values.clear();
      else if (choice === null) values.delete(key);
      else values.set(key, choice);
      const event = new Event('storage');
      Object.defineProperty(event, 'key', { value: key });
      window.dispatchEvent(event);
    },
    events: () => events,
  };
}

function expected(choice, osDark = false) {
  const dark = choice === 'soft' || choice === 'dark' || choice === 'system' && osDark;
  return { choice, dark, soft: choice === 'soft', scheme: dark ? 'dark' : 'light',
    colour: pages[choice === 'soft' ? 'soft' : dark ? 'dark' : 'light'] };
}

function luminance(hex) {
  const [r, g, b] = hex.slice(1).match(/../g).map(c => parseInt(c, 16) / 255)
    .map(c => c <= .04045 ? c / 12.92 : ((c + .055) / 1.055) ** 2.4);
  return .2126 * r + .7152 * g + .0722 * b;
}
function contrast(a, b) {
  const [hi, lo] = [luminance(a), luminance(b)].sort((a, b) => b - a);
  return (hi + .05) / (lo + .05);
}

async function builtContrast(page) {
  // Read the built Tailwind 4 tokens, not the document's older hex swatches.
  // A browser sRGB canvas resolves OKLCH/gamut mapping to displayed 8-bit RGB.
  const colours = await page.evaluate(names => {
    const css = getComputedStyle(document.documentElement);
    const canvas = document.createElement('canvas');
    canvas.width = canvas.height = 1;
    const ctx = canvas.getContext('2d', { colorSpace: 'srgb' });
    return Object.fromEntries(names.map(name => {
      const token = css.getPropertyValue(`--color-${name}`).trim();
      if (!token || !CSS.supports('color', token)) throw Error(`missing/invalid colour ${name}: ${token}`);
      ctx.clearRect(0, 0, 1, 1);
      ctx.fillStyle = token;
      ctx.fillRect(0, 0, 1, 1);
      const bytes = [...ctx.getImageData(0, 0, 1, 1).data];
      if (bytes[3] !== 255) throw Error(`invalid colour ${name}: ${token}`);
      return [name, { token, hex: '#' + bytes.slice(0, 3).map(c => c.toString(16).padStart(2, '0')).join('') }];
    }));
  }, accentTokens);
  for (const [name, { token, hex }] of Object.entries(colours)) {
    const ratios = softTokens.slice(0, 3).map(bg => contrast(hex, bg));
    console.log(`BUILT contrast ${name}: ${token} -> sRGB8 ${hex}, page/panel/chip ${ratios.map(r => r.toFixed(9)).join(' / ')}:1${ratios[2] < 4.5 ? ' [chip below AA 4.5]' : ''}`);
  }
  return colours;
}

function unitTests() {
  const { test } = require('node:test');
  test('synchronous bootstrap resolves every choice on either OS before WASM', () => {
    for (const os of [false, true]) for (const choice of ['system', 'light', 'dark', 'soft']) {
      const f = bootstrap(choice, os);
      assert.deepEqual(f.snapshot(), expected(choice, os));
      assert.equal(f.events(), 1);
    }
  });
  test('missing/unknown storage follows OS; legacy Dim remains unchanged Dark', () => {
    for (const os of [false, true]) {
      for (const choice of [null, '', 'unknown']) assert.deepEqual(bootstrap(choice, os).snapshot(), expected('system', os));
      assert.deepEqual(bootstrap('dim', os).snapshot(), expected('dark', os));
    }
  });
  test('switching removes stale Soft/dark classes and persists only in this browser', () => {
    const f = bootstrap('soft');
    for (const choice of ['dark', 'light', 'soft', 'system']) {
      f.theme.set(choice);
      assert.deepEqual(f.snapshot(), expected(choice));
      assert.equal(f.values.get('c6.theme'), choice);
      assert.deepEqual(bootstrap(f.values.get('c6.theme')).snapshot(), expected(choice));
    }
  });
  test('only System follows OS changes; Soft does not become ordinary Dark', () => {
    for (const choice of ['system', 'light', 'dark', 'soft']) {
      const f = bootstrap(choice);
      for (const os of [true, false]) {
        f.os(os);
        assert.deepEqual(f.snapshot(), expected(choice, os));
      }
    }
  });
  test('cross-tab storage updates, removal and clear apply; unrelated keys do not', () => {
    const f = bootstrap('light', true);
    f.storage('soft');
    assert.deepEqual(f.snapshot(), expected('soft', true));
    const events = f.events();
    f.storage('dark', 'c6.other');
    assert.equal(f.events(), events);
    f.storage(null);
    assert.deepEqual(f.snapshot(), expected('system', true));
    f.storage('soft');
    f.storage(null, null);
    assert.deepEqual(f.snapshot(), expected('system', true));
  });
  test('unavailable localStorage and missing theme-colour meta do not crash', () => {
    const f = bootstrap(null, true, true);
    f.theme.set('soft');
    assert.deepEqual(f.snapshot(), expected('system', true));
    assert.doesNotThrow(() => bootstrap('soft', false, false, false));
  });
  test('parser-blocking bootstrap precedes stylesheet; other helpers stay deferred', () => {
    const html = fs.readFileSync(path.join(UI, 'index.html'), 'utf8');
    const tags = html.match(/<script\b[^>]*src="\/js\/theme\.js"[^>]*>/g);
    assert.equal(tags.length, 1);
    assert(!/\b(defer|async|type)\b/.test(tags[0]));
    assert(html.indexOf(tags[0]) < html.indexOf('data-trunk rel="tailwind-css"'));
    assert(html.indexOf('meta name="theme-color"') < html.indexOf(tags[0]));
    for (const helper of ['crash', 'motion', 'terminal', 'passkey', 'search', 'files', 'push']) {
      assert(html.includes(`<script defer src="/js/${helper}.js"></script>`));
    }
  });
  test('Dark is unchanged; Soft only repoints grey tokens, including border steps', () => {
    const css = fs.readFileSync(path.join(UI, 'style/input.css'), 'utf8');
    const tokens = selector => [...css.match(new RegExp(`html\\.${selector} \\{([^}]+)\\}`))[1]
      .matchAll(/--color-zinc-(\d+): (#[\da-f]+);/g)].map(m => [m[1], m[2]]);
    assert.deepEqual(tokens('dark'), ['950', '900', '800'].map((shade, i) => [shade, darkTokens[i]]));
    assert.deepEqual(tokens('soft'), ['950', '900', '800', '700'].map((shade, i) => [shade, softTokens[i]]));
    const soft = css.match(/html\.soft \{([^}]+)\}/)[1];
    assert.equal(soft.match(/--/g).length, 4, 'no text/accent overrides');
  });
  test('Settings reuses the radio helper for all four choices in a named content grid', () => {
    const rust = fs.readFileSync(path.join(UI, 'src/settings.rs'), 'utf8');
    const appearance = rust.slice(rust.indexOf('fn AppearanceSetting()'), rust.indexOf('/// The user\'s identity colour'));
    assert.deepEqual([...appearance.matchAll(/\{option\("([^"]+)"/g)].map(m => m[1]), ['system', 'light', 'dark', 'soft']);
    assert(appearance.includes('@lg/settings:grid-cols-2 @4xl/settings:grid-cols-4'));
    assert(appearance.includes('Custom::new("c6-theme")'));
    assert(appearance.includes('on_cleanup(move || theme.remove())'));
  });
  test('exact sRGB contrast: muted passes; specified chip red rounds up but is below 4.5', t => {
    const colours = { muted: '#a1a1aa', red: '#f87171', white: '#ffffff',
      blue: '#38bdf8', amber: '#fbbf24', green: '#34d399' };
    for (const [name, colour] of Object.entries(colours)) {
      const ratios = softTokens.slice(0, 3).map(bg => contrast(colour, bg));
      t.diagnostic(`${name} ${colour}, page/panel/chip: ${ratios.map(r => r.toFixed(9)).join(' / ')}:1`);
      if (name !== 'red') assert(ratios.every(r => r >= 4.5));
    }
    assert(Math.abs(contrast(colours.muted, softTokens[2]) - 4.8396616782811295) < 1e-12);
    const red = contrast(colours.red, softTokens[2]);
    assert(red < 4.5 && red > 4.48);
    assert.equal(red.toFixed(1), '4.5');
    const builtRed = contrast('#ff6467', softTokens[2]);
    assert(Math.abs(builtRed - 4.294438) < 1e-6);
    t.diagnostic(`Tailwind 4 red-400 sRGB8 #ff6467 on chip: ${builtRed.toFixed(9)}:1, also below 4.5`);
  });
}

async function firstPaint(browser, origin, choice, os) {
  const context = await browser.newContext({ colorScheme: os, serviceWorkers: 'block' });
  let release, held = false;
  const gate = new Promise(resolve => { release = resolve; });
  await context.route('**/*.wasm', async route => { held = true; await gate; await route.abort().catch(() => {}); });
  await context.route('**/js/theme.js', async route => {
    await new Promise(resolve => setTimeout(resolve, Number(process.env.THEME_DELAY_MS || 300)));
    await route.continue();
  });
  await context.addInitScript(({ choice }) => {
    localStorage.setItem('c6.theme', choice);
    window.__themePaint = { frames: [], paints: [] };
    const canvas = document.createElement('canvas');
    canvas.width = canvas.height = 1;
    const ctx = canvas.getContext('2d', { colorSpace: 'srgb' });
    new PerformanceObserver(list => {
      for (const e of list.getEntries()) window.__themePaint.paints.push({ name: e.name, time: e.startTime });
    }).observe({ type: 'paint', buffered: true });
    const sample = time => {
      if (document.body) {
        const colour = getComputedStyle(document.body).backgroundColor;
        ctx.clearRect(0, 0, 1, 1);
        ctx.fillStyle = colour;
        ctx.fillRect(0, 0, 1, 1);
        const rgba = [...ctx.getImageData(0, 0, 1, 1).data];
        window.__themePaint.frames.push({ time, colour, alpha: rgba[3],
          background: `rgb(${rgba.slice(0, 3).join(', ')})`,
          dark: document.documentElement.classList.contains('dark'),
          soft: document.documentElement.classList.contains('soft') });
      }
      if (window.__themePaint.frames.length < 120) requestAnimationFrame(sample);
    };
    requestAnimationFrame(sample);
  }, { choice });
  try {
    const page = await context.newPage();
    await page.goto(origin, { waitUntil: 'commit' });
    await page.waitForFunction(() => window.c6Theme && window.__themePaint?.frames.length >= 3);
    const result = await page.evaluate(() => ({ ...window.__themePaint,
      choice: c6Theme.get(), scheme: document.documentElement.style.colorScheme,
      meta: document.querySelector('meta[name="theme-color"]').content,
      booted: !!window.wasmBindings, elements: document.body.children.length }));
    const e = expected(choice, os === 'dark');
    assert.equal(result.choice, choice);
    assert.equal(result.scheme, e.scheme);
    assert.equal(result.meta, e.colour);
    assert.equal(result.booted, false, 'checks must run before WASM starts');
    assert.equal(result.elements, 0, 'no app has rendered');
    for (const frame of result.frames) {
      assert.equal(frame.alpha, 255);
      assert.equal(frame.background, rgb(e.colour), `wrong first-rAF canvas: ${JSON.stringify(result)}`);
      assert.equal(frame.dark, e.dark);
      assert.equal(frame.soft, e.soft);
    }
    assert(held, 'WASM request must be held, not merely fast enough to win the race');
    console.log(`PASS first paint ${choice}, OS ${os}: ${result.frames.length} frames, delayed theme fetch, WASM held`);
    if (choice === 'soft' && os === 'dark') await builtContrast(page);
  } finally { release(); await context.close(); }
}

async function assertTheme(page, choice, os = false) {
  const actual = await page.evaluate(() => ({ choice: c6Theme.get(), dark: c6Theme.isDark(),
    soft: document.documentElement.classList.contains('soft'),
    scheme: document.documentElement.style.colorScheme,
    colour: document.querySelector('meta[name="theme-color"]').content }));
  assert.deepEqual(actual, expected(choice, os));
  if (choice === 'dark' || choice === 'soft') {
    const tokens = await page.evaluate(() => ['950', '900', '800', '700']
      .map(shade => getComputedStyle(document.documentElement).getPropertyValue(`--color-zinc-${shade}`).trim()));
    assert.deepEqual(tokens.slice(0, choice === 'soft' ? 4 : 3), choice === 'soft' ? softTokens : darkTokens);
  }
}

async function cardBounds(page) {
  const result = await page.locator('input[name=theme]').evaluateAll(radios => {
    const grid = radios[0].closest('label').parentElement;
    const bounds = grid.getBoundingClientRect();
    return radios.map(radio => {
      const label = radio.closest('label'), card = label.getBoundingClientRect();
      const r = radio.getBoundingClientRect(), text = radio.nextElementSibling.getBoundingClientRect();
      return { name: label.innerText, width: bounds.width,
        fits: card.left >= bounds.left - 1 && card.right <= bounds.right + 1 &&
          r.left >= card.left && r.right <= text.left + 1 && text.right <= card.right + 1,
        overflow: label.scrollWidth > label.clientWidth + 1 };
    });
  });
  assert.equal(result.length, 4);
  assert(result.every(card => card.fits && !card.overflow), `theme cards overflow/overlap: ${JSON.stringify(result)}`);
}

async function switching(browser, origin, layout, fixture, settle) {
  const motion = { choice: 'off', effective: 'off', os: 'no-preference' };
  const f = await fixture(browser, origin, layout === 'phone' ? { width: 640, height: 844 } :
    { width: 1600, height: 1000 }, 'dark', motion, () => {});
  try {
    const darkAccents = await f.page.evaluate(names => names
      .map(name => getComputedStyle(document.documentElement).getPropertyValue(`--color-${name}`).trim()), accentTokens);
    await f.page.evaluate(() => c6Theme.set('soft'));
    await assertTheme(f.page, 'soft');
    const softAccents = await f.page.evaluate(names => names
      .map(name => getComputedStyle(document.documentElement).getPropertyValue(`--color-${name}`).trim()), accentTokens);
    assert.deepEqual(softAccents, darkAccents, 'Soft must not repoint text/accent tokens');
    const composer = await f.page.locator('textarea').first().evaluate(el =>
      getComputedStyle(el.closest('.rounded-xl')).borderTopColor);
    assert.equal(composer, rgb('#4a4b52'), 'Soft composer keeps its stronger border');
    if (process.env.SCREENSHOTS) await f.page.screenshot({ path: path.join(process.env.SCREENSHOTS, `soft-theme-session-${layout}.png`) });
    await f.page.evaluate(() => localStorage.removeItem('c6.home.workspace.v2'));
    await f.page.goto(`${origin}/r/home/settings/appearance`, { waitUntil: 'domcontentloaded' });
    await f.page.locator('input[name=theme]').first().waitFor();
    const radios = f.page.locator('input[name=theme]');
    assert.equal(await radios.count(), 4);
    const themes = (process.env.THEMES || 'light,dark,soft,system').split(',');
    for (const width of (process.env.WIDTHS || '240,320,400,480,640').split(',').map(Number)) {
      if (layout === 'phone') await f.page.setViewportSize({ width, height: 844 });
      else await f.page.locator('[data-tile-motion]').first().evaluate((el, width) => { el.style.width = `${width + 8}px`; }, width);
      await settle(f.page);
      for (const choice of themes) {
        const index = ['system', 'light', 'dark', 'soft'].indexOf(choice);
        assert(index >= 0, `unknown THEMES choice ${choice}`);
        await radios.nth(index).check();
        await assertTheme(f.page, choice, true);
        assert.equal(await f.page.evaluate(() => localStorage.getItem('c6.theme')), choice);
        await cardBounds(f.page);
        if (process.env.SCREENSHOTS && choice === 'soft') {
          const target = layout === 'tile' ? f.page.locator('[data-tile-motion]').first() : f.page;
          await target.screenshot({ path: path.join(process.env.SCREENSHOTS, `soft-theme-${layout}-${width}.png`) });
        }
        console.log(`PASS ${layout} ${width}px ${choice}: four usable cards, theme and persistence`);
      }
    }
    // Both windows render the production Settings component. Real storage
    // events, not synthetic dispatches, must update palette AND selected card.
    const peer = await f.context.newPage();
    await peer.goto(`${origin}/r/home/settings/appearance`, { waitUntil: 'domcontentloaded' });
    await peer.locator('input[name=theme]').first().waitFor();
    for (const [choice, index] of [['soft', 3], ['dark', 2], ['light', 1], ['system', 0]]) {
      await radios.nth(index).check();
      await peer.waitForFunction(choice => c6Theme.get() === choice &&
        document.querySelectorAll('input[name=theme]')[['system', 'light', 'dark', 'soft'].indexOf(choice)].checked, choice);
      await assertTheme(peer, choice, true);
    }
    await f.page.emulateMedia({ colorScheme: 'light' });
    await f.page.waitForFunction(() => !c6Theme.isDark());
    await assertTheme(f.page, 'system');
    await radios.nth(3).check();
    await f.page.emulateMedia({ colorScheme: 'dark' });
    await assertTheme(f.page, 'soft', true);
    await f.page.emulateMedia({ colorScheme: 'light' });
    await assertTheme(f.page, 'soft');
    await f.page.reload({ waitUntil: 'domcontentloaded' });
    await f.page.locator('input[name=theme]').nth(3).waitFor();
    assert(await f.page.locator('input[name=theme]').nth(3).isChecked(), 'Soft survives reload');
    await assertTheme(f.page, 'soft');
    assert.deepEqual(f.errors, [], 'no production UI errors');
    console.log(`PASS ${layout}: cross-tab selected card, OS changes, Soft reload`);
  } finally { await f.context.close(); }
}

async function browserTests() {
  if (process.env.SCREENSHOTS) fs.mkdirSync(process.env.SCREENSHOTS, { recursive: true });
  const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');
  const playwright = require(process.env.PLAYWRIGHT_CORE || '/home/zheilbron/.bun/install/cache/playwright-core@1.50.1@@@1');
  const { server, origin } = await serve();
  let browser;
  try {
    browser = await launchBrowser(playwright, process.env.ENGINE || 'chromium');
    await within('theme first-paint and switching', async () => {
      for (const choice of ['system', 'light', 'dark', 'soft']) for (const os of ['light', 'dark']) {
        await firstPaint(browser, origin, choice, os);
      }
      if (!process.argv.includes('--paint-only')) for (const layout of (process.env.LAYOUTS || 'tile,phone').split(',')) {
        assert(['tile', 'phone'].includes(layout), `unknown LAYOUTS choice ${layout}`);
        await switching(browser, origin, layout, fixture, settle);
      }
    });
  } finally { await browser?.close(); await new Promise(resolve => server.close(resolve)); }
}

if (process.argv.includes('--browser')) browserTests().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
else unitTests();
