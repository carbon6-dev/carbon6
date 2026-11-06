#!/usr/bin/env node
'use strict';

// No build or dependencies: use motion.cjs's production fixture and browser paths.
// BEFORE=1 records the regressed build; the default phase asserts the fixed size.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');
const before = process.env.BEFORE === '1';
const shots = process.env.SCREENSHOTS || `/tmp/q1-screenshots/model-${before ? 'before' : 'after'}`;
const motion = { choice: 'off', effective: 'off', os: 'no-preference' };
const viewports = { desktop: { width: 1440, height: 1000 }, phone: { width: 390, height: 844 }, narrow: { width: 320, height: 568 } };

async function check(browser, origin, engine, name, theme) {
  let progress;
  const label = `${engine}-${name}-${theme}`;
  const phone = name !== 'desktop';
  const save = suffix => progress.page.screenshot({ path: path.join(shots, `${label}-${suffix}.png`) });
  try {
    const f = await fixture(browser, origin, viewports[name], theme, motion, f => { progress = f; });
    const { page } = f;
    const chip = page.getByRole('button', { name: /^Model: .*change it/ }).first();
    await chip.click();
    const input = page.getByPlaceholder('model id', { exact: true });
    await input.waitFor();
    await settle(page);
    const dialog = page.getByRole('dialog', { name: 'Model', exact: true });
    assert.equal(await dialog.count(), phone ? 1 : 0);
    const popup = phone ? dialog : page.locator('div.fixed[data-pop]').filter({ has: input });
    assert.equal(await popup.count(), 1, 'one model surface');
    if (!phone) assert(await popup.evaluate(el => !el.closest('[data-tile-motion]')), 'desktop picker escapes the clipped tile');
    const heading = popup.getByText('Model', { exact: true });
    const note = popup.getByText('It switches when you send your next message.', { exact: true });
    const style = locator => locator.evaluate(el => {
      const s = getComputedStyle(el);
      return { px: parseFloat(s.fontSize), color: s.color, family: s.fontFamily };
    });
    const metrics = { phase: before ? 'before' : 'after', label, popup: await style(popup),
      heading: await style(heading), note: await style(note), input: await style(input) };
    const subagents = popup.getByText('Subagents', { exact: true });
    assert.equal(await subagents.count(), 1, 'the same portal also contains the subagent picker');
    metrics.subagents = await style(subagents);
    await save('picker');
    await input.click();
    const choice = popup.getByRole('button', { name: 'fixture-model-32', exact: true });
    await choice.waitFor();
    metrics.choice = await style(choice);
    await save('choices');
    fs.writeFileSync(path.join(shots, `${label}.json`), JSON.stringify(metrics, null, 2) + '\n');
    console.log(JSON.stringify(metrics));

    const textSize = phone ? 14 : 12;
    if (before && !phone) {
      assert(metrics.popup.px > 12, 'baseline reproduces the lost text-xs inheritance');
    } else {
      assert.equal(metrics.popup.px, textSize, 'desktop retains toolbar size; phone retains text-sm sheet');
    }
    for (const part of ['heading', 'subagents', 'note']) {
      assert.equal(metrics[part].px, metrics.popup.px, `${part} inherits the model surface size`);
    }
    assert.equal(metrics.input.px, phone ? 16 : 12, 'phone fields retain the anti-iOS-zoom override');
    assert.equal(metrics.choice.px, 12, 'offered model rows keep their explicit text-xs');
    assert.equal(metrics.heading.color, metrics.subagents.color, 'both sections retain the same primary text color');
    assert.notEqual(metrics.note.color, metrics.heading.color, 'muted helper copy remains distinct');
    assert(await chip.locator('svg').count() > 0, 'retain the model chip icon');
    await choice.click();
    assert.equal(await input.inputValue(), 'fixture-model-32');
    const effort = popup.getByRole('combobox', { name: 'Reasoning effort', exact: true });
    await effort.selectOption('high');
    assert.equal(await effort.inputValue(), 'high');
    if (phone) {
      await page.evaluate(() => __keyboard(Math.min(innerHeight, 300), 12));
      await settle(page);
      // The focused field must stay reachable inside the keyboard-bounded sheet.
      await input.scrollIntoViewIfNeeded();
      await input.click();
      await page.waitForFunction(el => {
        const r = el.getBoundingClientRect();
        const tail = Math.max(0, -parseFloat(getComputedStyle(el).marginBottom));
        return r.top >= visualViewport.offsetTop - 1 &&
          r.bottom - tail <= visualViewport.offsetTop + visualViewport.height + 1;
      }, await dialog.elementHandle(), { polling: 'raf' });
      const bounds = await dialog.evaluate(el => {
        const r = el.getBoundingClientRect();
        const field = el.querySelector('input').getBoundingClientRect();
        return { top: r.top, bottom: r.bottom, fieldTop: field.top, fieldBottom: field.bottom,
          tail: Math.max(0, -parseFloat(getComputedStyle(el).marginBottom)),
          visibleTop: visualViewport.offsetTop, visibleBottom: visualViewport.offsetTop + visualViewport.height };
      });
      // The existing sheet has a decorative negative-margin spring tail,
      // just as motion.cjs's visibleBounds allows; it must not hide a field.
      assert(bounds.top >= bounds.visibleTop - 1 && bounds.bottom - bounds.tail <= bounds.visibleBottom + 1,
        `phone sheet stays above the keyboard: ${JSON.stringify(bounds)}`);
      assert(bounds.fieldTop >= bounds.top && bounds.fieldBottom <= bounds.visibleBottom, 'model field remains visible');
      metrics.keyboard = bounds;
      fs.writeFileSync(path.join(shots, `${label}.json`), JSON.stringify(metrics, null, 2) + '\n');
      await save('keyboard');
      await dialog.getByRole('button', { name: 'Close Model', exact: true }).click();
      await dialog.waitFor({ state: 'detached' });
    } else {
      await chip.click();
      await input.waitFor({ state: 'detached' });
    }
    assert(!f.requests.some(r => r.startsWith('POST ') && r.endsWith('/messages')), 'changing a model does not send');
    assert.deepEqual(f.errors, []);
    console.log(`PASS model typography ${label} (${before ? 'baseline' : 'fixed'})`);
  } catch (error) {
    await save('failure').catch(() => {});
    throw error;
  } finally { await progress?.context.close(); }
}

async function main() {
  fs.mkdirSync(shots, { recursive: true });
  const playwright = require(process.env.PLAYWRIGHT_CORE || '/home/zheilbron/.bun/install/cache/playwright-core@1.50.1@@@1');
  const engine = process.env.ENGINE || 'both';
  assert(['chromium', 'webkit', 'both'].includes(engine));
  const names = process.env.VIEWPORT ? process.env.VIEWPORT.split(',') : Object.keys(viewports);
  const themes = process.env.THEME ? process.env.THEME.split(',') : ['light', 'dark'];
  for (const name of names) assert(Object.hasOwn(viewports, name), `Unknown VIEWPORT ${name}`);
  for (const theme of themes) assert(['light', 'dark'].includes(theme), `Unknown THEME ${theme}`);
  const { server, origin } = await serve();
  try {
    for (const name of engine === 'both' ? ['chromium', 'webkit'] : [engine]) {
      const browser = await launchBrowser(playwright, name);
      try {
        for (const viewport of names) for (const theme of themes) {
          await within(`${name}/${viewport}/${theme}`, () => check(browser, origin, name, viewport, theme));
        }
      } finally { await browser.close(); }
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
}
within('model picker typography', main).catch(error => { console.error(error.stack || error); process.exitCode = 1; });
