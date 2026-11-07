#!/usr/bin/env node
'use strict';

// Two independent streams of the SAME root session. A normal subagent uses
// that root publisher; inventing a child-only publication tests the wrong path.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');
const before = process.env.BEFORE === '1';
const shots = process.env.SCREENSHOTS || '/tmp/n3-ui/item2/browser';
const originalTitle = 'Original subagent report — a deliberately long version title that must remain discoverable without widening a narrow tile or phone';
const publication = (seq, name, title) => ({ seq, ts: seq, type: 'output', name, title, mime: 'text/html', size: 42 });
const history = [
  { seq: 1, ts: 1, type: 'started', connection_id: 'test', model: 'test', cwd: '/tmp', system_prompt: '', tools: [] },
  publication(2, 'report.html', originalTitle),
  ...Array.from({ length: 20 }, (_, i) => publication(i + 3, `other-${i}.html`, `Other deliverable ${i}`)),
  publication(27, 'report.html', 'Latest subagent report'),
  { seq: 900, ts: 900, type: 'assistant', turn: 1, blocks: [{ type: 'text', text: 'Published through the shared root sink.' }], stop_reason: 'end_turn' },
  { seq: 901, ts: 901, type: 'turn_ended', turn: 1, reason: 'completed' },
];
const cases = { desktop: { width: 1600, height: 1000 }, narrow: { width: 1600, height: 1000 }, phone: { width: 390, height: 844 } };
async function check(browser, origin, theme, name) {
  console.log(`START ${theme}/${name}`);
  const streams = new Set();
  let opened = 0;
  let catalog;
  const f = await fixture(browser, origin, cases[name], theme,
    { choice: 'off', effective: 'off', os: 'reduce' }, () => {}, { serviceWorkers: 'allow' }, {
      instrumentation: false, path: '/sessions/parent#e27',
      ready: 'textarea',
      layout: { tiles: [{ key: 2, columns: [
        { key: 3, view: { view: 'session', id: 'parent', target: { q: '', anchor: 'e27', open: [], at: 27 } } },
        { key: 5, view: { view: 'session_outputs', id: 'parent', output: '' } },
      ], cursor: 1, active: 5, used: 5, back: [], forward: [], stack: 0, minimized: false }], focus: 0, next: 6, full: false },
      setup: async ({ context }) => {
        await context.routeWebSocket('**/sessions/*/stream*', socket => {
          const url = new URL(socket.url());
          assert.equal(url.pathname, '/api/v1/sessions/parent/stream');
          opened++;
          streams.add(socket); socket.onClose(() => streams.delete(socket));
          socket.send(JSON.stringify({ type: 'window', from: url.searchParams.has('at') ? 1 : 800 }));
          for (const event of history) socket.send(JSON.stringify({ type: 'event', event }));
          if (url.searchParams.has('at')) socket.send(JSON.stringify({ type: 'synced' }));
          else catalog = socket; // Deliberately finish the independent replay later.
        });
        await context.route('**/api/v1/sessions/parent/outputs/*', route => {
          const seq = new URL(route.request().url()).pathname.split('/').at(-1);
          return route.fulfill({ contentType: 'text/html', body: `<h1>Root output ${seq}</h1>` });
        });
      },
    });
  try {
    const { page } = f;
    console.log(`READY ${theme}/${name}`);
    const transcript = page.locator('[data-pane-motion="3"]');
    const pane = page.locator('[data-pane-motion="5"]');
    await transcript.getByText('Latest subagent report', { exact: true }).waitFor({ state: 'attached' });
    assert(await transcript.getByText('Latest subagent report', { exact: true }).count(), 'root transcript already has Published');
    if (name === 'narrow') await page.addStyleTag({ content: '[data-tile-motion] { width: 338px !important; height: 550px !important; }' });
    console.log(`STREAMS ${theme}/${name}: ${streams.size}`);
    await settle(page);
    if (!(await pane.isVisible())) {
      if (name === 'phone') {
        await page.getByRole('button', { name: /Open session sheet/ }).click();
        await page.getByRole('button', { name: 'Outputs', exact: true }).last().click();
      } else {
        await page.getByRole('button', { name: 'Actions', exact: true }).click();
        await page.locator('.c6-pop:visible').getByRole('button', { name: 'Outputs', exact: true }).click();
      }
    }
    await within('independent root catalog stream opened', async () => { while (!catalog || opened < 2) await new Promise(r => setTimeout(r, 20)); });
    await pane.getByText(before ? /Nothing published yet/ : 'Loading published files…', { exact: !before }).waitFor();
    if (!before) assert.equal(await pane.getByText(/Nothing published yet/).count(), 0, 'unsynced is not empty');
    await settle(page);
    await page.screenshot({ path: path.join(shots, `${theme}-${name}-loading.png`) });
    catalog.send(JSON.stringify({ type: 'synced' }));
    await pane.getByRole('button', { name: /Latest subagent report/ }).waitFor();
    const first = pane.getByRole('button').filter({ hasText: /deliverable|subagent report/ }).first();
    if (before) {
      assert.match(await first.textContent(), /Other deliverable 19/, 'baseline orders groups by first publication');
      assert.match(await pane.locator('h3').textContent(), /Other deliverable 19/, 'baseline preview ignores newest republication');
    } else {
      assert.match(await first.textContent(), /Latest subagent report/);
      assert.match(await pane.locator('h3').textContent(), /Latest subagent report/);
    }
    await settle(page);
    await page.screenshot({ path: path.join(shots, `${theme}-${name}-catalog.png`) });
    await pane.getByRole('button', { name: /Latest subagent report/ }).click();
    if (!before) assert.match(await pane.locator('select option[value="2"]').textContent(), /Original subagent report/);
    await pane.locator('select').selectOption('2');
    await pane.locator('h3').filter({ hasText: 'Original subagent report' }).waitFor();
    await page.frameLocator('[data-pane-motion="5"] iframe').getByRole('heading', { name: 'Root output 2' }).waitFor();
    const versionBounds = await pane.locator('select').evaluate(el => {
      const s = el.getBoundingClientRect(), p = el.closest('[data-pane-motion]').getBoundingClientRect();
      return { left: s.left, right: s.right, paneLeft: p.left, paneRight: p.right };
    });
    if (before) console.log(`baseline version bounds: ${JSON.stringify(versionBounds)}`);
    else assert(versionBounds.left >= versionBounds.paneLeft && versionBounds.right <= versionBounds.paneRight,
      `version control stays within its pane: ${JSON.stringify(versionBounds)}`);
    await page.screenshot({ path: path.join(shots, `${theme}-${name}-original.png`) });
    const live = publication(902, 'live.html', 'Live root publication');
    for (const socket of streams) socket.send(JSON.stringify({ type: 'event', event: live }));
    await pane.getByRole('button', { name: /Live root publication/ }).waitFor();
    assert.deepEqual(f.errors, []);
    console.log(`PASS ${theme}/${name}${before ? ' baseline false-empty and stale-order reproduced' : ''}`);
  } finally { await f.context.close(); }
}
async function main() {
  fs.mkdirSync(shots, { recursive: true });
  const { server, origin } = await serve();
  const playwright = require(process.env.PLAYWRIGHT_CORE || '/home/zheilbron/.bun/install/cache/playwright-core@1.50.1@@@1');
  const browser = await launchBrowser(playwright, process.env.ENGINE || 'chromium');
  try {
    for (const theme of (process.env.THEMES || 'light,dark').split(','))
      for (const name of (process.env.VIEWS || 'desktop,narrow,phone').split(',')) await check(browser, origin, theme, name);
  } finally { await browser.close(); await new Promise(resolve => server.close(resolve)); }
}
within('same-root Outputs replay and versions', main).catch(error => { console.error(error); process.exitCode = 1; });
