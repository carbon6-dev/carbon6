#!/usr/bin/env node
'use strict';

// Production WASM and deferred loopback responses: the failure must arrive
// after the next draft, not after an arbitrary delay.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');
const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
const before = process.env.BEFORE === '1';
const evidence = process.env.EVIDENCE || '/tmp/n3-ui/item3';
const doc = { id: 'reply-doc', realm: 'home', kind: 'doc', title: 'Replies without lost drafts', rev: 1,
  labels: [], created_by: 'you', created_at: 1, updated_by: 'you', updated_at: 1, pending: 0,
  tabs: [{ id: 'tab', title: 'Reading', blocks: [{ id: 'paragraph', kind: 'paragraph', text: 'Review this paragraph.', attrs: {}, hash: 'hash', by: 'you', rev: 1 }] }] };
const thread = { id: 'thread', doc: doc.id, anchor: { tab: 'tab', block: 'paragraph' }, state: 'open', about: 'Review this paragraph',
  created_by: 'you', created_at: 1, comments: [{ id: 'comment', by: 'you', at: 1, text: 'A conversation with room for another reply.' }] };
const cases = { desktop: { width: 1440, height: 900, count: 1 }, narrow: { width: 1600, height: 900, count: 3 }, phone: { width: 390, height: 844, count: 1 } };
function layout(count) {
  return { tiles: Array.from({ length: count }, (_, i) => ({ key: 2 + i * 2, columns: [{ key: 3 + i * 2,
    view: i === 2 ? { view: 'work', id: '' } : i ? { view: 'sessions' } : { view: 'docs', id: doc.id } }], cursor: 0, active: 3 + i * 2, used: 3 + i * 2, back: [], forward: [], stack: i, minimized: false })), focus: 0, next: 2 + count * 2, full: false };
}
async function check(browser, origin, engine, name, theme) {
  const { width, height, count } = cases[name];
  let f, release, arrived;
  const requestArrived = new Promise(resolve => { arrived = resolve; });
  const responseReleased = new Promise(resolve => { release = resolve; });
  try {
    f = await fixture(browser, origin, { width, height }, theme, { choice: 'off', effective: 'off', os: 'reduce' }, () => {}, {}, {
      layout: layout(count), path: `/docs/${doc.id}`, ready: '[data-block="paragraph"]',
      setup: async ({ context }) => context.route('**/api/**', async route => {
        const p = new URL(route.request().url()).pathname;
        if (p.endsWith(`/docs/${doc.id}/threads/thread/comments`)) {
          assert.equal(route.request().postDataJSON().text, 'Reply A: keep me too.');
          arrived(); await responseReleased;
          return route.fulfill({ status: 503, json: { error: 'Reply A could not be sent' } });
        }
        if (p.endsWith(`/docs/${doc.id}/threads`)) return route.fulfill({ json: [thread] });
        if (p.endsWith(`/docs/${doc.id}`)) return route.fulfill({ json: doc });
        if (p.endsWith('/docs')) return route.fulfill({ json: [{ ...doc, tabs: undefined }] });
        return route.fallback();
      }),
    });
    const { page } = f;
    console.log(`READY ${engine}/${name}/${theme}`);
    if (count > 1) assert((await page.locator('[data-tile-motion="2"] > section').boundingBox()).width < 640, 'real narrow tile');
    const open = async () => {
      const block = page.locator('[data-block="paragraph"]'); await block.hover();
      await block.locator('[data-marks] button').click();
      await page.locator('[data-thread="thread"] textarea').waitFor();
    };
    await open();
    const area = page.locator('[data-thread="thread"] textarea');
    await area.fill('Reply A: keep me too.');
    await page.getByRole('button', { name: 'Send the reply', exact: true }).click();
    await within('reply request arrived', () => requestArrived);
    await area.fill('Draft B: still writing.');
    release();
    await page.getByText('Reply A could not be sent', { exact: false }).waitFor();
    await settle(page);
    await page.screenshot({ path: path.join(evidence, `${before ? 'before' : 'after'}-${engine}-${name}-${theme}.png`) });
    if (before) {
      assert.equal(await area.inputValue(), 'Reply A: keep me too.', 'baseline demonstrates overwrite of B');
    } else {
      assert.equal(await area.inputValue(), 'Draft B: still writing.');
      const failed = page.locator('[data-failed-reply]');
      assert.equal(await failed.locator('pre').textContent(), 'Reply A: keep me too.');
      assert(await failed.getByRole('button', { name: 'Restore reply' }).isDisabled(), 'restore cannot replace B');
      // Real reload tears down the component: both texts must remain recoverable.
      await page.reload(); await page.locator('[data-block="paragraph"]').waitFor();
      if (!(await area.count())) await open();
      await area.waitFor();
      assert.equal(await area.inputValue(), 'Draft B: still writing.');
      assert.equal(await failed.locator('pre').textContent(), 'Reply A: keep me too.');
      await area.fill(''); await failed.getByRole('button', { name: 'Restore reply' }).click();
      assert.equal(await area.inputValue(), 'Reply A: keep me too.');
      assert.equal(await failed.count(), 0);
    }
    assert.deepEqual(f.errors, []);
    console.log(`PASS ${engine}/${name}/${theme}${before ? ' (baseline overwrite reproduced)' : ''}`);
  } finally { release(); await f?.context.close(); }
}

// Exercise the shared production decoder without a real head. A successful
// status still needs valid JSON; failures retain structured/fallback errors,
// and a later 401 must replace an already-mounted app with sign-in.
async function responses(browser, origin, engine) {
  for (const kind of ['malformed', 'structured', 'fallback', 'body-read', 'unauthorized']) {
    let f, release = () => {};
    const released = new Promise(resolve => { release = resolve; });
    try {
      f = await fixture(browser, origin, { width: 1440, height: 900 }, 'light',
        { choice: 'off', effective: 'off', os: 'reduce' }, partial => { f = partial; }, {}, {
          ready: kind === 'unauthorized' ? 'textarea' : 'body',
          setup: async ({ context, page }) => {
            if (kind === 'body-read') await page.addInitScript(() => {
              const text = Response.prototype.text;
              Response.prototype.text = function (...args) {
                return this.url.endsWith('/auth/status') ? Promise.reject(new Error('Fixture body read failed')) : text.apply(this, args);
              };
            });
            if (kind === 'unauthorized') {
              await context.route('**/api/v1/connections', async route => {
                await released;
                await route.fulfill({ status: 401, json: { error: 'Fixture expired sign-in' } });
              });
            } else if (kind !== 'body-read') {
              await context.route('**/api/v1/auth/status', route => kind === 'structured'
                ? route.fulfill({ status: 503, json: { error: 'Fixture decoder unavailable' } })
                : route.fulfill({ status: kind === 'fallback' ? 403 : 200, contentType: 'application/json', body: '{' }));
            }
          },
        });
      const { page } = f;
      if (kind === 'unauthorized') {
        release();
        await page.getByRole('heading', { name: 'Sign in to carbon6', exact: true }).waitFor();
        assert.equal(await page.locator('textarea').count(), 0, '401 removes the signed-in workspace');
      } else {
        const error = page.getByText('Cannot reach the head:', { exact: false });
        await error.waitFor();
        const message = await error.textContent();
        const expected = { malformed: /EOF while parsing an object/, structured: /Fixture decoder unavailable/,
          fallback: /Forbidden/, 'body-read': /Fixture body read failed/ }[kind];
        assert.match(message, expected);
      }
      assert.deepEqual(f.errors, [], 'no panic or browser crash on an API failure');
      console.log(`PASS ${engine}/responses/${kind}`);
    } finally { release(); await f?.context.close(); }
  }
}

(async () => {
  fs.mkdirSync(evidence, { recursive: true });
  const { server, origin } = await serve();
  try {
    for (const engine of (process.env.ENGINES || 'chromium').split(',')) {
      const browser = await launchBrowser(playwright, engine);
      try {
        if (process.argv.includes('--responses')) await responses(browser, origin, engine);
        else for (const name of Object.keys(cases)) for (const theme of ['light', 'dark']) await check(browser, origin, engine, name, theme);
      }
      finally { await browser.close(); }
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
})().catch(error => { console.error(error); process.exitCode = 1; });
