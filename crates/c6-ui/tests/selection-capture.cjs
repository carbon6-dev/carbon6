#!/usr/bin/env node
'use strict';

// Built production UI with a private loopback fixture. Run from the repo root:
// ROOT=crates/c6-ui/dist ENGINE=chromium node crates/c6-ui/tests/selection-capture.cjs
// Set SCREENSHOTS to retain screenshots and JSON. BEFORE=1 expects the original bug.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, within, launchBrowser } = require('./motion.cjs');

const first = { seq: 70, text: 'Selected words from the first reply.' };
const second = { seq: 71, text: 'Different words from the next reply.' };
const events = [first, second].map(({ seq, text }) => ({ seq, ts: seq, type: 'assistant', turn: seq,
  blocks: [{ type: 'text', text }], stop_reason: 'end_turn' }));
const viewport = { width: 1440, height: 1000 };
const before = process.env.BEFORE === '1';
const evidence = process.env.SCREENSHOTS;
const todoText = 'Read the first reply';
const followupText = 'Explain the second reply';

async function select(page, session, entry) {
  const text = page.locator(`[id="${session}-e${entry.seq}"]`).getByText(entry.text);
  await text.scrollIntoViewIfNeeded();
  await settle(page);
  // Measure text only; Range is never installed as the browser's selection.
  const { left, right, y, reselect } = await text.evaluate((el, selected) => {
    const range = document.createRange();
    const nodes = document.createTreeWalker(el, NodeFilter.SHOW_TEXT);
    let node, start = -1;
    while ((node = nodes.nextNode())) {
      start = node.textContent.indexOf(selected);
      if (start >= 0) break;
    }
    if (!node) throw new Error('Selection text must live in a single text node');
    range.setStart(node, start);
    range.setEnd(node, start + selected.length);
    const rects = range.getClientRects();
    if (rects.length !== 1) throw new Error(`Expected one line of fixture text, got ${rects.length} rects`);
    const rect = rects[0];
    const selection = getSelection();
    return { left: rect.left + 1, right: rect.right - 1, y: rect.top + rect.height / 2,
      reselect: !selection.isCollapsed && selection.getRangeAt(0).isPointInRange(node, start + 1) };
  }, entry.text);
  // Escape hides the menu but leaves the highlight. Clear it with a real click
  // before reselecting that line; otherwise the drag moves the selected text.
  if (reselect) await page.mouse.click(left, y);
  await page.mouse.move(left, y);
  await page.mouse.down();
  await page.mouse.move(right, y, { steps: 12 });
  await page.mouse.up();
  await settle(page);
  assert.equal(await page.evaluate(() => getSelection().toString().trim()), entry.text,
    'trusted mouse drag selects the fixture text');
  await page.locator('[data-selection-capture]').waitFor({ state: 'visible' });
}

async function mockPage(browser, origin) {
  const followups = [];
  return fixture(browser, origin, viewport, 'light',
    { name: 'off', choice: 'off', os: 'reduce', effective: 'off' }, () => {}, {}, {
      events,
      setup: async ({ context }) => {
        await context.route('**/api/v1/sessions/parent/{todos,followups}', async route => {
          const request = route.request(), pathname = new URL(request.url()).pathname;
          if (request.method() === 'GET' && pathname.endsWith('/followups')) return route.fulfill({ json: followups });
          if (request.method() !== 'POST') return route.fallback();
          const body = request.postDataJSON();
          if (pathname.endsWith('/todos')) return route.fulfill({ json: { work: 'capture-work', title: 'Capture work' } });
          const followup = { id: 'capture-followup', session: 'parent', ...body, at: 100 };
          followups.push(followup);
          return route.fulfill({ json: followup });
        });
      },
    });
}

// Optional real-head evidence. Use only a disposable, local, auth-off head:
// ORIGIN=http://127.0.0.1:7790 SESSION=<id> FIRST_SEQ=<n> FIRST_TEXT='<single-line phrase>' \
// SECOND_SEQ=<n> SECOND_TEXT='<single-line phrase>' node crates/c6-ui/tests/selection-capture.cjs
// This mode creates a real to-do and follow-up in the scratch session.
async function realPage(browser, origin, session) {
  assert(['localhost', '127.0.0.1', '[::1]'].includes(new URL(origin).hostname), 'ORIGIN must be a local scratch head');
  const context = await browser.newContext({ viewport, colorScheme: 'light', reducedMotion: 'reduce' });
  context.setDefaultTimeout(Number(process.env.C6_TEST_DEADLINE_SECS || 15) * 1000);
  const page = await context.newPage(), errors = [];
  page.on('pageerror', error => errors.push(String(error)));
  const response = await page.goto(new URL(`/r/${process.env.REALM || 'home'}/sessions/${session}`, origin).href);
  assert(response?.ok(), `scratch head did not serve the UI (HTTP ${response?.status()})`);
  await page.locator('textarea').first().waitFor();
  return { page, context, errors };
}

async function run({ page, errors }, session, entries, engine, live) {
  const [first, second] = entries;
  const endpoint = name => `/api/v1/sessions/${session}/${name}`;
  const posts = [];
  page.on('request', request => {
    const pathname = new URL(request.url()).pathname;
    if (request.method() === 'POST' && [endpoint('todos'), endpoint('followups')].includes(pathname)) {
      posts.push({ pathname, body: request.postDataJSON() });
    }
  });
  const report = { engine, before, session, entries, clicks: {}, posts };
  const saved = {};
  const stem = `${engine}-selection-capture-${before ? 'before' : 'after'}`;
  const shot = async label => {
    if (evidence) await page.screenshot({ path: path.join(evidence, `${stem}-${label}.png`) });
  };
  const popup = page.locator('[data-selection-capture]');
  const composer = page.locator('textarea').first();
  const pick = entry => select(page, session, entry);
  const dismiss = async () => {
    await page.keyboard.press('Escape');
    await popup.waitFor({ state: 'hidden' });
  };
  try {
    await pick(first);
    await dismiss();
    report.escapeDismissed = true;
    await pick(first);
    const originalStyle = await popup.getAttribute('style');
    await pick(second); // Replace a still-open pick, not just a dismissed one.
    assert.notEqual(await popup.getAttribute('style'), originalStyle, 'popover follows the moved selection');
    report.moved = true;
    const draft = await composer.inputValue();
    const separator = !draft || draft.endsWith('\n\n') ? '' : draft.endsWith('\n') ? '\n' : '\n\n';
    const quotedDraft = `${draft}${separator}> ${second.text}\n\n`;
    await shot('quote-menu');
    await popup.getByRole('button', { name: 'Quote', exact: true }).click();
    await settle(page);
    const value = await composer.inputValue();
    report.clicks.Quote = { draftUnchanged: value === draft, hasQuote: value === quotedDraft, popupVisible: await popup.isVisible() };
    await shot('quote-click');
    if (!before) {
      assert(report.clicks.Quote.hasQuote, 'Quote puts the moved selection in the draft');
      await popup.waitFor({ state: 'hidden' });
    }
    await dismiss();

    for (const [name, entry, placeholder, text, status] of [
      ['To-do', first, 'What to do (Enter to add)', todoText, /^To-do added to “.+”\.$/],
      ['Ask next', second, 'What to ask or tell it (Enter to hold)', followupText, 'Held in Up next.'],
    ]) {
      await pick(entry);
      await popup.getByRole('button', { name, exact: true }).click();
      const input = popup.getByPlaceholder(placeholder, { exact: true });
      await settle(page);
      report.clicks[name] = { inputVisible: await input.isVisible(), popupVisible: await popup.isVisible() };
      const label = name === 'To-do' ? 'todo' : 'ask-next';
      await shot(`${label}-click`);
      if (!before) {
        assert(report.clicks[name].inputVisible, `${name} opens its input`);
        await input.fill(text);
        if (live) {
          const pathname = endpoint(name === 'To-do' ? 'todos' : 'followups');
          const [response] = await Promise.all([
            page.waitForResponse(res => res.request().method() === 'POST' && new URL(res.url()).pathname === pathname),
            input.press('Enter'),
          ]);
          assert(response.ok(), `${name} POST returned ${response.status()}`);
          saved[name] = await response.json();
        } else await input.press('Enter');
        await popup.getByText(status, { exact: true }).waitFor();
        await shot(`${label}-saved`);
      }
      await dismiss();
    }
    if (before) {
      // Check every click before failing the baseline, not just the first one.
      assert.deepEqual(report.clicks, {
        Quote: { draftUnchanged: true, hasQuote: false, popupVisible: true },
        'To-do': { inputVisible: false, popupVisible: true },
        'Ask next': { inputVisible: false, popupVisible: true },
      }, 'baseline must reproduce all three swallowed clicks');
    } else {
      const upNext = page.locator('[data-up-next]');
      const toggle = upNext.getByRole('button', { name: /^Up next · / });
      if (await toggle.getAttribute('aria-expanded') === 'false') await toggle.click();
      await upNext.getByText(followupText, { exact: true }).waitFor();
      await upNext.getByRole('link', { name: `“${second.text}”`, exact: true }).waitFor();
      report.upNextVisible = true;
      await shot('up-next');
    }
    assert.deepEqual(posts, before ? [] : [
      { pathname: endpoint('todos'), body: { text: todoText, quote: { session, ...first } } },
      { pathname: endpoint('followups'), body: { text: followupText, quote: { session, ...second } } },
    ]);
    if (live && !before) {
      const get = async pathname => (await page.request.get(new URL(pathname, live).href, { failOnStatusCode: true })).json();
      assert(saved['To-do'].work, 'to-do response identifies its work');
      const [work, followups] = await Promise.all([get(`/api/v1/work/${saved['To-do'].work}`), get(endpoint('followups'))]);
      report.persisted = { saved, work, followups };
      assert(work.attached.some(item => item.ref === `session:${session}`), 'work is linked to the scratch session');
      const matches = ({ text, quote }) => item => item.text === text && item.quote?.session === quote.session &&
        item.quote.seq === quote.seq && item.quote.text === quote.text;
      assert(work.todos.some(matches(posts[0].body)),
        'saved work to-do keeps the quote');
      assert(followups.some(matches(posts[1].body)),
        'saved follow-up keeps the quote');
    }
    // The adjacent fixture rows guarantee the overlap geometry; a real
    // scratch session may have arbitrary rows between its two sample texts.
    if (!live && !before) {
      await pick(first);
      const nextEntryId = `${session}-e${second.seq}`;
      const next = page.locator(`[id="${nextEntryId}"]`).getByText(second.text);
      const menu = await popup.boundingBox(), text = await next.boundingBox();
      assert(menu && text, 'original selection and next text are visible');
      await page.evaluate(session => {
        document.addEventListener('mouseup', event => {
          const selection = getSelection();
          const entry = node => (node?.parentElement?.closest(`[id^="${session}-e"]`)?.id || null);
          window.__selectionCaptureRelease = {
            trusted: event.isTrusted, overMenu: !!event.target.closest('[data-selection-capture]'),
            anchor: entry(selection.anchorNode), focus: entry(selection.focusNode),
            collapsed: selection.isCollapsed,
          };
        }, { once: true, capture: true });
      }, session);
      const y = text.y + text.height / 2;
      await page.mouse.move(text.x + 2, y);
      await page.mouse.down();
      await page.mouse.move(text.x + Math.min(text.width - 2, 100), y, { steps: 8 });
      await page.mouse.move(menu.x + menu.width / 2, menu.y + menu.height / 2, { steps: 18 });
      await page.mouse.up();
      await settle(page);
      report.overlap = await page.evaluate(() => window.__selectionCaptureRelease);
      assert(report.overlap.trusted && report.overlap.overMenu && report.overlap.anchor === nextEntryId &&
        report.overlap.focus !== nextEntryId && !report.overlap.collapsed,
        `native drag starts in the next entry and releases onto the old popover: ${JSON.stringify(report.overlap)}`);
      report.overlapDismissed = !await popup.isVisible();
      await shot('overlapping-drag');
      assert(report.overlapDismissed, 'a drag ending outside the transcript must hide the old popover');
    }
    assert.deepEqual(errors, []);
  } finally {
    console.log(JSON.stringify(report));
    if (evidence) fs.writeFileSync(path.join(evidence, `${stem}.json`), JSON.stringify(report, null, 2) + '\n');
  }
}

async function main() {
  const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
  const engine = process.env.ENGINE || 'chromium', live = process.env.ORIGIN;
  const session = live ? process.env.SESSION : 'parent';
  const entries = live ? ['FIRST', 'SECOND'].map(key => ({ seq: Number(process.env[`${key}_SEQ`]), text: process.env[`${key}_TEXT`] })) : [first, second];
  assert(/^[\w-]+$/.test(session || '') && entries.every(({ seq, text }) => Number.isSafeInteger(seq) && seq > 0 && text),
    'Scratch ORIGIN requires SESSION, FIRST_SEQ, FIRST_TEXT, SECOND_SEQ and SECOND_TEXT');
  if (evidence) fs.mkdirSync(evidence, { recursive: true });
  const built = live ? null : await serve();
  try {
    const browser = await launchBrowser(playwright, engine);
    try {
      await within(`${engine} selection capture`, async () => {
        const f = live ? await realPage(browser, live, session) : await mockPage(browser, built.origin);
        try { await run(f, session, entries, engine, live); } finally { await f.context.close(); }
      });
      console.log(`${engine}: selection capture ${before ? 'reproduced all three baseline failures' : 'passed'}`);
    } finally { await browser.close(); }
  } finally { if (built) await new Promise(resolve => built.server.close(resolve)); }
}

main().catch(error => { console.error(error); process.exitCode = 1; });
