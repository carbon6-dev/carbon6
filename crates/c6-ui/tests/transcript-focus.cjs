#!/usr/bin/env node
'use strict';

// Production WASM/CSS and the owned loopback head from motion.cjs. Do not
// replace the transcript or suppress its native keyboard focus indicator.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, launchBrowser } = require('./motion.cjs');
const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
const output = process.env.SCREENSHOTS || '/tmp/n3-ui/item5';
const views = {
  desktop: { width: 1440, height: 1000, tiles: 1 },
  tile: { width: 1440, height: 1000, tiles: 4 },
  phone: { width: 390, height: 844, tiles: 1 },
  'phone-small': { width: 320, height: 568, tiles: 1 },
};
const layout = count => ({ tiles: Array.from({ length: count }, (_, i) => ({ key: 2 + i * 2,
  columns: [{ key: 3 + i * 2, view: i ? { view: ['sessions', 'workers', 'inbox'][i - 1] } : { view: 'session', id: 'parent' } }],
  cursor: 0, active: 3 + i * 2, used: 3 + i * 2, back: [], forward: [], stack: i, minimized: false })),
focus: 0, next: 2 + count * 2, full: false });

async function check(browser, origin, engine, theme, view) {
  const { width, height, tiles } = views[view], phone = width < 768;
  const label = `${engine}-${theme}-${view}`, records = [];
  let f;
  try {
    f = await fixture(browser, origin, { width, height }, theme,
      { choice: 'reduced', os: 'reduce', effective: 'reduced' }, () => {},
      { deviceScaleFactor: phone ? 2 : 1 }, { instrumentation: false, layout: layout(tiles) });
    const { page } = f;
    const scroll = page.locator('[tabindex="0"].overflow-y-auto').filter({ has: page.locator('.c6-transcript-row') });
    assert.equal(await scroll.count(), 1, 'one real, keyboard-scrollable transcript');
    if (view === 'tile') assert((await scroll.boundingBox()).width < 400, 'a genuinely narrow desktop tile');
    const composer = page.locator('textarea').first();
    const pointer = locator => phone ? locator.tap() : locator.click();
    const snapshot = async (phase, keyboard = false) => {
      f.phase = phase;
      await settle(page);
      const state = await scroll.evaluate(el => {
        const style = getComputedStyle(el), active = document.activeElement;
        return { focused: active === el, visible: el.matches(':focus-visible'),
          outline: { style: style.outlineStyle, width: style.outlineWidth, color: style.outlineColor },
          shadow: style.boxShadow, active: { tag: active.tagName, label: active.getAttribute('aria-label') },
          top: el.scrollTop, width: el.clientWidth, height: el.clientHeight, scrollHeight: el.scrollHeight };
      });
      records.push({ phase, ...state });
      if (process.env.DEBUG_FIXTURE) console.log(`${label}/${phase}: ${JSON.stringify(state)}`);
      if (keyboard) {
        assert(state.focused && state.visible, `${phase}: keyboard focus remains visible`);
        assert((state.outline.style !== 'none' && parseFloat(state.outline.width) > 0) || state.shadow !== 'none',
          `${phase}: keyboard users keep a painted focus cue`);
      } else if (keyboard === false) {
        assert.equal(state.visible, false, `${phase}: pointer does not leave keyboard focus on the transcript`);
        assert.equal(state.outline.style, 'none', `${phase}: no transcript outline from pointer interaction`);
        assert.equal(state.shadow, 'none', `${phase}: no transcript focus ring from pointer interaction`);
      }
      await page.screenshot({ path: path.join(output, `${label}-${phase}.png`) });
    };
    const text = scroll.locator('.md p').last();
    await snapshot('initial');
    await pointer(text); await snapshot('pointer-text');
    await pointer(composer); await pointer(text); await snapshot('pointer-after-composer');

    // Traverse natively rather than using locator.press(), which would focus
    // the target for us and conceal an unreachable keyboard scroll container.
    await pointer(composer);
    const stops = await page.locator('button,a[href],input,textarea,select,[tabindex="0"]').count();
    let reached = false;
    for (let i = 0; i <= stops && !reached; i++) {
      await page.keyboard.press('Shift+Tab');
      reached = await scroll.evaluate(el => document.activeElement === el);
    }
    assert(reached, 'transcript is reachable by native keyboard traversal');
    await snapshot('keyboard-tab', true);
    await page.keyboard.press('Home');
    await page.waitForFunction(el => el.scrollTop === 0, await scroll.elementHandle());
    await page.keyboard.press('PageDown');
    await page.waitForFunction(el => el.scrollTop > 0, await scroll.elementHandle());
    await snapshot('keyboard-pagedown', true);
    const down = await scroll.evaluate(el => el.scrollTop);
    await page.keyboard.press('PageUp');
    await page.waitForFunction(({ el, down }) => el.scrollTop < down, { el: await scroll.elementHandle(), down });
    await snapshot('keyboard-pageup', true);
    await page.keyboard.press('End');
    for (let i = 1; i <= 12; i++) f.send({ type: 'background_task', task_id: `focus-${i}`, kind: 'shell',
      description: `Focus fixture task ${i}`, state: 'started', exit_code: 0, elapsed_ms: 0,
      notify: true, native_type: '', foreground: false });
    await page.waitForFunction(phone => [...document.querySelectorAll(phone ? '[data-phone-status]' : '[data-transcript-status]')]
      .some(el => el.textContent.includes('12 background tasks')), phone);
    // This is the state inherited by the ring harness's next-width captures:
    // phoneStatusRow ends with scroll.focus() and keyboard End, not a tap.
    await snapshot('keyboard-background-12', true);
    // Some engines keep :focus-visible when clicking an already-focused
    // keyboard target. Record that native mixed-input state, not a pointer-
    // originated outline. Moving focus away and back must clear the cue.
    await pointer(text); await snapshot('mixed-pointer-on-keyboard-focus', null);

    if (phone) {
      const opener = page.locator('[data-phone-status] button');
      const status = page.getByRole('dialog', { name: 'Session status', exact: true });
      for (const close of ['button', 'outside', 'escape']) {
        await opener.tap(); await status.waitFor();
        await snapshot(`pointer-status-${close}-open`);
        if (close === 'button') await status.getByRole('button', { name: 'Close', exact: true }).tap();
        else if (close === 'outside') await page.locator('.phone-dialog .c6-scrim').tap({ position: { x: 4, y: 4 } });
        else await page.keyboard.press('Escape');
        await status.waitFor({ state: 'detached' });
        await snapshot(`pointer-status-${close}-closed`);
      }
      await page.getByRole('button', { name: /Open session sheet/ }).tap();
      const details = page.getByRole('dialog', { name: 'Session details', exact: true });
      await details.waitFor(); await snapshot('pointer-sheet-open');
      await details.getByRole('button', { name: 'Close', exact: true }).tap();
      await details.waitFor({ state: 'detached' }); await snapshot('pointer-sheet-closed');
    }
    await pointer(composer); await pointer(text); await snapshot('pointer-after-keyboard');
    assert.deepEqual(f.errors, [], 'no browser crashes, invalid streams or external requests');
    console.log(`PASS ${label}: pointer has no transcript outline; keyboard traversal/scrolling retains its cue`);
  } finally {
    fs.writeFileSync(path.join(output, `${label}.json`), JSON.stringify(records, null, 2));
    await f?.context.close();
  }
}

(async () => {
  fs.mkdirSync(output, { recursive: true });
  const { server, origin } = await serve();
  try {
    for (const engine of (process.env.ENGINE || 'both').replace('both', 'chromium,webkit').split(',')) {
      for (const theme of (process.env.THEME || 'light,dark').split(',')) {
        for (const view of (process.env.VIEWPORT || Object.keys(views).join(',')).split(',')) {
          // Own the browser process as well as its context: cases share no
          // process state. Startup errors fail; no retry or ignored crash.
          const browser = await launchBrowser(playwright, engine);
          try { await check(browser, origin, engine, theme, view); }
          finally { await browser.close(); }
        }
      }
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
})().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
