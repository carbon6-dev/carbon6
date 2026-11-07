#!/usr/bin/env node
'use strict';

const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, launchBrowser } = require('./motion.cjs');
const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
const before = process.env.BEFORE === '1';
const evidence = process.env.EVIDENCE || '/tmp/n3-ui/item9';
const title = 'Review "quoted" cards: \\ <config> & 日本語';
const renamed = 'My "work": \\ <script> & 日本語';
const card = { id: 91, realm: 'home', kind: 'task', title, document: 'Keep this card as a to-do in another work item.',
  state: 'open', priority: 'normal', source: 'session:parent', created_at: 1, updated_at: 1, sessions: ['parent'],
  context: { session: 'parent', prompt: 'Review the config safely.' }, form: [],
  options: ['Start it', 'Do it here', 'Add to this work', 'Add as new work', 'Not now'].map(label => ({ label, does: '' })) };
const cases = { desktop: { width: 1440, height: 900, count: 1 }, narrow: { width: 1600, height: 900, count: 3 }, phone: { width: 390, height: 844, count: 1 } };
function layout(count) {
  return { tiles: Array.from({ length: count }, (_, i) => ({ key: 2 + i * 2, columns: [{ key: 3 + i * 2,
    view: i === 2 ? { view: 'work', id: '' } : i ? { view: 'sessions' } : { view: 'decisions' } }], cursor: 0, active: 3 + i * 2, used: 3 + i * 2, back: [], forward: [], stack: i, minimized: false })), focus: 0, next: 2 + count * 2, full: false };
}
async function check(browser, origin, engine, name, theme) {
  const { width, height, count } = cases[name], answers = [];
  let f;
  try {
    f = await fixture(browser, origin, { width, height }, theme, { choice: 'off', effective: 'off', os: 'reduce' }, () => {}, {}, {
      layout: layout(count), path: '/decisions', ready: '[data-tile-motion="2"] main',
      setup: async ({ context }) => context.route('**/api/**', route => {
        const p = new URL(route.request().url()).pathname;
        if (p.endsWith('/decisions/91/answer')) {
          answers.push(route.request().postDataJSON());
          // A failed answer keeps the card available for the next interaction.
          return route.fulfill({ status: 503, json: { error: 'Save unavailable; try again' } });
        }
        if (p.endsWith('/decisions/count')) return route.fulfill({ json: { open: 2 } });
        if (p.endsWith('/decisions')) return route.fulfill({ json: [{ ...card, id: 92, title: 'Another card must not seed this name' }, card] });
        if (p.endsWith('/work')) return route.fulfill({ json: [{ id: 'existing', realm: 'home', version: 1, title: 'Existing destination', status: 'open', state: 'open', created_at: 1, updated_at: 1, todos: [], attached: [], live: [], labels: [] }] });
        return route.fallback();
      }),
    });
    const { page } = f, chooser = page.getByRole('dialog', { name: 'Save elsewhere' });
    console.log(`READY ${engine}/${name}/${theme}`);
    if (process.env.DEBUG_FIXTURE) console.log(await page.locator('[data-tile-motion="2"]').textContent());
    assert.equal(await page.locator('html').evaluate(el => el.classList.contains('dark')), theme === 'dark', 'actual theme');
    if (count > 1) assert((await page.locator('[data-tile-motion="2"] > section').boundingBox()).width < 640, 'real narrow tile');
    const open = async () => { await page.locator('#decision-91').getByRole('button', { name: 'Save elsewhere…', exact: true }).click(); await chooser.waitFor(); };
    await open();
    await settle(page);
    await page.screenshot({ path: path.join(evidence, `${before ? 'before' : 'after'}-${engine}-${name}-${theme}-chooser.png`) });
    await chooser.getByRole('button', { name: 'New work', exact: true }).click();
    if (before) {
      await page.getByText('Save unavailable; try again', { exact: false }).waitFor();
      assert.equal(answers.length, 1, 'baseline creates immediately');
      assert.equal(answers[0].option, 'Add as new work');
      await page.screenshot({ path: path.join(evidence, `before-${engine}-${name}-${theme}-submitted.png`) });
    } else {
      const input = chooser.getByRole('textbox', { name: 'Work name', exact: true });
      await input.waitFor(); assert.equal(await input.inputValue(), title);
      assert(await input.evaluate(el => el === document.activeElement), 'name focused');
      assert.equal(answers.length, 0, 'New work is not confirmation');
      await input.fill('');
      assert(await chooser.getByRole('button', { name: 'Create work', exact: true }).isDisabled());
      await input.fill(renamed); await settle(page);
      await page.screenshot({ path: path.join(evidence, `after-${engine}-${name}-${theme}-name.png`) });
      const r = await input.boundingBox(); assert(r.x >= 0 && r.x + r.width <= width, 'input fits viewport');
      await input.press('Escape'); await chooser.waitFor({ state: 'detached' });
      assert.equal(answers.length, 0, 'Escape never saves');
      assert(await page.locator('#decision-91').getByRole('button', { name: 'Save elsewhere…', exact: true }).evaluate(el => el === document.activeElement), 'Escape returns focus');
      await open(); await chooser.getByRole('button', { name: 'New work', exact: true }).click();
      assert.equal(await input.inputValue(), title, 'fresh choice reseeds selected card title');
      await input.fill(renamed); await input.press('Enter');
      await page.getByText('Save unavailable; try again', { exact: false }).waitFor();
      assert.equal(answers.length, 1); assert.equal(answers[0].option, 'Add as new work');
      assert.deepEqual(answers[0].values, { work_title: renamed });
      await open(); await chooser.getByRole('button', { name: 'New work', exact: true }).click();
      const answered = page.waitForResponse(r => r.url().endsWith('/decisions/91/answer'));
      await chooser.getByRole('button', { name: 'Create work', exact: true }).click(); await answered;
      assert.equal(answers.length, 2); assert.deepEqual(answers[1].values, { work_title: title });
      // Existing destinations keep their original action and need no name.
      await page.getByText('Save unavailable; try again', { exact: false }).waitFor();
      await open(); const existing = page.waitForResponse(r => r.url().endsWith('/decisions/91/answer'));
      await chooser.getByRole('button', { name: 'Existing destination', exact: true }).click(); await existing;
      assert.equal(answers[2].option, 'Add to work:existing');
      assert.deepEqual(answers[2].values, {});
    }
    assert.deepEqual(f.errors, []);
    console.log(`PASS ${engine}/${name}/${theme}${before ? ' (baseline instant create reproduced)' : ''}`);
  } finally { await f?.context.close(); }
}
(async () => {
  fs.mkdirSync(evidence, { recursive: true });
  const { server, origin } = await serve();
  try {
    for (const engine of (process.env.ENGINES || 'chromium').split(',')) {
      const browser = await launchBrowser(playwright, engine);
      try { for (const name of Object.keys(cases)) for (const theme of ['light', 'dark']) await check(browser, origin, engine, name, theme); }
      finally { await browser.close(); }
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
})().catch(error => { console.error(error); process.exitCode = 1; });
