#!/usr/bin/env node
'use strict';

// Real production renderer in transcript and document views; private loopback
// head fixtures only. Never inject replacement markup into the page.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const playwright = require('playwright-core');
const { serve, fixture, launchBrowser } = require('./motion.cjs');
const shots = process.env.SCREENSHOTS || '/tmp/s9-markdown-html';
const DOC = 'markdown-html-doc';
const markdown = [
  '| HTML list | Lines | Safety |',
  '| --- | --- | --- |',
  '| <ul onclick="window.__xss=1" style="color:red" class="bad"><li>first<ol start=5><li><b>nested</b></li></ol></li><li>second</li></ul> | line one<br class=bad>line two | <img src=x onerror="window.__xss=1"><scRipt>window.__xss=1; "<b>literal</b>"</scRipt><a href="javascript:window.__xss=1">link</a>&lt;script&gt; |',
  '| <ul><li>unfinished | </li></ul>contained | safe next cell |',
  '',
  'Ordinary <strong title=x>bold</strong> and <kbd class=bad>Enter</kbd>.',
  '',
  '<ul>',
  '<li>block list</li>',
  '</ul>',
  '',
  'After the table and block.',
].join('\n');

function layout(view) {
  return { tiles: [{ key: 2, columns: [{ key: 3, view }], cursor: 0, active: 3, used: 3,
    back: [], forward: [], stack: 0, minimized: false }], focus: 0, next: 4, full: false };
}

async function check(browser, origin, engine, theme, view) {
  assert(['light', 'dark'].includes(theme));
  assert(['transcript', 'doc'].includes(view));
  const label = `${engine}-${theme}-${view}`;
  let f;
  try {
    f = await fixture(browser, origin, { width: 1440, height: 1000 }, theme,
      { choice: 'off', effective: 'off', os: 'reduce' }, partial => { f = partial; }, {}, {
        instrumentation: false,
        path: view === 'doc' ? `/docs/${DOC}` : '/sessions/parent',
        layout: layout(view === 'doc' ? { view: 'docs', id: DOC } : { view: 'session', id: 'parent' }),
        ready: view === 'doc' ? '[data-doc-reading] table' : '.md table',
        events: [{ seq: 70, ts: 70, type: 'assistant', turn: 70,
          blocks: [{ type: 'text', text: markdown }], stop_reason: 'end_turn' }],
        setup: async ({ context }) => context.route('**/api/**', route => {
          const p = new URL(route.request().url()).pathname;
          if (p.endsWith(`/docs/${DOC}`)) return route.fulfill({ json: {
            id: DOC, realm: 'home', kind: 'doc', title: 'Safe Markdown HTML', labels: [], rev: 1,
            created_by: 'you', updated_by: 'you', created_at: 1, updated_at: 1, pending: 0,
            tabs: [{ id: 'reading', title: 'Reading', blocks: [{ id: 'html-table', kind: 'paragraph',
              text: markdown, attrs: {}, hash: 'html-table-hash', by: 'you', rev: 1, children: [] }] }],
          } });
          if (p.endsWith(`/docs/${DOC}/threads`)) return route.fulfill({ json: [] });
          return route.fallback();
        }),
      });
    const { page } = f;
    const content = view === 'doc' ? page.locator('[data-doc-reading]') : page.locator('.md').filter({ has: page.locator('table') });
    const table = content.locator('table');
    const row = table.locator('tbody tr').first();
    await row.locator('td').first().locator(':scope > ul > li').first().waitFor();
    assert.equal(await row.locator('td').first().locator(':scope > ul > li').count(), 2, 'real list items inside the table cell');
    assert.equal(await row.locator('ul > li > ol > li > b').textContent(), 'nested');
    assert.equal(await row.locator('ul').evaluate(el => getComputedStyle(el).listStyleType), 'disc');
    assert.equal(await row.locator('ol').evaluate(el => getComputedStyle(el).listStyleType), 'decimal');
    assert.equal(await row.locator('td').nth(1).locator('br').count(), 1);
    assert.match(await row.locator('td').nth(1).innerText(), /line one\nline two/);
    const unsafe = row.locator('td').nth(2);
    assert.equal(await unsafe.locator('img, script, a, b').count(), 0, 'disallowed HTML and raw-text bodies never create DOM elements');
    assert.match(await unsafe.textContent(), /<img src=x onerror=/);
    assert.match(await unsafe.textContent(), /<scRipt>window\.__xss=1; "<b>literal<\/b>"<\/scRipt>/);
    assert.match(await unsafe.textContent(), /<a href="javascript:/);
    assert.match(await unsafe.textContent(), /<script>$/);
    assert.equal(await table.locator('tbody tr').nth(1).locator('ul, li').count(), 0, 'unclosed tags stay text, not cross-cell DOM');
    assert.equal(await table.locator('tbody tr').nth(1).locator('td').nth(1).textContent(), '</li></ul>contained');
    assert.equal(await content.locator('strong').last().textContent(), 'bold');
    assert.equal(await content.locator('kbd').textContent(), 'Enter');
    assert.equal(await content.locator('ul').last().textContent(), '\nblock list\n');
    assert.match(await content.textContent(), /After the table and block\./);
    assert.deepEqual(await content.locator('ul, ol, li, b, br, strong, kbd').evaluateAll(elements =>
      elements.flatMap(el => [...el.attributes].map(a => `${el.tagName}:${a.name}`))), [], 'all HTML attributes stripped');
    assert.equal(await page.evaluate(() => window.__xss), undefined);
    assert.equal(await page.locator('html').evaluate(el => el.classList.contains('dark')), theme === 'dark');
    assert.deepEqual(f.errors, []);
    await page.screenshot({ path: path.join(shots, `${label}.png`) });
    console.log(`PASS ${label}: real list, nested list, break, stripped attributes, escaped unsafe/unbalanced HTML and intact following content`);
  } finally { await f?.context.close(); }
}

(async () => {
  fs.mkdirSync(shots, { recursive: true });
  const { server, origin } = await serve();
  let passed = 0;
  try {
    for (const engine of (process.env.ENGINE || 'chromium,webkit').replace('both', 'chromium,webkit').split(',')) {
      const browser = await launchBrowser(playwright, engine);
      try {
        for (const theme of (process.env.THEME || 'light,dark').split(',')) for (const view of (process.env.VIEW || 'transcript,doc').split(',')) {
          await check(browser, origin, engine, theme, view); passed++;
        }
      } finally { await browser.close(); }
    }
    console.log(`${passed} passed; no retries`);
  } finally { await new Promise(resolve => server.close(resolve)); }
})().catch(error => { console.error(error); process.exitCode = 1; });
