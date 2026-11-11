#!/usr/bin/env node
'use strict';

// Production WASM only: motion.cjs owns the private loopback head and browsers.
// Every matrix case runs once; failures remain failures, including startup traps.
const assert = require('node:assert/strict');
const crypto = require('node:crypto');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, within, launchBrowser, session } = require('./motion.cjs');
const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
const root = path.resolve(process.env.ROOT || path.join(__dirname, '../dist'));
const shots = path.join(process.env.SCREENSHOTS || '/tmp/s5-api-message-label', `${Date.now()}-${process.pid}`);
const views = {
  desktop: { width: 1440, height: 1000 },
  phone: { width: 390, height: 844 },
  'phone-small': { width: 320, height: 568 },
};
const mentionId = '01a18f50-326a-7e26-9348-8ee7c41bb3bd';
const linked = { ...session(mentionId), title: 'Automation result session' };
const messages = [
  { key: 'api-token', origin: { kind: 'api', token_name: 'c6-cli-zmbp' }, label: 'Sent via the API · token c6-cli-zmbp' },
  { key: 'api-empty', origin: { kind: 'api', token_name: '' }, label: 'Sent via the API' },
  { key: 'unknown', origin: { kind: 'unknown' }, label: 'Message · origin unknown' },
  { key: 'human', origin: { kind: 'human' }, own: true },
  { key: 'legacy', own: true }, // Deliberately no origin property in historical JSON.
  { key: 'session', from_session_id: 'sender', from_title: 'Release helper', label: 'Message from another session' },
  { key: 'unknown-attachment-only', origin: { kind: 'unknown' }, label: 'Message · origin unknown', attachmentOnly: true },
  { key: 'api-attachment-only', origin: { kind: 'api', token_name: '' }, label: 'Sent via the API', attachmentOnly: true },
  { key: 'card-answer', origin: { kind: 'unknown' }, command: 'card-answer', mention: true, label: 'Message · origin unknown' },
  { key: 'task-card-start', origin: { kind: 'task_card' }, label: 'From a task card' },
  { key: 'task-card-answer', origin: { kind: 'task_card_answer' }, command: 'card-answer', mention: true, label: 'Answer to a task card' },
  { key: 'delegated', origin: { kind: 'unknown' }, delegated_by: 'parent-call', label: 'From the agent that started it' },
].map((message, i) => ({ ...message, seq: 70 + i,
  editable: !!message.own || (['unknown', 'task_card', 'task_card_answer'].includes(message.origin?.kind) && !message.delegated_by),
  speaker: message.own ? 'You' : ['unknown', 'task_card', 'task_card_answer'].includes(message.origin?.kind) && !message.delegated_by ? 'Message' : 'Sender',
  subject: message.attachmentOnly ? '' : message.mention ? `Started session:${mentionId}.` : `${message.key}: **plain text** & <origin-probe>`,
  rest: message.attachmentOnly || message.mention ? '' : `Second line for ${message.key}.\n\nLiteral <origin-rest> and **not Markdown**; session:${mentionId} stays plain.\n/tmp/${'unbrokenpath'.repeat(24)}/notes.txt`,
  prompt: `Exact model-facing context for ${message.key}.\n<origin-prompt> & **literal**`,
  file: `${message.key}-notes.txt`,
}));
const events = messages.map(message => ({ seq: message.seq, ts: message.seq, type: 'user', command_id: message.key,
  text: message.attachmentOnly ? '' : message.mention ? message.subject : `${message.subject}\n\n${message.rest}`, prompt: message.prompt,
  ...(message.origin ? { origin: message.origin } : {}),
  ...(message.command ? { command: message.command } : {}),
  ...(message.delegated_by ? { delegated_by: message.delegated_by } : {}),
  ...(message.from_session_id ? { from_session_id: message.from_session_id, from_title: message.from_title } : {
    attachments: [{ name: message.file, mime: 'text/plain', size: 123 }],
  }),
}));

async function contained(locator, label) {
  const geometry = await locator.evaluate(el => {
    const r = el.getBoundingClientRect();
    return { x: r.x, right: r.right, width: r.width, scroll: el.scrollWidth, client: el.clientWidth, viewport: innerWidth };
  });
  assert(geometry.width > 0 && geometry.x >= -1 && geometry.right <= geometry.viewport + 1,
    `${label}: visible and inside viewport: ${JSON.stringify(geometry)}`);
  assert(geometry.scroll <= geometry.client + 1, `${label}: no horizontal overflow: ${JSON.stringify(geometry)}`);
  return geometry;
}

async function icon(locator, label) {
  assert.equal(await locator.count(), 1, `${label}: one retained SVG`);
  assert.equal(await locator.getAttribute('aria-hidden'), 'true', `${label}: icon is decorative`);
  assert(await locator.locator('path, circle, rect, line, polyline, polygon, ellipse').count() > 0, `${label}: real icon geometry`);
  const box = await locator.boundingBox();
  assert(box && box.width >= 14 && box.height >= 14, `${label}: icon is painted at readable size`);
}

async function exercise(f, label, phone, theme, evidence) {
  const { page } = f;
  const fixtureOrigin = new URL(page.url()).origin;
  const activate = async locator => {
    // Let the prior disclosure's resize/scroll observers finish before the
    // next native tap, rather than tapping a target while its column moves.
    await settle(page);
    await (phone ? locator.tap() : locator.click());
    await settle(page);
  };
  const save = suffix => page.screenshot({ path: path.join(shots, `${label}-${suffix}.png`) });
  await page.locator(`#parent-e${messages.at(-1).seq}`).waitFor();
  assert.equal(await page.locator('.c6-message-card').count(), messages.filter(message => !message.own).length,
    'API, unknown, task-card, delegated and existing session share the sender card');
  assert.equal(await page.locator('html').evaluate(el => el.classList.contains('dark')), theme === 'dark', 'actual app theme');
  assert.equal(await page.evaluate(() => matchMedia('(prefers-color-scheme: dark)').matches), theme === 'dark', 'OS theme fixture');
  if (phone) {
    // WPE WebKit exposes native touch events but reports zero maxTouchPoints.
    // Production touch_first() uses the coarse-pointer query in both engines.
    const policy = await page.evaluate(() => ({ coarse: matchMedia('(pointer: coarse)').matches,
      points: navigator.maxTouchPoints, touchEvents: 'ontouchstart' in window }));
    assert(policy.coarse && (policy.points > 0 || policy.touchEvents), `real phone input policy: ${JSON.stringify(policy)}`);
  }

  for (const message of messages) {
    f.phase = `${message.key}: content`;
    const row = page.locator(`#parent-e${message.seq}`);
    const card = row.locator('.c6-message-card');
    await row.scrollIntoViewIfNeeded();
    await settle(page);
    const entry = { key: message.key, row: await contained(row, `${message.key} row`) };
    evidence.push(entry);
    if (message.own) {
      assert.equal(await card.count(), 0, `${message.key}: unchanged human/legacy bubble`);
      const bubble = row.locator('.rounded-br-sm');
      assert.equal(await bubble.textContent(), `${message.subject}\n\n${message.rest}`, 'bubble retains the full plain text');
      assert.equal(await bubble.locator('strong, origin-probe, origin-rest, a.c6-mention').count(), 0, 'bubble text is not HTML/Markdown or a generated mention');
      // Chromium 158 serializes specified flex-start/end as flow-start/end.
      // Normalize that spelling, then prove the actual reading-column edge.
      assert.equal(await bubble.evaluate(el => getComputedStyle(el.parentElement).alignItems.replace(/^flow-/, 'flex-')), 'flex-end', 'bubble remains right-aligned');
      const boxes = await bubble.evaluate(el => ({ bubble: el.getBoundingClientRect().right, parent: el.parentElement.getBoundingClientRect().right }));
      assert(Math.abs(boxes.bubble - boxes.parent) < 1, 'bubble right edge stays on the reading column');
      await contained(bubble, `${message.key} bubble`);
    } else {
      assert.equal(await card.count(), 1, `${message.key}: sender card, not your bubble`);
      assert.equal(await row.locator('.rounded-br-sm').count(), 0);
      assert.equal(await card.evaluate(el => getComputedStyle(el).alignItems.replace(/^flow-/, 'flex-')), 'flex-start', 'sender card stays left-aligned');
      const cardEdge = await card.evaluate(el => ({
        direction: getComputedStyle(el).flexDirection,
        gap: el.firstElementChild.getBoundingClientRect().left - el.getBoundingClientRect().left,
      }));
      assert.equal(cardEdge.direction, 'column', 'card is not a reversed flex column');
      assert(Math.abs(cardEdge.gap) < 1, 'card left edge stays on the reading column');
      const caption = card.locator(':scope > div > p').first();
      assert.equal((await caption.textContent()).trim(), message.label, 'exact attribution, including the empty-token fallback');
      await icon(caption.locator('svg'), `${message.key} sender icon`);
      const subject = card.locator(':scope > div > p').nth(1);
      if (message.mention) {
        const link = subject.locator(`a.c6-mention[data-ref="session:${mentionId}"]`);
        assert.equal(await link.count(), 1, 'recorded card answer retains its clickable session mention');
        assert.equal(new URL(await link.getAttribute('href'), page.url()).pathname, `/r/home/sessions/${mentionId}`);
        assert.equal(await link.locator('.c6-mention-dot').count(), 1, 'mention retains the session mark');
        assert.match(await subject.textContent(), /^Started .+\.$/, 'generated answer is not truncated');
        await contained(link, 'card-answer mention');
      } else {
        assert.equal(await subject.textContent(), message.subject, 'subject is exactly the first line, including an empty attachment-only subject');
        assert.equal(await card.locator('a.c6-mention').count(), 0, 'ordinary origin text stays plain');
      }
      assert.equal(await card.locator('.md, strong, origin-probe, origin-rest').count(), 0, 'sender content is readable plain text, not parsed markup');
      if (message.rest) assert(!(await card.textContent()).includes(message.rest), 'only the first line is shown initially');
      entry.style = await subject.evaluate(el => {
        const color = getComputedStyle(el).color;
        const canvas = document.createElement('canvas'), ctx = canvas.getContext('2d');
        ctx.fillStyle = color; ctx.fillRect(0, 0, 1, 1);
        const rgb = [...ctx.getImageData(0, 0, 1, 1).data].slice(0, 3);
        return { color, rgb, background: getComputedStyle(el.parentElement).backgroundColor };
      });
      const brightness = entry.style.rgb.reduce((sum, channel) => sum + channel, 0) / 3;
      assert(theme === 'dark' ? brightness > 180 : brightness < 100, `actual ${theme} subject text color: ${JSON.stringify(entry.style)}`);
      await contained(card, `${message.key} card`);
      await contained(caption, `${message.key} caption`);
      await contained(subject, `${message.key} subject`);
      const source = card.getByTitle('Open the session that sent it', { exact: true });
      assert.equal(await source.count(), message.from_session_id ? 1 : 0, 'API and unknown do not invent a sender session link');
      if (message.from_session_id) {
        assert.equal((await source.textContent()).trim(), message.from_title);
        assert.match(await source.getAttribute('href'), /\/sessions\/sender$/);
      }
      await save(`${message.key}-closed`);
      if (message.attachmentOnly || message.mention) {
        assert.equal(await card.getByRole('button', { name: /Show the full message|Show less/ }).count(), 0, 'no empty disclosure or split generated mention');
      } else {
        await activate(card.getByRole('button', { name: 'Show the full message', exact: true }));
        const rest = card.getByText(message.rest, { exact: true });
        await rest.waitFor();
        assert.equal(await rest.textContent(), message.rest, 'expansion preserves all remaining plain text');
        assert.equal(await rest.locator('a, strong, origin-rest').count(), 0, 'ordinary expanded content stays plain, including session references');
        await contained(rest, `${message.key} expanded text`);
        await contained(row, `${message.key} expanded row`);
        if (message.key === 'api-token') await save('api-token-expanded');
        await activate(card.getByRole('button', { name: 'Show less', exact: true }));
        await rest.waitFor({ state: 'hidden' });
      }
      const disclosure = card.locator('details');
      assert.equal(await disclosure.getAttribute('open'), null, 'model-facing prompt starts folded');
      await activate(disclosure.getByText('What the agent received', { exact: true }));
      await page.waitForFunction(el => el.open, await disclosure.elementHandle());
      await disclosure.locator('pre').waitFor();
      assert.equal(await disclosure.locator('pre').textContent(), message.prompt, 'prompt disclosure preserves exact received text');
      assert(await disclosure.locator('pre').isVisible(), 'prompt disclosure really expands');
      await contained(disclosure.locator('pre'), `${message.key} prompt`);
      if (message.key === 'api-token') await save('api-token-prompt');
      await activate(disclosure.locator('summary'));
    }
    if (!message.from_session_id) {
      const file = row.getByRole('link', { name: message.file, exact: true });
      assert.equal(await file.count(), 1, 'attachment link survives origin presentation');
      const url = new URL(await file.getAttribute('href'), page.url());
      assert.equal(url.pathname, `/api/v1/sessions/parent/attachments/${message.seq}/0`, 'attachment retains its exact event/index path');
      assert.equal(url.searchParams.get('c6_realm'), 'home', 'attachment retains realm routing');
      assert.equal(await file.getAttribute('target'), '_blank');
      await icon(file.locator('svg'), `${message.key} file icon`);
      await contained(file, `${message.key} attachment`);
    }
    if (message.own) await save(`${message.key}-bubble`);

    f.phase = `${message.key}: actions and Ask`;
    // Unknown is neutral, not positively nonhuman. Only positive API/session/
    // delegated provenance removes Rewind. No read message has an Edit flow.
    const edit = row.getByRole('button', { name: 'Edit', exact: true, includeHidden: true });
    const rewind = row.getByRole('button', { name: 'Rewind', exact: true, includeHidden: true });
    assert.equal(await edit.count(), 0, 'read messages never expose Edit, even hidden');
    assert.equal(await rewind.count(), message.editable ? 1 : 0, 'only positive nonhuman provenance bars Rewind');
    const inlineAsk = row.getByRole('button', { name: 'Ask', exact: true, includeHidden: true });
    assert.equal(await inlineAsk.count(), 1, 'every source remains askable');
    const rowNode = await row.elementHandle();
    const askNode = await inlineAsk.elementHandle();
    const askIcon = await inlineAsk.locator('svg').elementHandle();
    const rewindNode = message.editable ? await rewind.elementHandle() : null;
    const rewindIcon = message.editable ? await rewind.locator('svg').elementHandle() : null;
    const assertRetained = async () => {
      assert(await row.evaluate((el, kept) => el === kept, rowNode), 'actions preserve the original transcript row');
      assert(await inlineAsk.evaluate((el, kept) => el === kept, askNode), 'Ask button retains DOM identity');
      assert(await inlineAsk.locator('svg').evaluate((el, kept) => el === kept, askIcon), 'Ask retains its original SVG');
      if (message.editable) {
        assert(await rewind.evaluate((el, kept) => el === kept, rewindNode), 'Rewind button retains DOM identity');
        assert(await rewind.locator('svg').evaluate((el, kept) => el === kept, rewindIcon), 'Rewind retains its original SVG');
      }
      assert.equal(await page.locator('[data-message-edit]').count(), 0, 'opening actions never mounts MessageEdit');
      assert.equal(await page.getByRole('button', { name: 'Send correction', exact: true, includeHidden: true }).count(), 0,
        'read messages have no correction action');
    };
    await assertRetained();
    if (message.editable) {
      // own_message eligibility, not the bubble renderer, selects direct
      // actions. Unknown/task-card input keeps its neutral card presentation.
      await page.mouse.move(0, 0);
      await settle(page);
      assert.equal(await row.getByRole('button', { name: 'Message actions', exact: true }).count(), 0,
        'eligible read messages never expose the phone sheet trigger');
      for (const name of ['Rewind', 'Ask']) {
        const button = row.getByRole('button', { name, exact: true });
        await button.scrollIntoViewIfNeeded();
        await settle(page);
        assert(await button.isVisible(), `${name} is directly visible on phone and desktop`);
        assert(await button.isEnabled(), `${name} is usable`);
        assert.equal(await button.getAttribute('aria-label'), name, `${name} has an explicit accessible name`);
        assert.equal(await button.getAttribute('title'), name, `${name} has a tooltip matching its accessible name`);
        assert.equal((await button.textContent()).trim(), '', `${name} stays icon-only`);
        await icon(button.locator('svg'), `${message.key} direct ${name} icon`);
        const target = await button.evaluate(el => {
          const r = el.getBoundingClientRect(), after = getComputedStyle(el, '::after');
          const expanded = after.content !== 'none' && after.content !== 'normal' && after.position === 'absolute';
          let visible = true;
          for (let node = el; node; node = node.parentElement) {
            const css = getComputedStyle(node);
            if (css.visibility === 'hidden' || Number(css.opacity) === 0) visible = false;
          }
          return { visible, hit: el.contains(document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2)),
            x: r.x, right: r.right, viewport: innerWidth, width: r.width, height: r.height,
            targetWidth: r.width - (expanded ? parseFloat(after.left) + parseFloat(after.right) : 0),
            targetHeight: r.height - (expanded ? parseFloat(after.top) + parseFloat(after.bottom) : 0) };
        });
        // The button intentionally overflows by 8px for its transparent touch
        // target. The enclosing transcript row must still pass contained().
        assert(target.x >= -1 && target.right <= target.viewport + 1, `${name} stays inside the viewport`);
        assert(target.visible && target.hit, `${name} is painted and hit-testable without hover: ${JSON.stringify(target)}`);
        assert(Math.abs(target.width - 28) <= 1 && Math.abs(target.height - 28) <= 1, `${name} keeps a quiet 28px icon button`);
        if (phone) assert(target.targetWidth >= 44 && target.targetHeight >= 44, `${name} has a 44px touch target`);
      }
    }
    if (phone && !message.editable) {
      assert(!(await inlineAsk.isVisible()), 'positive nonhuman rows use the phone sheet, not overlapping inline actions');
      const opener = row.getByRole('button', { name: 'Message actions', exact: true });
      await contained(opener, `${message.key} action opener`);
      await icon(opener.locator('svg'), `${message.key} action icon`);
      await opener.tap();
      const sheet = page.getByRole('dialog', { name: 'Message actions', exact: true });
      await sheet.waitFor();
      assert.match(await sheet.getAttribute('class'), /\bphone-message-actions\b/);
      assert.equal(await sheet.getByText(message.speaker, { exact: true }).count(), 1, 'sheet preserves positive nonhuman attribution');
      for (const other of ['You', 'Message', 'Sender'].filter(speaker => speaker !== message.speaker)) {
        assert.equal(await sheet.getByText(other, { exact: true }).count(), 0, 'never invent a human identity for unknown input');
      }
      if (message.attachmentOnly) assert.equal(await sheet.getByText(message.file, { exact: true }).count(), 1, 'empty-text action context uses the attachment name');
      assert.equal(await sheet.getByRole('button', { name: 'Edit', exact: true, includeHidden: true }).count(), 0,
        'notice sheets never expose Edit');
      assert.equal(await sheet.getByRole('button', { name: 'Rewind…', exact: true }).count(), 0,
        'positive nonhuman provenance bars sheet Rewind');
      const ask = sheet.getByRole('button', { name: 'Ask in a side chat', exact: true });
      await icon(ask.locator('svg'), `${message.key} sheet Ask icon`);
      await contained(sheet, `${message.key} actions sheet`);
      if (message.key === 'api-token') await save(`${message.key}-actions`);
      await ask.tap();
      await sheet.waitFor({ state: 'hidden' });
    } else {
      if (!message.editable) {
        await row.hover();
        await settle(page);
      }
      assert.equal(await row.getByRole('button', { name: 'Message actions', exact: true }).count(), 0,
        'direct actions do not expose the phone sheet trigger');
      await icon(inlineAsk.locator('svg'), `${message.key} inline Ask icon`);
      if (message.key === 'human') await save(`${message.key}-actions`);
      await activate(inlineAsk);
    }
    const input = page.getByPlaceholder(phone ? 'Ask about this in a side chat' : 'Ask about this in a side chat (Enter to send, Esc to cancel)', { exact: true });
    await input.waitFor();
    assert(await input.evaluate(el => document.activeElement === el && !el.closest('[inert]')), 'Ask opens with natural focus and usable input');
    await input.fill(`Inspect ${message.key} without sending`);
    assert.equal(await input.inputValue(), `Inspect ${message.key} without sending`);
    await contained(input, `${message.key} Ask input`);
    if (message.key === 'api-token') await save('api-token-ask');
    await activate(input.locator('..').getByRole('button', { name: 'Cancel', exact: true }));
    await input.waitFor({ state: 'hidden' });
    await assertRetained();

    // Every eligible bubble and neutral notice must really enter Rewind,
    // not merely expose an inert-looking action button.
    if (message.editable) {
      f.phase = `${message.key}: Rewind entry`;
      await row.scrollIntoViewIfNeeded();
      await activate(rewind);
      // RewindPanel is the transcript row's sibling, not inside the row.
      const confirmation = page.getByText(/^Rewind the conversation to before this message\?/).locator('..');
      await confirmation.waitFor();
      assert.equal(await confirmation.getByRole('checkbox').isChecked(), false, 'restoring files still needs explicit consent');
      assert.equal(await confirmation.getByRole('button', { name: 'Rewind', exact: true }).isEnabled(), true, 'rewind is available after the stream is ready');
      await assertRetained();
      await activate(confirmation.getByRole('button', { name: 'Cancel', exact: true }));
      await confirmation.waitFor({ state: 'hidden' });
      await assertRetained();
    }
    // Read RPCs and transcript viewed/seen receipts also use POST. Only the
    // message-action endpoints would send, branch, interrupt or rewind here.
    assert.deepEqual(f.requests.filter(request => /^(POST|PUT|PATCH|DELETE) .*\/(messages|side-chats|rewind|interrupt)(\/|$)/.test(request)), [],
      'opening/cancelling Ask and Rewind never sends, edits, withdraws or rewinds anything');
  }
  f.phase = 'card-answer mention navigation';
  const answer = messages.find(message => message.mention);
  const mention = page.locator(`#parent-e${answer.seq} a.c6-mention[data-ref="session:${mentionId}"]`);
  const navigation = page.waitForRequest(request => request.method() === 'GET' &&
    new URL(request.url()).pathname === `/api/v1/sessions/${mentionId}`);
  await activate(mention);
  await navigation;
  await page.locator(`[id="${mentionId}-e1"]`).waitFor({ state: 'attached' });
  await save('card-answer-navigation');
  assert.equal(page.frames().length, 1, 'message actions and mention navigation never create a frame');
  assert.equal(new URL(page.url()).origin, fixtureOrigin, 'navigation stays in the fixture origin');
  assert.deepEqual(f.errors, [], 'no browser errors, crash reports or external requests');
}

async function check(browser, origin, engine, view, theme) {
  const label = `${engine}-${view}-${theme}`, evidence = [];
  let progress;
  try {
    await within(label, async () => {
      const f = await fixture(browser, origin, views[view], theme, { choice: 'off', os: 'reduce', effective: 'off' },
        current => { progress = current; }, {}, { events, sessions: [linked], instrumentation: false,
          setup: async ({ context }) => {
            await context.route(new RegExp(`/api/v1/sessions/${mentionId}(?:\\?.*)?$`), route => route.fulfill({ json: {
              session: linked, worker_name: 'Motion fixture', worker_online: true, queued: [],
            } }));
          },
        });
      await Promise.race([exercise(f, label, view !== 'desktop', theme, evidence), f.crash]);
    });
    console.log(`PASS ${label}`);
    return { label, passed: true, evidence };
  } catch (error) {
    await progress?.page.screenshot({ path: path.join(shots, `${label}-failure.png`) }).catch(() => {});
    const result = { label, passed: false, phase: progress?.phase, error: error.stack, errors: progress?.errors,
      requests: progress?.requests, console: progress?.consoleMessages, evidence };
    fs.writeFileSync(path.join(shots, `${label}-failure.json`), JSON.stringify(result, null, 2));
    console.error(`FAIL ${label}: ${error.stack}`);
    return result;
  } finally { await progress?.context.close(); }
}

(async () => {
  const engine = process.env.ENGINE || 'both';
  assert(['chromium', 'webkit', 'both'].includes(engine), 'ENGINE=chromium,webkit,both');
  const names = (process.env.VIEWPORT || Object.keys(views).join(',')).split(',');
  const themes = (process.env.THEME || 'light,dark').split(',');
  for (const name of names) assert(Object.hasOwn(views, name), `Unknown VIEWPORT ${name}`);
  for (const theme of themes) assert(['light', 'dark'].includes(theme), `Unknown THEME ${theme}`);
  fs.mkdirSync(shots, { recursive: true });
  fs.copyFileSync(__filename, path.join(shots, 'message-origin.cjs'));
  const { server, origin } = await serve();
  const report = { root, wasm: fs.readdirSync(root, { recursive: true }).filter(file => file.endsWith('.wasm')).map(file => ({ file,
    sha256: crypto.createHash('sha256').update(fs.readFileSync(path.join(root, file))).digest('hex') })), results: [] };
  const record = result => {
    report.results.push(result);
    fs.writeFileSync(path.join(shots, 'results.json'), JSON.stringify(report, null, 2));
  };
  try {
    for (const name of engine === 'both' ? ['chromium', 'webkit'] : [engine]) {
      let browser;
      try {
        browser = await launchBrowser(playwright, name);
      } catch (error) {
        for (const view of names) for (const theme of themes) {
          record({ label: `${name}-${view}-${theme}`, passed: false, phase: 'browser launch', error: error.stack });
        }
        console.error(error);
        continue;
      }
      try {
        for (const view of names) for (const theme of themes) record(await check(browser, origin, name, view, theme));
      } finally { await browser.close(); }
    }
  } finally { await new Promise(resolve => server.close(resolve)); }
  const failures = report.results.filter(result => !result.passed).length;
  console.log(`${report.results.length - failures}/${report.results.length} passed; ${failures} failed (no retries). Evidence: ${shots}`);
  if (failures) process.exitCode = 1;
})().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
