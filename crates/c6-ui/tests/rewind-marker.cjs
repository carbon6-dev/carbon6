#!/usr/bin/env node
'use strict';

// Production WASM, fake loopback streams only; never starts a head or model.
const assert = require('node:assert/strict');
const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');
const playwright = require(process.env.PLAYWRIGHT_CORE || '/home/zheilbron/.bun/install/cache/playwright-core@1.50.1@@@1');
const user = text => ({ type: 'user', command_id: text, text });
const reply = (text, turn = 73) => ({ type: 'assistant', turn,
  blocks: [{ type: 'text', text }], stop_reason: 'interrupted' });
const rewind = (to_seq, extra = {}) => ({ type: 'rewound', to_seq, restored_files: false, ...extra });
const seed = [user('Interrupted question'), { type: 'turn_started', turn: 73 }, reply('Interrupted answer'),
  { type: 'turn_ended', turn: 73, reason: 'interrupted' }, rewind(72, { restored_files: true })]
  .map((event, i) => ({ seq: 72 + i, ts: 72 + i, ...event }));

async function check(browser, origin, engine, viewport, theme) {
  const phone = viewport.width < 768;
  console.log(`START ${engine}/${viewport.width}/${theme}`);
  let f;
  try {
    f = await fixture(browser, origin, viewport, theme, { choice: 'off', effective: 'off', os: 'reduce' },
      partial => { f = partial; }, {}, { events: seed });
    const { page } = f;
    const row = seq => page.locator(`#parent-e${seq}`);
    const click = button => phone ? button.tap() : button.click();
    const button = (seq, action, count) => row(seq).getByRole('button', { name: `${action} ${count}`, exact: true });
    const folded = async (seq, expected) => {
      await row(seq).waitFor({ state: 'attached' });
      assert.equal(await row(seq).evaluate(el => el.classList.contains('hidden')), expected, `row ${seq} fold state`);
      assert(await row(seq).evaluate(el => el.classList.contains('opacity-50')), 'rewound content stays visibly marked');
    };
    const one = '1 message and its reply';
    await button(76, 'show', one).waitFor();
    assert((await row(76).textContent()).includes('Rewound to before “Interrupted question” · files restored'));
    assert.equal(await row(76).locator('svg').count(), 1, 'keep the undo icon');
    const original = await row(72).elementHandle();
    await folded(72, true);
    await click(button(76, 'show', one));
    await button(76, 'hide', one).waitFor();
    await folded(72, false); await folded(74, false);
    assert(await original.evaluate(el => el.isConnected), 'show/hide keeps the original message node');
    assert(await page.getByText('Interrupted answer', { exact: true }).isVisible());
    await click(button(76, 'hide', one));
    await button(76, 'show', one).waitFor();
    await folded(72, true);

    // No Assistant event: turn starts/ends and usage are not a reply.
    const unanswered = f.send(user('Question without a reply'));
    f.send({ type: 'turn_started', turn: 101 });
    const usage = { type: 'usage', model: 'test', input_tokens: 1, output_tokens: 1,
      cache_read_tokens: 0, cache_write_tokens: 0, reasoning_tokens: 0 };
    f.send(usage);
    f.send({ type: 'turn_ended', turn: 101, reason: 'interrupted' });
    const zero = f.send(rewind(unanswered));
    await button(zero, 'show', '1 message, 0 replies').waitFor();
    await click(button(zero, 'show', '1 message, 0 replies'));
    await button(zero, 'hide', '1 message, 0 replies').waitFor();
    await folded(unanswered, false);
    await click(button(zero, 'hide', '1 message, 0 replies'));
    await button(zero, 'show', '1 message, 0 replies').waitFor();

    const multi = f.send(user('First of two questions'));
    f.send(user('Second of two questions'));
    f.send({ ...reply('First reply', 110), blocks: [{ type: 'text', text: 'First reply' }, { type: 'text', text: 'Another block, same reply' }] });
    f.send(reply('Second reply', 110)); f.send(reply('Third reply', 110));
    f.send({ type: 'assistant', turn: 110, blocks: [{ type: 'thinking', text: 'Not a text reply' }], stop_reason: '' });
    f.send({ type: 'assistant', turn: 110, blocks: [{ type: 'tool_use', id: 'rewind-tool', name: 'bash', input_json: '{}' }], stop_reason: 'tool_use' });
    f.send({ type: 'tool_result', call_id: 'rewind-tool', tool: 'bash', output: 'Fixture output', is_error: false, truncated: false, duration_ms: 1 });
    f.send({ ...user('Background notice, not your message'), command: 'task-notification' });
    f.send({ ...user('Hook notice, not your message'), command: 'stop-hook' });
    f.send({ ...user('Other agent, not your message'), from_session_id: 'other', from_title: 'Other agent' });
    f.send(usage);
    const many = f.send(rewind(multi, { restore_error: 'Fixture restore error' }));
    const plural = '2 messages, 3 replies';
    await button(many, 'show', plural).waitFor();
    assert((await row(many).textContent()).includes('files not restored: Fixture restore error'));
    await click(button(many, 'show', plural));
    await button(many, 'hide', plural).waitFor();
    assert(await page.getByText('Third reply', { exact: true }).isVisible());
    await click(button(many, 'hide', plural));
    await button(many, 'show', plural).waitFor();

    // A wider rewind owns only the new conversation, not the old folds again.
    const newest = f.send(user('New question after all folds'));
    f.send(reply('New answer', 130));
    const outer = f.send(rewind(72));
    await button(outer, 'show', one).waitFor();
    await folded(76, true); await folded(zero, true); await folded(many, true);
    await click(button(outer, 'show', one));
    await button(outer, 'hide', one).waitFor();
    await button(76, 'show', one).waitFor(); await button(many, 'show', plural).waitFor();
    await folded(72, true); await folded(unanswered, true); await folded(multi, true); await folded(newest, false);
    await click(button(76, 'show', one));
    await button(76, 'hide', one).waitFor();
    await folded(72, false);
    await click(button(outer, 'hide', one));
    await button(outer, 'show', one).waitFor();
    await folded(newest, true);
    await settle(page);
    const geometry = await button(outer, 'show', one).evaluate(el => {
      const r = el.getBoundingClientRect();
      return { left: r.left, right: r.right, viewport: innerWidth, scroll: document.documentElement.scrollWidth };
    });
    assert(geometry.left >= 0 && geometry.right <= geometry.viewport + 1 && geometry.scroll <= geometry.viewport + 1,
      `marker remains readable without horizontal page overflow: ${JSON.stringify(geometry)}`);
    assert.deepEqual(f.errors, [], 'no browser or dropped-closure errors');
    assert(!f.requests.some(request => /POST .*\/(rewind|messages|side-chats)$/.test(request)), 'disclosure never performs a session action');
    console.log(`PASS ${engine}/${viewport.width}/${theme}: interrupted, unanswered, plural, nested show/hide`);
  } finally { await f?.context.close(); }
}

(async () => {
  const viewports = [{ width: 1440, height: 900 }, { width: 390, height: 844 }, { width: 320, height: 568 }]
    .filter(viewport => !process.env.WIDTH || String(viewport.width) === process.env.WIDTH);
  const themes = ['light', 'dark'].filter(theme => !process.env.THEME || theme === process.env.THEME);
  assert(viewports.length && themes.length, 'WIDTH/THEME must select at least one case');
  const { server, origin } = await serve();
  try {
    for (const engine of (process.env.ENGINE === 'both' ? ['chromium', 'webkit'] : [process.env.ENGINE || 'chromium'])) {
      const browser = await launchBrowser(playwright, engine);
      try {
        for (const viewport of viewports)
          for (const theme of themes) await within(`${engine}/${viewport.width}/${theme}`, () => check(browser, origin, engine, viewport, theme));
      } finally { await browser.close(); }
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
})().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
