#!/usr/bin/env node
'use strict';

// Production handlers and real DOM bubbling, using motion.cjs's loopback head.
// No UI build here: ROOT points at the coordinator's final production dist.
const assert = require('node:assert/strict');
const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');
const motion = { choice: 'off', effective: 'off', os: 'no-preference' };
const keys = [...'123456789', '[', ']', '\\', 'k', 'j'];
const layout = {
  tiles: [
    { key: 2, columns: [{ key: 3, view: { view: 'session', id: 'parent' } }],
      cursor: 0, active: 3, used: 3, back: [[{ key: 6, view: { view: 'sessions' } }]],
      forward: [], stack: 0, minimized: false },
    { key: 4, columns: [{ key: 5, view: { view: 'workers' } }],
      cursor: 0, active: 5, used: 5, back: [], forward: [], stack: 0, minimized: false },
  ], focus: 0, next: 7, full: false,
};

async function check(browser, origin, platform) {
  // Set the browser platform before production startup, not the head/worker OS.
  const configured = { newContext: async options => {
    const context = await browser.newContext(options);
    await context.addInitScript(platform => {
      Object.defineProperty(navigator, 'platform', { configurable: true, value: platform });
    }, platform);
    return context;
  } };
  let progress;
  try {
    const f = await fixture(configured, origin, { width: 1440, height: 1000 }, 'light', motion, f => { progress = f; });
    const { page } = f;
    await page.evaluate(layout => localStorage.setItem('c6.home.workspace.v2', JSON.stringify(layout)), layout);
    await page.reload({ waitUntil: 'domcontentloaded' });
    await page.locator('[data-tile-motion="2"] textarea').waitFor();
    await page.locator('[data-tile-motion="4"]').waitFor();
    await settle(page);
    assert.equal(await page.evaluate(() => navigator.platform), platform);
    // A plain input proves ordinary editable focus is still allowed. The
    // terminal DOM fixture deliberately has no xterm JS to swallow events:
    // these keystrokes must reach the real window handlers unprevented.
    await page.evaluate(() => {
      const input = document.createElement('textarea');
      input.id = 'shortcut-input';
      const terminal = document.createElement('div');
      terminal.className = 'xterm';
      const area = document.createElement('textarea');
      area.className = 'xterm-helper-textarea';
      terminal.append(area);
      document.body.append(input, terminal);
    });
    const command = platform.startsWith('Mac') ? { metaKey: true } : { ctrlKey: true };
    const opposite = platform.startsWith('Mac') ? { ctrlKey: true } : { metaKey: true };
    const snapshot = () => page.evaluate(() => ({
      layout: JSON.parse(localStorage.getItem('c6.home.workspace.v2')),
      path: location.pathname,
      sidebar: document.querySelector('.c6-sidebar')?.getAttribute('data-open'),
      ask: !!document.querySelector('[role="dialog"][aria-label="Ask carbon6"]'),
      focus: document.activeElement?.id || document.activeElement?.getAttribute('placeholder') || document.activeElement?.className,
    }));
    const focus = selector => page.locator(selector).focus();
    const dispatch = async (selector, key, modifiers, prevent = false) => {
      const result = await page.evaluate(({ selector, key, modifiers, prevent }) => {
        const target = selector === 'window' ? window : document.querySelector(selector);
        const ev = new KeyboardEvent('keydown', { key, bubbles: true, cancelable: true, ...modifiers });
        let reachedWindow = false;
        const seen = event => { if (event === ev) reachedWindow = true; };
        window.addEventListener('keydown', seen);
        if (prevent) target.addEventListener('keydown', ev => ev.preventDefault(), { once: true });
        target.dispatchEvent(ev);
        window.removeEventListener('keydown', seen);
        return { prevented: ev.defaultPrevented, reachedWindow };
      }, { selector, key, modifiers, prevent });
      assert(result.reachedWindow, `${platform} ${key} must actually reach window by bubbling`);
      await settle(page);
      return result;
    };
    const input = '#shortcut-input', terminal = '.xterm-helper-textarea';
    await focus(input);
    const unchanged = async (target, key, modifiers, prevent = false) => {
      const before = await snapshot();
      const result = await dispatch(target, key, modifiers, prevent);
      assert.equal(result.prevented, prevent, `${platform} ${target} ${key}: app must not prevent input`);
      assert.deepEqual(await snapshot(), before, `${platform} ${target} ${key}: no navigation, sidebar or focus change`);
    };
    for (const key of keys) {
      for (const modifiers of [opposite, {}, { metaKey: true, ctrlKey: true },
        { ...command, altKey: true }, { ...command, shiftKey: true }, { ...command, isComposing: true }]) {
        await unchanged(input, key, modifiers);
      }
      await unchanged(input, key, command, true);
    }
    const accepted = async key => {
      await focus(input);
      assert((await dispatch(input, key, command)).prevented, `${platform} ${key}: command should be handled`);
      return snapshot();
    };
    assert.equal((await accepted('2')).layout.focus, 1);
    assert.equal((await accepted('1')).layout.focus, 0);
    for (const key of '3456789') assert.equal((await accepted(key)).layout.focus, 0, 'absent tile stays put');
    assert.equal((await accepted('[')).layout.tiles[0].columns[0].view.view, 'sessions', '[ goes back in tile history');
    assert.equal((await accepted(']')).layout.tiles[0].columns[0].view.view, 'session', '] goes forward in tile history');
    const sidebar = (await snapshot()).sidebar;
    assert.notEqual((await accepted('\\')).sidebar, sidebar);
    assert.equal((await accepted('\\')).sidebar, sidebar);
    await accepted('k');
    assert(await page.locator('input[placeholder="Search sessions (⌘K)"]:focus').isVisible());
    await accepted('j');
    await page.getByRole('dialog', { name: 'Ask carbon6', exact: true }).waitFor();
    await page.getByRole('dialog', { name: 'Ask carbon6', exact: true }).getByRole('button', { name: 'Cancel', exact: true }).click();
    await page.getByRole('dialog', { name: 'Ask carbon6', exact: true }).waitFor({ state: 'detached' });
    await focus(terminal);
    for (const target of [terminal, 'window']) {
      for (const key of keys) {
        for (const modifiers of [command, opposite]) await unchanged(target, key, modifiers);
      }
    }
    // Explicit macOS regression: Ctrl+[ is ESC for the terminal, not tile Back.
    await unchanged(terminal, '[', { ctrlKey: true });
    await unchanged('window', '[', { ctrlKey: true });
    assert.equal((await accepted('2')).layout.focus, 1, 'shortcuts resume after leaving terminal');
    assert.equal((await accepted('1')).layout.focus, 0);
    assert.deepEqual(f.errors, []);
    console.log(`PASS keyboard shortcuts ${platform}`);
  } finally { await progress?.context.close(); }
}

async function main() {
  const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
  const { server, origin } = await serve();
  let browser;
  try {
    const engine = process.env.ENGINE || 'chromium';
    assert(['chromium', 'webkit', 'both'].includes(engine));
    for (const name of engine === 'both' ? ['chromium', 'webkit'] : [engine]) {
      browser = await launchBrowser(playwright, name);
      try {
        for (const platform of ['MacIntel', 'Linux x86_64', 'Win32']) await check(browser, origin, platform);
      } finally { await browser.close(); browser = null; }
    }
  } finally { await browser?.close(); await new Promise(resolve => server.close(resolve)); }
}
within('platform and terminal keyboard shortcuts', main).catch(error => { console.error(error.stack || error); process.exitCode = 1; });
