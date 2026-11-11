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

async function checkSummarizedUpdates(browser, origin, engine, theme, width, showThinking) {
  const label = `${engine}-${theme}-summaries-${width}-thinking-${showThinking}`;
  const events = [
    { seq: 70, ts: 70, type: 'assistant', turn: 70, blocks: [{ type: 'text', text: 'Ordinary reply stays full size.' }], stop_reason: 'end_turn' },
    { seq: 71, ts: 71, type: 'assistant', turn: 71, blocks: [
      { type: 'narration', text: '**First summary** stays visible with a long_wrapping_summary_word_that_must_not_widen_the_phone.' },
      { type: 'narration', text: 'Second, separate summary.' },
    ], stop_reason: 'tool_use' },
    { seq: 72, ts: 72, type: 'assistant', turn: 71, blocks: [{ type: 'thinking', text: 'Genuine private reasoning.' }], stop_reason: 'tool_use' },
    { seq: 73, ts: 73, type: 'assistant', turn: 71, blocks: [
      { type: 'tool_use', id: 'summary-tool', name: 'Bash', input_json: '{"command":"echo summary-fixture"}' },
    ], stop_reason: 'tool_use' },
    { seq: 74, ts: 74, type: 'tool_result', call_id: 'summary-tool', tool: 'Bash', output: 'summary-fixture', is_error: false,
      truncated: false, duration_ms: 1 },
    { seq: 75, ts: 75, type: 'assistant', turn: 71, blocks: [{ type: 'text', text: 'Historical update stored as plain Text.' }], stop_reason: 'end_turn' },
  ];
  let f;
  try {
    f = await fixture(browser, origin, { width, height: 1000 }, theme,
      { choice: 'off', effective: 'off', os: 'reduce' }, partial => { f = partial; }, {}, {
        instrumentation: false, events,
        path: '/sessions/parent', layout: layout({ view: 'session', id: 'parent' }),
        setup: async ({ context }) => context.route('**/api/**', route => {
          if (new URL(route.request().url()).pathname.endsWith('/settings/transcript')) {
            return route.fulfill({ json: { show_thinking: showThinking } });
          }
          return route.fallback();
        }),
      });
    const { page } = f;
    const items = page.locator('[data-summarized-update]');
    await items.nth(1).waitFor();
    assert.equal(await items.count(), 2, 'each narration block retains its own item');
    assert.equal(await items.getByText('Summarized update', { exact: true }).count(), 2);
    assert.equal(await items.first().locator('strong').textContent(), 'First summary', 'keep summary Markdown');
    assert.equal(await items.locator('details, summary, button').count(), 0, 'visible summaries, not a Thinking disclosure or unbuilt action');
    const ordinary = page.locator('#parent-e70 .md');
    const historical = page.locator('#parent-e75');
    assert.equal(await historical.locator('[data-summarized-update]').count(), 0, 'do not reclassify old Text');
    assert.equal((await ordinary.textContent()).trim(), 'Ordinary reply stays full size.');
    await page.waitForFunction(show => document.body.textContent.includes('Genuine private reasoning.') === show, showThinking);
    const sizes = await page.evaluate(() => ({
      reply: parseFloat(getComputedStyle(document.querySelector('#parent-e70 .md')).fontSize),
      summary: parseFloat(getComputedStyle(document.querySelector('[data-summarized-update] .md')).fontSize),
      overflow: document.documentElement.scrollWidth > innerWidth,
    }));
    assert(sizes.summary < sizes.reply, `summaries are quieter than replies: ${JSON.stringify(sizes)}`);
    assert.equal(sizes.overflow, false, 'summary wraps on phones');
    const icons = await page.locator('#parent-e73 svg').count();
    assert(icons > 0, 'the neighboring tool retains its icon');

    const append = event => {
      const seq = f.send(event);
      events.push({ ...event, seq, ts: Date.now() });
      return seq;
    };
    append({ type: 'turn_started', turn: 101 });
    f.stream({ type: 'thinking', turn: 101, text: 'Transient unclassified thinking.' });
    if (showThinking) await page.getByText('Transient unclassified thinking.', { exact: true }).waitFor();
    const live = append({ type: 'assistant', turn: 101, blocks: [{ type: 'narration', text: 'Live classified summary.' }], stop_reason: 'tool_use' });
    await page.locator(`#parent-e${live} [data-summarized-update]`).waitFor();
    assert.equal(await items.count(), 3);
    assert.equal(await page.getByText('Transient unclassified thinking.', { exact: true }).count(), 0, 'durable narration replaces provisional thinking');
    append({ type: 'assistant', turn: 101, blocks: [{ type: 'text', text: 'Actual answer after the summary.' }], stop_reason: 'end_turn' });
    append({ type: 'turn_ended', turn: 101, reason: 'completed' });
    await page.getByText('Actual answer after the summary.', { exact: true }).waitFor();

    await f.reload({ waitUntil: 'domcontentloaded' });
    await page.locator(`#parent-e${live} [data-summarized-update]`).waitFor();
    assert.equal(await items.count(), 3, 'replay retains summary provenance');
    assert.equal(await items.getByText('Summarized update', { exact: true }).count(), 3);
    assert.equal(await page.locator('#parent-e73 svg').count(), icons, 'tool icons survive replay unchanged');
    assert.equal(await page.locator('#parent-e75 [data-summarized-update]').count(), 0, 'historical Text stays unchanged on replay');
    await page.waitForFunction(show => document.body.textContent.includes('Genuine private reasoning.') === show, showThinking);
    assert.equal(await page.getByText('Ask for the full text', { exact: true }).count(), 0);
    assert.deepEqual(f.errors, []);
    await page.screenshot({ path: path.join(shots, `${label}.png`) });
    console.log(`PASS ${label}: separate quiet labeled summaries, live/replay, old Text, thinking preference, Markdown, wrapping and tool icons`);
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
        for (const theme of (process.env.THEME || 'light,dark').split(',')) for (const width of [1440, 320]) for (const show of [false, true]) {
          await checkSummarizedUpdates(browser, origin, engine, theme, width, show); passed++;
        }
      } finally { await browser.close(); }
    }
    console.log(`${passed} passed; no retries`);
  } finally { await new Promise(resolve => server.close(resolve)); }
})().catch(error => { console.error(error); process.exitCode = 1; });
