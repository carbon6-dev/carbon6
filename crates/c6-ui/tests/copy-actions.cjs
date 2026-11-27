#!/usr/bin/env node
'use strict';

// Production WASM, private loopback fixtures, real Chromium clipboard. No live
// head, worker, provider or clipboard shim. Run after the integration UI build.
const assert = require('node:assert/strict');
const { serve, fixture, launchBrowser } = require('./motion.cjs');
const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
const prompt = 'Ask session:other about **literal** text.\nSecond line & <tag>.';
const code = 'printf "雪 & <tag>"\n';
const reply = '# Original Markdown\n\n**Bold**, not flattened text.\n\n```bash\n' + code + '```\n';
const output = 'complete tool output\n'.repeat(2000) + 'END (not Bash or report.md)';
const published = '# Published content\n\nNot report.md.\n';
const events = [
  { seq: 70, ts: 70, type: 'user', command_id: 'copy-prompt', text: prompt },
  { seq: 71, ts: 71, type: 'assistant', turn: 1,
    blocks: [{ type: 'text', text: reply }], stop_reason: 'end_turn' },
  { seq: 72, ts: 72, type: 'assistant', turn: 2,
    blocks: [{ type: 'tool_use', id: 'copy-tool', name: 'Bash', input_json: '{ "command": "printf fixture" }' }], stop_reason: 'tool_use' },
  { seq: 73, ts: 73, type: 'tool_result', call_id: 'copy-tool', tool: 'Bash', output,
    is_error: false, truncated: false, duration_ms: 1 },
  { seq: 74, ts: 74, type: 'output', name: 'report.md', title: 'Copy fixture', mime: 'text/markdown', size: published.length },
  { seq: 75, ts: 75, type: 'error', message: 'Full error\nCaused by: fixture failure\n' },
];

async function main() {
  const { server, origin } = await serve();
  const browser = await launchBrowser(playwright, 'chromium');
  let f;
  try {
    f = await fixture(browser, origin, { width: 1440, height: 1100 }, 'light',
      { choice: 'off', effective: 'off', os: 'reduce' }, () => {},
      { permissions: ['clipboard-read', 'clipboard-write'] }, {
        instrumentation: false, events, ready: '#parent-e71',
        setup: async ({ context }) => context.route('**/api/v1/sessions/parent/outputs/74*',
          route => route.fulfill({ contentType: 'text/markdown', body: published })),
      });
    const { page } = f;
    const check = async (button, expected) => {
      await button.scrollIntoViewIfNeeded();
      await button.focus(); // keyboard-visible even without hovering the block
      await button.press('Enter');
      await button.getByRole('status').filter({ hasText: /^Copied$/ }).waitFor();
      assert.equal(await page.evaluate(() => navigator.clipboard.readText()), expected);
      assert.equal(await button.locator('svg').count(), 1, 'success retains the icon');
    };
    await check(page.locator('#parent-e70').getByRole('button', { name: 'Copy message', exact: true }), prompt);
    await check(page.locator('#parent-e71').getByRole('button', { name: 'Copy as Markdown', exact: true }), reply);
    await check(page.locator('#parent-e71').getByRole('button', { name: 'Copy content', exact: true }), code);
    const tool = page.locator('#parent-e72 .c6-tool-details');
    if (!await tool.evaluate(el => el.open)) await tool.locator(':scope > summary').click();
    await check(tool.getByRole('button', { name: 'Copy tool input', exact: true }), '{ "command": "printf fixture" }');
    await check(tool.getByRole('button', { name: 'Copy tool output', exact: true }), output);
    await check(page.locator('#parent-e74').getByRole('button', { name: 'Copy output content', exact: true }), published);
    await check(page.locator('#parent-e75').getByRole('button', { name: 'Copy error', exact: true }), events.at(-1).message);
    assert.deepEqual(f.errors, []);
    console.log('PASS source messages, Markdown, code, tool JSON/full output, published content, errors and retained icons');
  } finally {
    await f?.context.close();
    await browser.close();
    await new Promise(resolve => server.close(resolve));
  }
}
main().catch(error => { console.error(error); process.exitCode = 1; });
