#!/usr/bin/env node
'use strict';

// Production WASM, private loopback fixture. BEFORE=1 captures the old row
// with the same events; it never substitutes markup for the app's views.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, launchBrowser } = require('./motion.cjs');
const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
const before = process.env.BEFORE === '1';
const phase = before ? 'before' : 'after';
const shots = process.env.SCREENSHOTS || '/tmp/q5-told';
const views = {
  desktop: { width: 1440, height: 1000, tiles: 1 },
  narrow: { width: 1040, height: 1000, tiles: 4 },
  phone: { width: 390, height: 844, tiles: 1 },
  'phone-small': { width: 320, height: 568, tiles: 1 },
};
const raw = 'Sent to subagent ops via the API (token "ops"): "AUTOMATED OPS RESUME for approved preview. Read /Users/example/ops/rollout.md"\n\n<subagent id="ops" status="completed">\nReport:\n## Preview deployed\n\nThe **checks passed**.\n</subagent>';
const notices = [
  { text: raw, task_notice: { kind: 'subagent', title: 'zmbp ops', session_id: 'ops', status: 'completed', markdown: '## Preview deployed\n\nThe **checks passed**.\n\n- API attribution retained\n- No services restarted by this test' } },
  { text: 'The background command ended. Raw command output.', task_notice: { kind: 'command', title: 'Run all checks', session_id: '', status: 'completed', markdown: 'All checks passed.' } },
  { text: '<subagent id="review" status="failed">Raw failure digest</subagent>', task_notice: { kind: 'subagent', title: 'Release readiness review with a deliberately long title that must fit a narrow tile', session_id: 'review', status: 'failed', markdown: 'The review could not complete.\n\nThe build returned an error.' } },
  { text: 'The user messaged subagent older-worker directly: "An old event with unknown origin"' },
];
function layout(count) {
  return { tiles: Array.from({ length: count }, (_, i) => ({ key: 2 + i * 2,
    columns: [{ key: 3 + i * 2, view: i ? { view: 'sessions' } : { view: 'session', id: 'parent' } }],
    cursor: 0, active: 3 + i * 2, used: 3 + i * 2, back: [], forward: [], stack: i, minimized: false })),
  focus: 0, next: 2 + count * 2, full: false };
}

async function check(browser, origin, engine, name, theme) {
  let f;
  const label = `${phase}-${engine}-${theme}-${name}`;
  const { width, height, tiles } = views[name];
  try {
    f = await fixture(browser, origin, { width, height }, theme, { choice: 'off', os: 'reduce', effective: 'off' }, () => {}, {}, {
      layout: layout(tiles), events: notices.map((notice, i) => ({ seq: 70 + i, ts: 70 + i, type: 'user', command_id: `notice-${i}`,
        command: 'task-notification', ...notice })),
    });
    const { page } = f;
    const rows = before ? page.locator('details').filter({ has: page.locator('summary').filter({ hasText: 'the agent was told' }) }) : page.locator('.c6-task-notice');
    await rows.last().waitFor();
    assert.equal(await rows.count(), 4);
    await rows.last().scrollIntoViewIfNeeded();
    await settle(page);
    await page.screenshot({ path: path.join(shots, `${label}-closed.png`) });
    if (!before) {
      assert.equal(await rows.locator(':scope > summary svg').count(), 4, 'every row retains its corner-arrow icon');
      const labels = await rows.locator(':scope > summary').allTextContents();
      assert.match(labels[0], /Report from zmbp ops.*finished/);
      assert.match(labels[1], /Background command finished: Run all checks/);
      assert.match(labels[2], /Report from Release readiness review.*failed/);
      assert.equal(labels[3].trim(), 'Background notice');
      assert(labels.every(label => !/the user|the agent was told|<subagent|AUTOMATED OPS/i.test(label)));
      for (const row of await rows.all()) {
        assert.equal(await row.getAttribute('open'), null, 'notice starts closed');
        const geometry = await row.locator(':scope > summary').evaluate(el => {
          const r = el.getBoundingClientRect();
          return { height: r.height, width: r.width, scroll: el.scrollWidth, client: el.clientWidth };
        });
        assert(geometry.height <= 22, `${label}: one quiet line: ${JSON.stringify(geometry)}`);
        assert(geometry.scroll <= geometry.client + 1, `${label}: no horizontal overflow`);
      }
    }
    const report = rows.first();
    console.log(`${label}: opening report`);
    if (before) {
      // The old narrow row's invisible Edit/Rewind/Ask actions overlap its
      // summary. Capture its expanded content using native keyboard activation.
      await report.locator(':scope > summary').press('Enter');
    } else {
      const point = await report.locator(':scope > summary').evaluate(el => {
        const r = el.getBoundingClientRect(), x = r.left + 4, y = r.top + r.height / 2;
        return { x, y, visible: el.contains(document.elementFromPoint(x, y)) };
      });
      assert(point.visible, `${label}: notice actions must not cover its disclosure/icon`);
      await page.mouse.click(point.x, point.y);
    }
    await page.waitForFunction(el => el.open, await report.elementHandle());
    console.log(`${label}: report opened`);
    await report.scrollIntoViewIfNeeded();
    if (!before) {
      await report.locator('.md h2').waitFor();
      assert.equal(await report.locator('.md strong').textContent(), 'checks passed');
      const colors = await report.locator('.md').evaluate(el => ({ color: getComputedStyle(el).color, theme: document.documentElement.classList.contains('dark') }));
      assert.equal(colors.theme, theme === 'dark', `${label}: actual theme applied to report`);
      assert.equal(await report.locator('a[href$="/sessions/ops"]').textContent(), 'Open zmbp ops');
      assert.equal(await report.locator('details').getAttribute('open'), null, 'raw text is not the first expanded content');
      assert.equal(await report.locator('pre:visible').count(), 0);
      assert.equal(await report.locator('pre').textContent(), raw, 'exact text the agent saw is retained');
    }
    await settle(page);
    await page.screenshot({ path: path.join(shots, `${label}-report.png`) });
    if (!before) {
      await report.locator('details > summary').click();
      await report.locator('pre').waitFor();
      await page.screenshot({ path: path.join(shots, `${label}-raw.png`) });
      await report.locator(':scope > summary').click();
      const old = rows.last();
      await old.locator(':scope > summary').click();
      assert(!/the user/i.test(await old.locator(':scope > p').textContent()), 'old input is neutral, never assumed human');
      await old.locator('details > summary').click();
      assert.equal(await old.locator('pre').textContent(), notices[3].text);
    }
    assert.deepEqual(f.errors, [], `${label}: no browser errors`);
    console.log(`${label}: ${before ? 'baseline closed/expanded rows captured' : 'closed/report/raw, Markdown, icon, source link, neutral fallback and geometry checked'}`);
  } finally { await f?.context.close(); }
}

(async () => {
  fs.mkdirSync(shots, { recursive: true });
  const { server, origin } = await serve();
  try {
    for (const engine of (process.env.ENGINE || 'both').replace('both', 'chromium,webkit').split(',')) {
      const browser = await launchBrowser(playwright, engine);
      try {
        for (const theme of (process.env.THEME || 'light,dark').split(',')) {
          for (const name of (process.env.VIEWPORT || Object.keys(views).join(',')).split(',')) await check(browser, origin, engine, name, theme);
        }
      } finally { await browser.close(); }
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
})().catch(error => { console.error(error); process.exitCode = 1; });
