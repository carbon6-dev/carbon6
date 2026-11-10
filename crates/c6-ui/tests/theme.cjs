#!/usr/bin/env node
'use strict';

// Unit/contrast/source guards (no build): node --test crates/c6-ui/tests/theme.cjs
// Production browser: node crates/c6-ui/tests/theme.cjs --browser
// Retained-tile radio regression only: add --workspace-radios.
// First-paint only, no WASM needed: add --paint-only. Both modes use ROOT=dist.
// Browser options match motion.cjs: ENGINE, PLAYWRIGHT_CORE, CHROMIUM, etc.
// WIDTHS=240,320,400,480,640 THEMES=light,dark,system LAYOUTS=tile,phone
// THEME_DELAY_MS=300 delays the actual bootstrap fetch; WASM is held separately.
// SCREENSHOTS=/tmp/q6-dark/after records the Dark session and three-card picker.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const vm = require('node:vm');
const UI = path.resolve(__dirname, '..');
const source = fs.readFileSync(path.join(UI, 'js/theme.js'), 'utf8');
const pages = { light: '#fafafa', dark: '#1c1d21' };
const darkTokens = ['#1c1d21', '#25262b', '#33343a', '#4a4b52'];
const accentTokens = ['zinc-400', 'red-400', 'white', 'sky-400', 'amber-400', 'emerald-400', 'blue-400'];
const redPairs = [['red-300', 'red-950'], ['red-200', 'red-950'], ['rose-300', 'zinc-800'], ['white', 'red-600']];
const rgb = hex => `rgb(${hex.slice(1).match(/../g).map(c => parseInt(c, 16)).join(', ')})`;

function bootstrap(choice, osDark = false, denied = false, withMeta = true, readOnly = false) {
  const values = new Map(choice === null ? [] : [['c6.theme', choice]]);
  const classes = new Set(), root = { style: {}, classList: {
    toggle(name, on) { if (on) classes.add(name); else classes.delete(name); },
    remove(name) { classes.delete(name); },
    add(name) { classes.add(name); },
    contains: name => classes.has(name),
  } };
  const storage = {
    getItem(key) { if (denied) throw Error('denied'); return values.get(key) ?? null; },
    setItem(key, value) { if (denied || readOnly) throw Error('denied'); values.set(key, value); },
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
  if (choice === 'soft' || choice === 'dim') choice = 'dark';
  const dark = choice === 'dark' || choice === 'system' && osDark;
  return { choice, dark, soft: false, scheme: dark ? 'dark' : 'light',
    colour: pages[dark ? 'dark' : 'light'] };
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
  }, [...new Set([...accentTokens, ...redPairs.flat(), 'rose-900'])]);
  for (const name of accentTokens) {
    const { token, hex } = colours[name];
    const ratios = darkTokens.slice(0, 3).map(bg => contrast(hex, bg));
    console.log(`BUILT contrast ${name}: ${token} -> sRGB8 ${hex}, page/panel/chip ${ratios.map(r => r.toFixed(9)).join(' / ')}:1`);
    assert(ratios.every(r => r >= 4.5), `${name} must meet AA on every Dark background`);
  }
  for (const [fg, bg] of redPairs) {
    const ratio = contrast(colours[fg].hex, colours[bg].hex);
    console.log(`BUILT pair ${fg} ${colours[fg].hex} on ${bg} ${colours[bg].hex}: ${ratio.toFixed(9)}:1`);
    assert(ratio >= 4.5, `${fg} on ${bg} chip/button must meet AA`);
  }
  const blend = (fg, bg, alpha) => '#' + fg.slice(1).match(/../g).map((c, i) =>
    Math.round(parseInt(c, 16) * alpha + parseInt(bg.slice(1 + 2 * i, 3 + 2 * i), 16) * (1 - alpha)).toString(16).padStart(2, '0')).join('');
  const roseRatios = darkTokens.slice(0, 3).map(bg => contrast(colours['rose-300'].hex, blend(colours['rose-900'].hex, bg, .4)));
  console.log(`BUILT pair rose-300 on rose-900/40 over page/panel/chip: ${roseRatios.map(r => r.toFixed(9)).join(' / ')}:1`);
  assert(roseRatios.every(r => r >= 4.5), 'rose status chips must meet AA');
  assert.equal(colours['red-400'].hex, '#ff7476', 'Dark-only red lift');
  // Test actual utilities too, not just token variables. The SVG probe uses
  // the production diagram's depth-two opacity, including its alpha blend.
  const text = await page.evaluate(() => {
    const probe = document.createElement('div');
    probe.innerHTML = '<span class="text-muted">muted</span><span class="text-zinc-500 dark:text-zinc-400">secondary</span><svg><text class="fill-zinc-500 dark:fill-zinc-400">edge</text><text class="fill-current dark:fill-zinc-200 opacity-80">node</text></svg>';
    document.body.append(probe);
    const canvas = document.createElement('canvas');
    canvas.width = canvas.height = 1;
    const ctx = canvas.getContext('2d', { colorSpace: 'srgb' });
    const result = [...probe.querySelectorAll('span, text')].map(el => {
      const css = getComputedStyle(el);
      return { name: el.textContent, colours: ['#1c1d21', '#25262b', '#33343a'].map(bg => {
        ctx.globalAlpha = 1; ctx.fillStyle = bg; ctx.fillRect(0, 0, 1, 1);
        ctx.globalAlpha = Number(css.opacity);
        ctx.fillStyle = el.localName === 'text' ? css.fill : css.color;
        ctx.fillRect(0, 0, 1, 1);
        return '#' + [...ctx.getImageData(0, 0, 1, 1).data].slice(0, 3).map(c => c.toString(16).padStart(2, '0')).join('');
      }) };
    });
    probe.remove();
    return result;
  });
  for (const row of text) {
    const ratios = row.colours.map((fg, i) => contrast(fg, darkTokens[i]));
    console.log(`BUILT text ${row.name}: ${row.colours.join(' / ')}, page/panel/chip ${ratios.map(r => r.toFixed(9)).join(' / ')}:1`);
    assert(ratios.every(r => r >= 4.5), `${row.name} utility/fill with opacity must meet AA`);
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
  test('missing/unknown storage follows OS; legacy Dim and Soft migrate to selected Dark', () => {
    for (const os of [false, true]) {
      for (const choice of [null, '', 'unknown']) assert.deepEqual(bootstrap(choice, os).snapshot(), expected('system', os));
      for (const legacy of ['dim', 'soft']) {
        const f = bootstrap(legacy, os);
        assert.deepEqual(f.snapshot(), expected('dark', os));
        assert.equal(f.values.get('c6.theme'), 'dark', `${legacy} is rewritten, not a fourth choice`);
      }
    }
  });
  test('switching removes stale Soft/dark classes and persists only in this browser', () => {
    const f = bootstrap('soft');
    f.root.classList.add('soft');
    for (const choice of ['dark', 'light', 'soft', 'system']) {
      f.theme.set(choice);
      assert.deepEqual(f.snapshot(), expected(choice));
      assert.equal(f.values.get('c6.theme'), expected(choice).choice);
      assert.deepEqual(bootstrap(f.values.get('c6.theme')).snapshot(), expected(choice));
    }
  });
  test('only System follows OS changes; migrated Soft stays Dark', () => {
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
    assert.equal(f.values.get('c6.theme'), 'dark');
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
    const readOnly = bootstrap('soft', false, false, true, true);
    assert.deepEqual(readOnly.snapshot(), expected('dark'));
    assert.equal(readOnly.values.get('c6.theme'), 'soft', 'failed write still presents selected Dark');
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
  test('Dark takes the Soft greys and accessible red; there is no Soft selector or Light override', () => {
    const css = fs.readFileSync(path.join(UI, 'style/input.css'), 'utf8');
    const tokens = selector => [...css.match(new RegExp(`html\\.${selector} \\{([^}]+)\\}`))[1]
      .matchAll(/--color-zinc-(\d+): (#[\da-f]+);/g)].map(m => [m[1], m[2]]);
    assert.deepEqual(tokens('dark'), ['950', '900', '800', '700'].map((shade, i) => [shade, darkTokens[i]]));
    assert(!css.includes('html.soft'));
    assert(!css.includes('html.light'));
    const overrides = [...css.matchAll(/--color-red-400:\s*([^;]+);/g)];
    assert.equal(overrides.length, 1, 'one red-400 override');
    assert.equal(overrides[0][1], '#ff7476');
    assert(css.match(/html\.dark \{([^}]+)\}/)[1].includes('--color-red-400: #ff7476;'));
  });
  test('Settings reuses the radio helper for three choices in a named content grid', () => {
    const rust = fs.readFileSync(path.join(UI, 'src/settings.rs'), 'utf8');
    const appearance = rust.slice(rust.indexOf('fn AppearanceSetting()'), rust.indexOf('/// The user\'s identity colour'));
    assert.deepEqual([...appearance.matchAll(/\{option\("([^"]+)"/g)].map(m => m[1]), ['system', 'light', 'dark']);
    assert(appearance.includes('@lg/settings:grid-cols-2 @4xl/settings:grid-cols-3'));
    assert(appearance.includes('Custom::new("c6-theme")'));
    assert(appearance.includes('on_cleanup(move || theme.remove())'));
    assert(appearance.includes('Custom::new("c6-motion-choice")'));
    assert(appearance.includes('on_cleanup(move || motion_choice.remove())'));
  });
  test('native Settings radio names are instance-local, including Docs and Claude Code', () => {
    const rust = fs.readFileSync(path.join(UI, 'src/settings.rs'), 'utf8');
    assert(!/type="radio" name="/.test(rust), 'static native radio names collide across retained Settings views');
    for (const group of ['theme', 'motion', 'docs-write', 'cc-native', 'cc-tools']) {
      assert(rust.includes(`radio_name("${group}")`), `${group} needs its own instance-local radio group`);
    }
  });
  test('exact sRGB contrast: every named Dark text accent meets unrounded AA on page, panel and chip', t => {
    const colours = { muted: '#9f9fa9', red: '#ff7476', white: '#ffffff',
      blue: '#51a2ff', sky: '#00bcff', amber: '#ffb900', green: '#00d492' };
    for (const [name, colour] of Object.entries(colours)) {
      const ratios = darkTokens.slice(0, 3).map(bg => contrast(colour, bg));
      t.diagnostic(`${name} ${colour}, page/panel/chip: ${ratios.map(r => r.toFixed(9)).join(' / ')}:1`);
      assert(ratios.every(r => r >= 4.5), `${name} must pass without rounding`);
    }
    const css = fs.readFileSync(path.join(UI, 'style/input.css'), 'utf8');
    assert(css.includes(`--color-red-400: ${colours.red};`), 'test the shipped Dark red');
  });
  test('audited secondary text and SVG labels have dark-only readable greys', () => {
    for (const file of ['chats.rs', 'aspects.rs']) {
      const rust = fs.readFileSync(path.join(UI, 'src', file), 'utf8');
      for (const match of rust.matchAll(/"([^"\n]*\btext-zinc-500\b[^"\n]*)"/g)) {
        assert(match[1].includes('dark:text-zinc-400'), `${file}: ${match[1]}`);
      }
    }
    const rust = fs.readFileSync(path.join(UI, 'src/explorer.rs'), 'utf8');
    assert(rust.includes('fill-zinc-500 text-[10px] dark:fill-zinc-400'));
    assert(rust.includes('fill-current dark:fill-zinc-200 {size}'));
  });
}

async function firstPaint(browser, origin, choice, os) {
  const context = await browser.newContext({ colorScheme: os, serviceWorkers: 'block' });
  let release, held = false;
  const gate = new Promise(resolve => { release = resolve; });
  await context.route('**/*.wasm', async route => { held = true; await gate; await route.abort().catch(() => {}); });
  await context.route('**/js/theme.js', async route => {
    // Adversarial network latency, not a completion sleep: the bootstrap must
    // remain parser-blocking while unavailable. Waiting for page rAF here
    // can deadlock before the blocked document's first rendering opportunity.
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
    assert.equal(result.choice, e.choice);
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
    if (choice === 'dark' && os === 'dark') await builtContrast(page);
    if (choice === 'light' && os === 'dark') {
      const colour = await page.evaluate(() => {
        const canvas = document.createElement('canvas');
        canvas.width = canvas.height = 1;
        const ctx = canvas.getContext('2d', { colorSpace: 'srgb' });
        ctx.fillStyle = getComputedStyle(document.documentElement).getPropertyValue('--color-red-400');
        ctx.fillRect(0, 0, 1, 1);
        return '#' + [...ctx.getImageData(0, 0, 1, 1).data].slice(0, 3).map(c => c.toString(16).padStart(2, '0')).join('');
      });
      assert.equal(colour, '#ff6467', 'Light keeps Tailwind red-400');
    }
  } finally { release(); await context.close(); }
}

async function assertTheme(page, choice, os = false) {
  const actual = await page.evaluate(() => ({ choice: c6Theme.get(), dark: c6Theme.isDark(),
    soft: document.documentElement.classList.contains('soft'),
    scheme: document.documentElement.style.colorScheme,
    colour: document.querySelector('meta[name="theme-color"]').content }));
  assert.deepEqual(actual, expected(choice, os));
  if (expected(choice, os).dark) {
    const tokens = await page.evaluate(() => ['950', '900', '800', '700']
      .map(shade => getComputedStyle(document.documentElement).getPropertyValue(`--color-zinc-${shade}`).trim()));
    assert.deepEqual(tokens, darkTokens);
  }
}

async function cardBounds(page) {
  const result = await page.locator('input[name|=theme]').evaluateAll(radios => {
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
  assert.equal(result.length, 3);
  assert(result.every(card => card.fits && !card.overflow), `theme cards overflow/overlap: ${JSON.stringify(result)}`);
}

async function switching(browser, origin, layout, fixture, settle) {
  const motion = { choice: 'off', effective: 'off', os: 'no-preference' };
  const f = await fixture(browser, origin, layout === 'phone' ? { width: 390, height: 844 } :
    { width: 1600, height: 1000 }, 'dark', motion, () => {});
  try {
    // Exercise the legacy setter as well as startup and real cross-tab writes.
    await f.page.evaluate(() => c6Theme.set('soft'));
    await assertTheme(f.page, 'dark');
    const composer = await f.page.locator('textarea').first().evaluate(el =>
      getComputedStyle(el.closest('.rounded-xl')).borderTopColor);
    assert.equal(composer, rgb('#4a4b52'), 'Dark composer keeps its stronger border');
    if (process.env.SCREENSHOTS) await f.page.screenshot({ path: path.join(process.env.SCREENSHOTS, `dark-theme-session-${layout}.png`) });
    await f.page.evaluate(() => localStorage.removeItem('c6.home.workspace.v2'));
    await f.page.goto(`${origin}/r/home/settings/appearance`, { waitUntil: 'domcontentloaded' });
    await f.page.locator('input[name|=theme]').first().waitFor();
    const radios = f.page.locator('input[name|=theme]');
    assert.equal(await radios.count(), 3);
    assert(await radios.nth(2).isChecked(), 'legacy Soft lands on the selected Dark card');
    const themes = (process.env.THEMES || 'light,dark,system').split(',');
    for (const width of (process.env.WIDTHS || '240,320,400,480,640').split(',').map(Number)) {
      if (layout === 'phone') await f.page.setViewportSize({ width, height: 844 });
      else await f.page.locator('[data-tile-motion]').first().evaluate((el, width) => { el.style.width = `${width + 8}px`; }, width);
      await settle(f.page);
      // Card geometry is independent of palette. Keep every width in Dark,
      // and all policy choices at 320px; standalone/all mode is exhaustive.
      const choices = process.env.BROWSER_MATRIX === 'sample' && !process.env.THEMES && !process.env.WIDTHS && width !== 320 ? ['dark'] : themes;
      for (const choice of choices) {
        const index = ['system', 'light', 'dark'].indexOf(choice);
        assert(index >= 0, `unknown THEMES choice ${choice}`);
        await radios.nth(index).check();
        await assertTheme(f.page, choice, true);
        assert.equal(await f.page.evaluate(() => localStorage.getItem('c6.theme')), choice);
        await cardBounds(f.page);
        if (process.env.SCREENSHOTS && choice === 'dark') {
          const target = layout === 'tile' ? f.page.locator('[data-tile-motion]').first() : f.page;
          await target.screenshot({ path: path.join(process.env.SCREENSHOTS, `dark-theme-${layout}-${width}.png`) });
        }
        console.log(`PASS ${layout} ${width}px ${choice}: three usable cards, theme and persistence`);
      }
    }
    // Both windows render the production Settings component. Real storage
    // events, not synthetic dispatches, must update palette AND selected card.
    const peer = await f.context.newPage();
    await peer.goto(`${origin}/r/home/settings/appearance`, { waitUntil: 'domcontentloaded' });
    await peer.locator('input[name|=theme]').first().waitFor();
    await peer.evaluate(() => localStorage.setItem('c6.theme', 'soft'));
    await f.page.waitForFunction(() => c6Theme.get() === 'dark' && document.querySelectorAll('input[name|=theme]')[2].checked);
    await peer.waitForFunction(() => c6Theme.get() === 'dark' && document.querySelectorAll('input[name|=theme]')[2].checked);
    assert.equal(await peer.evaluate(() => localStorage.getItem('c6.theme')), 'dark', 'legacy cross-tab write is migrated');
    for (const [choice, index] of [['dark', 2], ['light', 1], ['system', 0]]) {
      await radios.nth(index).check();
      await peer.waitForFunction(choice => c6Theme.get() === choice &&
        document.querySelectorAll('input[name|=theme]')[['system', 'light', 'dark'].indexOf(choice)].checked, choice);
      await assertTheme(peer, choice, true);
    }
    await f.page.emulateMedia({ colorScheme: 'light' });
    await f.page.waitForFunction(() => !c6Theme.isDark());
    await assertTheme(f.page, 'system');
    await radios.nth(2).check();
    await f.page.emulateMedia({ colorScheme: 'dark' });
    await assertTheme(f.page, 'dark', true);
    await f.page.emulateMedia({ colorScheme: 'light' });
    await assertTheme(f.page, 'dark');
    await f.page.reload({ waitUntil: 'domcontentloaded' });
    await f.page.locator('input[name|=theme]').nth(2).waitFor();
    assert(await f.page.locator('input[name|=theme]').nth(2).isChecked(), 'Dark survives reload');
    await assertTheme(f.page, 'dark');
    assert.deepEqual(f.errors, [], 'no production UI errors');
    console.log(`PASS ${layout}: migrated Soft, cross-tab selected card, OS changes, Dark reload`);
  } finally { await f.context.close(); }
}

// Use real retained tiles, not CSS-resized copies of a single settings page.
// Settings is single-copy: saved duplicates collapse, and section navigation
// remounts Appearance with fresh document-wide native radio group names.
async function workspaceRadios(browser, origin, fixture, settle) {
  const f = await fixture(browser, origin, { width: 1600, height: 1000 }, 'dark',
    { choice: 'system', effective: 'full', os: 'no-preference' }, () => {});
  const { page } = f;
  try {
    for (const [layout, count, repeated] of [['single', 1], ['grid', 5], ['stack', 6], ['grid-saved-duplicate', 5, true], ['stack-saved-duplicate', 6, true]]) {
      await page.evaluate(({ count, repeated }) => {
        const views = [{ view: 'settings', section: 'appearance' }, { view: 'session', id: 'parent' },
          { view: 'workers' }, { view: 'sessions' }, repeated ? { view: 'settings', section: 'appearance' } : { view: 'workers' }, { view: 'docs', id: '' }];
        const tiles = views.slice(0, count).map((view, i) => ({ key: 10 + i,
          columns: [{ key: 20 + i, view }], cursor: 0, active: 20 + i, used: 20 + i,
          back: [], forward: [], stack: i % 4, minimized: false }));
        localStorage.setItem('c6.home.workspace.v2', JSON.stringify({ tiles, focus: 0, next: 40, full: false }));
        localStorage.setItem('c6.theme', 'soft');
        localStorage.setItem('c6.motion', 'system');
      }, { count, repeated: !!repeated });
      await page.goto(`${origin}/r/home/settings/appearance`, { waitUntil: 'domcontentloaded' });
      await page.getByRole('heading', { name: 'Theme', exact: true }).first().waitFor();
      const appearance = page.locator('[data-tile-motion="10"]');
      const check = async (phase, motion = 'system') => {
        await settle(page);
        const state = await page.evaluate(() => ({ theme: c6Theme.get(), motion: c6Motion.get(),
          soft: document.documentElement.classList.contains('soft'), effectiveMotion: document.documentElement.dataset.motion,
          panels: [...document.querySelectorAll('[data-tile-motion]')].flatMap(tile => {
            const radios = [...tile.querySelectorAll('input[name|=theme], input[name|=motion]')];
            return radios.length ? [{ key: tile.dataset.tileMotion,
              checked: radios.filter(r => r.checked).map(r => r.nextElementSibling.firstElementChild.textContent),
              selectedCards: radios.filter(r => r.closest('label').matches(':has(:checked)')).length,
              styled: radios.filter(r => r.checked).every(r =>
                getComputedStyle(r.closest('label')).borderColor !== getComputedStyle(radios.find(r => !r.checked).closest('label')).borderColor) }] : [];
          }) }));
        assert.equal(state.theme, 'dark', `${layout}/${phase}: applied palette`);
        assert.equal(state.motion, motion, `${layout}/${phase}: applied motion choice`);
        assert.equal(state.soft, false, `${layout}/${phase}: no obsolete Soft class`);
        assert.equal(state.effectiveMotion, motion === 'system' ? 'full' : motion, `${layout}/${phase}: effective motion`);
        assert.equal(state.panels.length, 1, `${layout}/${phase}: one surviving Appearance view`);
        assert.equal(state.panels[0].key, '10', `${layout}/${phase}: original Settings tile survives`);
        for (const panel of state.panels) {
          assert.deepEqual(panel.checked, ['Dark', motion[0].toUpperCase() + motion.slice(1)], `${layout}/${phase}: actual checked properties ${JSON.stringify(state)}`);
          assert.equal(panel.selectedCards, 2, `${layout}/${phase}: selected card CSS`);
          assert(panel.styled, `${layout}/${phase}: selected border painted`);
        }
        console.log(`PASS radios ${layout}/${phase}: ${JSON.stringify(state)}`);
      };
      await check('initial');
      if (repeated) {
        assert.equal(await page.locator('[data-tile-motion="14"]').count(), 0, `${layout}: saved duplicate Settings tile collapsed`);
      }
      await appearance.getByRole('radio', { name: /^Light/ }).check();
      await appearance.getByRole('radio', { name: /^Dark/ }).check();
      await appearance.getByRole('radio', { name: /^Off/ }).check();
      await check('motion-off', 'off');
      const names = () => appearance.locator('input[name|=theme], input[name|=motion]').evaluateAll(radios =>
        [...new Set(radios.map(r => r.name))]);
      const beforeNames = await names();
      assert.equal(beforeNames.length, 2, `${layout}: theme and motion have distinct native groups`);
      const section = async (value, label) => {
        const selector = appearance.locator('main > select');
        if (await selector.isVisible()) await selector.selectOption(value);
        else await appearance.locator('main > nav').getByRole('button', { name: label, exact: true }).click();
      };
      await section('docs', 'Docs');
      await appearance.locator('input[name|=theme]').first().waitFor({ state: 'detached' });
      await section('appearance', 'Appearance');
      await appearance.getByRole('heading', { name: 'Theme', exact: true }).waitFor();
      await check('remounted', 'off');
      const afterNames = await names();
      assert.equal(afterNames.length, 2, `${layout}: remounted theme and motion have distinct native groups`);
      assert(afterNames.every(name => !beforeNames.includes(name)), `${layout}: each mounted instance has fresh native group names`);
      await appearance.getByRole('radio', { name: /^Full/ }).check();
      await check('motion-full', 'full');
      await appearance.getByRole('radio', { name: /^System/ }).last().check();
      await check('selected');
      if (repeated) {
        const peer = await f.context.newPage();
        try {
          await peer.goto(`${origin}/r/home/settings/appearance`, { waitUntil: 'domcontentloaded' });
          const peerAppearance = peer.locator('[data-tile-motion="10"]');
          await peerAppearance.getByRole('radio', { name: /^Full/ }).check();
          await page.waitForFunction(() => c6Motion.get() === 'full');
          await check('peer-motion-full', 'full');
          await peerAppearance.getByRole('radio', { name: /^System/ }).last().check();
          await page.waitForFunction(() => c6Motion.get() === 'system');
          await check('peer-motion-system');
        } finally { await peer.close(); }
      }
      if (count > 1) {
        const other = page.locator('[data-tile-motion="11"]');
        await other.getByRole('button', { name: 'Focus', exact: true }).click();
        await check('other-maximized');
        await other.getByRole('button', { name: 'Defocus', exact: true }).click();
        await check('restored');
      }
      await page.reload({ waitUntil: 'domcontentloaded' });
      await page.getByRole('heading', { name: 'Theme', exact: true }).first().waitFor();
      await check('reload');
    }
    assert.deepEqual(f.errors, [], 'no production UI errors');
  } finally { await f.context.close(); }
}

// Both custom Choices callers and the session aspect palette use their real
// production cards. Keep fixture data here, sharing motion's transport/head.
async function workspaceChoices(browser, origin, fixture, settle) {
  const question = { seq: 66, ts: 66, type: 'question', call_id: 'retained-choice', state: 'asked',
    title: 'Choose a workspace verification approach', fields: [{ name: 'approach', label: 'Question approach',
      type: 'choice', required: true, choices: [{ value: 'first', label: 'Question first answer' }, { value: 'second', label: 'Question second answer' }] }] };
  const f = await fixture(browser, origin, { width: 1600, height: 1000 }, 'dark',
    { choice: 'system', effective: 'full', os: 'no-preference' }, () => {}, {}, { events: [question] });
  const { page } = f;
  try {
    await require('./narrow-fixtures.cjs').install(f.context);
    let marks = {};
    await f.context.route('**/api/**', async route => {
      const p = new URL(route.request().url()).pathname;
      if (p.endsWith('/settings/session_facets')) return route.fulfill({ json: { shown: ['worker', 'model'], tile_bars: true } });
      if (!p.endsWith('/marks')) return route.fallback();
      if (route.request().method() === 'PUT') {
        const { key, mark } = route.request().postDataJSON();
        marks = { ...marks, [key]: mark };
        f.setMarks(marks);
      }
      return route.fulfill({ json: marks });
    });
    await page.evaluate(() => {
      const views = [{ view: 'session', id: 'parent' }, { view: 'decisions' }, { view: 'workers' },
        { view: 'docs', id: '' }, { view: 'settings', section: 'appearance' }, { view: 'sessions' }];
      const tiles = views.map((view, i) => ({ key: 10 + i, columns: [{ key: 20 + i, view }],
        cursor: 0, active: 20 + i, used: 20 + i, back: [], forward: [], stack: i % 4, minimized: false }));
      localStorage.setItem('c6.home.workspace.v2', JSON.stringify({ tiles, focus: 0, next: 40, full: false }));
      localStorage.setItem('c6.theme', 'soft');
    });
    await page.goto(`${origin}/r/home/sessions/parent`, { waitUntil: 'domcontentloaded' });
    const session = page.locator('[data-tile-motion="10"]');
    const questionGroup = session.getByRole('radiogroup', { includeHidden: true });
    const decisionGroup = page.locator('#decision-7103').getByRole('radiogroup', { includeHidden: true });
    const checked = group => group.getByRole('radio', { includeHidden: true }).evaluateAll(rows => rows.filter(r => r.getAttribute('aria-checked') === 'true').map(r => r.textContent));
    const check = async phase => {
      await settle(page);
      assert.deepEqual(await checked(questionGroup), ['Question second answer'], `${phase}: session QuestionCard Choices`);
      assert.deepEqual(await checked(decisionGroup), ['Review every layout'], `${phase}: decision-form Choices`);
      console.log(`PASS custom Choices/${phase}: both real cards retain their independent selection`);
    };
    await questionGroup.waitFor();
    await decisionGroup.waitFor();
    assert.deepEqual(await checked(questionGroup), ['Question first answer'], 'required question defaults to its first choice');
    assert.deepEqual(await checked(decisionGroup), [], 'decision choice starts empty');
    await questionGroup.getByRole('radio', { name: 'Question second answer', exact: true }).click();
    const decisionPick = decisionGroup.getByRole('radio', { name: 'Review every layout', exact: true });
    await decisionPick.click(); await decisionPick.click();
    assert.deepEqual(await checked(decisionGroup), [], 'optional Choices supports clearing the chosen row');
    await decisionPick.click();
    await check('selected');
    const other = page.locator('[data-tile-motion="12"]');
    await other.getByRole('button', { name: 'Focus', exact: true }).click();
    await check('other-maximized');
    await other.getByRole('button', { name: 'Defocus', exact: true }).click();
    await check('restored');
    // Aspect strips intentionally yield in narrow tiles; use the real maximize
    // control to expose the session's bar, rather than forcing CSS visibility.
    await session.getByRole('button', { name: 'Focus', exact: true }).click();
    const worker = session.getByRole('button', { name: 'Worker: Motion fixture', exact: true });
    const card = page.locator('.c6-pop').filter({ hasText: 'Its mark, wherever it shows' });
    await worker.click();
    const save = page.waitForResponse(r => new URL(r.url()).pathname.endsWith('/marks') && r.request().method() === 'PUT');
    await card.getByRole('button', { name: 'rose', exact: true }).click();
    await save;
    await card.waitFor({ state: 'detached' });
    const aspect = async phase => {
      await worker.click();
      await card.waitFor();
      assert.match(await card.getByRole('button', { name: 'rose', exact: true }).getAttribute('class'), /ring-2/, `${phase}: aspect palette selected mark`);
      assert.match(await card.innerText(), /No other session has it/);
      await page.keyboard.press('Escape');
      await card.waitFor({ state: 'detached' });
      console.log(`PASS session aspect card/${phase}: saved mark remains selected`);
    };
    await aspect('selected');
    await session.getByRole('button', { name: 'Defocus', exact: true }).click();
    await other.getByRole('button', { name: 'Focus', exact: true }).click();
    await check('aspect-other-maximized');
    await other.getByRole('button', { name: 'Defocus', exact: true }).click();
    await session.getByRole('button', { name: 'Focus', exact: true }).click();
    await aspect('restored');
    await page.reload({ waitUntil: 'domcontentloaded' });
    await questionGroup.waitFor();
    assert.deepEqual(await checked(questionGroup), ['Question first answer'], 'reload reconstructs required question defaults, not an unsaved answer');
    await aspect('reload');
    await session.getByRole('button', { name: 'Defocus', exact: true }).click();
    await decisionGroup.waitFor();
    assert.deepEqual(await checked(decisionGroup), [], 'reload reconstructs the unanswered decision; unsent choices are not persisted');
    await questionGroup.getByRole('radio', { name: 'Question second answer', exact: true }).click();
    await decisionPick.click();
    await check('reload-reselected');
    assert.deepEqual(f.errors, [], 'no production UI errors');
  } finally { await f.context.close(); }
}

async function browserTests() {
  if (process.env.SCREENSHOTS) fs.mkdirSync(process.env.SCREENSHOTS, { recursive: true });
  const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');
  const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
  const { server, origin } = await serve();
  let browser;
  try {
    browser = await launchBrowser(playwright, process.env.ENGINE || 'chromium');
    if (process.argv.includes('--workspace-choices')) {
      await within('retained workspace Choices and aspects', () => workspaceChoices(browser, origin, fixture, settle));
      return;
    }
    if (process.argv.includes('--workspace-radios')) {
      await within('retained workspace radios', () => workspaceRadios(browser, origin, fixture, settle));
      await within('retained workspace Choices and aspects', () => workspaceChoices(browser, origin, fixture, settle));
      return;
    }
    await within('theme first-paint and switching', async () => {
      for (const choice of ['system', 'light', 'dark', 'soft', 'dim']) for (const os of ['light', 'dark']) {
        await firstPaint(browser, origin, choice, os);
      }
      if (!process.argv.includes('--paint-only')) for (const layout of (process.env.LAYOUTS || 'tile,phone').split(',')) {
        assert(['tile', 'phone'].includes(layout), `unknown LAYOUTS choice ${layout}`);
        await switching(browser, origin, layout, fixture, settle);
      }
      if (!process.argv.includes('--paint-only')) await workspaceRadios(browser, origin, fixture, settle);
      if (!process.argv.includes('--paint-only')) await workspaceChoices(browser, origin, fixture, settle);
    });
  } finally { await browser?.close(); await new Promise(resolve => server.close(resolve)); }
}

if (process.argv.includes('--browser')) browserTests().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
else unitTests();
