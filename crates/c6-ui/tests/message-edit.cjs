#!/usr/bin/env node
'use strict';

// Built production WASM; each case owns loopback HTTP/WS and browser storage.
// No real head/model/settings writes, implicit send, or timing sleeps.
const assert = require('node:assert/strict');
const { serve, fixture, settle, within, launchBrowser, session } = require('./motion.cjs');
const ownText = 'Return this original message 🧭 to the existing box';
const existingDraft = 'Keep my unfinished draft 📝';
const resetError = 'Claude Code history reset failed; input is blocked. Retry rewind before sending another message.';
const tooLateNote = "The agent had already read this, so it wasn't taken back. Your text is still in the box.";
const upload = { name: 'recovered-original.txt', mime: 'text/plain', data: Buffer.from('original attachment bytes 🧭').toString('base64') };
const draftUpload = { name: 'existing-draft.txt', mime: 'text/plain', data: Buffer.from('keep existing bytes').toString('base64') };
const deferred = () => {
  let resolve;
  const promise = new Promise(r => { resolve = r; });
  return { promise, resolve };
};
const warningText = 'Rewind the conversation to before this message? The messages after it stop counting (they stay visible, folded).';
const queueRow = (page, cid) => page.locator(`[data-queued-command="${cid}"]`);
// The textarea has its own hint wrapper. Chips and the file picker belong to
// the containing Composer box, not that immediate parent.
const composerBox = composer => composer.locator('xpath=ancestor::div[.//input[@type="file"]][1]');

async function waitingInFlow(waiting) {
  assert(await waiting.evaluate(el => {
    let scroller = el.parentElement;
    while (scroller && getComputedStyle(scroller).overflowY !== 'auto') scroller = scroller.parentElement;
    return !!el.closest('[data-follow-content]') && !!scroller &&
      getComputedStyle(el).position === 'static' && getComputedStyle(el).overflowY === 'visible' &&
      getComputedStyle(el).maxHeight === 'none';
  }), 'waiting rows belong to the transcript scroller, in normal flow without an inner scroll limit');
}

async function waitingAtEnd(page) {
  await page.waitForFunction(() => {
    const el = document.querySelector('[data-waiting-messages]');
    let scroller = el?.parentElement;
    while (scroller && getComputedStyle(scroller).overflowY !== 'auto') scroller = scroller.parentElement;
    if (!scroller) return false;
    const row = el.lastElementChild?.getBoundingClientRect(), box = scroller.getBoundingClientRect();
    return row && row.bottom <= box.bottom + 1 && row.bottom > box.top &&
      scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight <= 1;
  });
}

async function checkWaitingPlacement(browser, origin, phone, empty) {
  let progress;
  const connected = deferred();
  const f = await fixture(browser, origin, phone ? { width: 390, height: 844 } : { width: 1440, height: 1000 },
    'dark', { choice: 'off', effective: 'off', os: 'reduce' }, p => { progress = p; }, {}, {
      instrumentation: false,
      setup: async ({ page, context }) => {
        await page.route('**/api/v1/sessions/parent/messages/*/draft', route => route.fulfill({ json: [] }));
        await context.routeWebSocket(/\/sessions\/parent\/stream(?:\?|$)/, ws => {
          if (!empty) ws.send(JSON.stringify({ type: 'event', event: { seq: 1, ts: 1, type: 'assistant', turn: 1,
            blocks: [{ type: 'text', text: 'The only short reply.' }], stop_reason: 'end_turn' } }));
          ws.send(JSON.stringify({ type: 'synced' }));
          connected.resolve(ws);
        });
      },
    }).catch(async error => { await progress?.context.close(); throw error; });
  try {
    const { page } = f;
    const socket = await connected.promise;
    socket.send(JSON.stringify({ type: 'queue', queued: [queued()[0]] }));
    const waiting = page.locator('[data-waiting-messages]');
    await queueRow(page, 'later-one').waitFor();
    await settle(page);
    await waitingInFlow(waiting);
    assert(await waiting.evaluate((el, empty) => {
      const previous = el.previousElementSibling;
      const content = el.closest('[data-follow-content]').getBoundingClientRect();
      const row = el.getBoundingClientRect();
      const gap = parseFloat(getComputedStyle(el.parentElement).rowGap);
      return row.top < content.top + 180 && (empty ||
        (previous?.querySelector('.md')?.textContent.trim() === 'The only short reply.' &&
          Math.abs(row.top - previous.getBoundingClientRect().bottom - gap) <= 1));
    }, empty), 'short/empty transcript leaves waiting input high in the pane, directly after the last content');
    await f.setState('working');
    for (let i = 1; i <= 3; i++) {
      const text = `Stream batch ${i}.\n\n` + 'Growing streamed reply.\n\n'.repeat(35);
      socket.send(JSON.stringify({ type: 'text', turn: 2, seq: 2, text }));
      await page.locator('.md').filter({ hasText: `Stream batch ${i}.` }).waitFor();
      await waitingAtEnd(page);
      assert(await waiting.evaluate(el => {
        const previous = el.previousElementSibling;
        const gap = parseFloat(getComputedStyle(el.parentElement).rowGap);
        return previous?.textContent.includes('Growing streamed reply.') &&
          Math.abs(el.getBoundingClientRect().top - previous.getBoundingClientRect().bottom - gap) <= 1;
      }), 'waiting follows the growing streaming reply without reordering');
    }
    // Mobile WebKit cannot dispatch wheel input. Home exercises the real
    // scroller's input/follow policy in both engines without writing scrollTop.
    await waiting.evaluate(el => {
      let scroller = el.parentElement;
      while (getComputedStyle(scroller).overflowY !== 'auto') scroller = scroller.parentElement;
      scroller.focus({ preventScroll: true });
    });
    await page.keyboard.press('Home');
    await page.getByTitle('To the latest (End)', { exact: true }).waitFor();
    assert(await waiting.evaluate(el => {
      let scroller = el.parentElement;
      while (getComputedStyle(scroller).overflowY !== 'auto') scroller = scroller.parentElement;
      return el.getBoundingClientRect().top >= scroller.getBoundingClientRect().bottom;
    }), 'scrolling up leaves waiting rows outside the transcript viewport');
    socket.send(JSON.stringify({ type: 'text', turn: 2, seq: 2, text: 'Still writing while scrolled up.\n\n'.repeat(100) }));
    await page.locator('.md').filter({ hasText: 'Still writing while scrolled up.' }).waitFor();
    await settle(page);
    assert(await waiting.evaluate(el => {
      let scroller = el.parentElement;
      while (getComputedStyle(scroller).overflowY !== 'auto') scroller = scroller.parentElement;
      return el.getBoundingClientRect().top >= scroller.getBoundingClientRect().bottom;
    }), 'continued streaming does not pull a reader back to the waiting rows');
    await page.getByTitle('To the latest (End)', { exact: true }).click();
    await waitingAtEnd(page);
    socket.send(JSON.stringify({ type: 'event', event: { seq: 2, ts: 2, type: 'assistant', turn: 2,
      blocks: [{ type: 'text', text: 'The completed streamed reply.' }], stop_reason: 'end_turn' } }));
    await page.locator('.c6-transcript-row').filter({ hasText: 'The completed streamed reply.' }).waitFor();
    await settle(page);
    assert(await waiting.evaluate(el => el.previousElementSibling?.textContent.includes('The completed streamed reply.')),
      'the completed reply replaces its stream before waiting input, keeping transcript order');
    assert.deepEqual(f.errors, []);
  } finally { await f.context.close(); }
}

function queued() {
  return ['First', 'Second'].map((name, n) => ({ command_id: n ? 'later-two' : 'later-one',
    text: `${name} later waiting message`, created_at: n + 1, rejected: null, with_agent: false }));
}

async function sameComposer(composer, node, text, caret) {
  assert(await composer.evaluate((el, kept) => el === kept, node), 'reuse the existing Composer textarea, never a second editor');
  assert.equal(await composer.inputValue(), text);
  if (caret !== undefined) assert.deepEqual(await composer.evaluate(el => ({
    focused: document.activeElement === el, start: el.selectionStart, end: el.selectionEnd,
  })), { focused: true, start: caret, end: caret }, 'natural focus and collapsed caret use UTF-16 after the restored block, before the existing draft');
}

async function actionTarget(button, phone, icon = true) {
  await button.scrollIntoViewIfNeeded();
  const geometry = await button.evaluate(el => {
    const r = el.getBoundingClientRect(), after = getComputedStyle(el, '::after');
    const insets = [after.top, after.right, after.bottom, after.left].map(parseFloat);
    const expanded = after.content !== 'none' && after.content !== 'normal' && after.position === 'absolute';
    const [top, right, bottom, left] = expanded ? insets : [0, 0, 0, 0];
    const target = { left: r.left + left, right: r.right - right, top: r.top + top, bottom: r.bottom - bottom };
    const x = (target.left + target.right) / 2, y = (target.top + target.bottom) / 2;
    const edges = [[target.left + 1, y], [target.right - 1, y], [x, target.top + 1], [x, target.bottom - 1]];
    let visible = true;
    for (let node = el; node; node = node.parentElement) {
      const css = getComputedStyle(node);
      if (css.visibility === 'hidden' || Number(css.opacity) === 0) visible = false;
    }
    return { width: r.width, height: r.height, expanded, insets, visible,
      targetWidth: target.right - target.left, targetHeight: target.bottom - target.top,
      edgesHit: edges.every(([x, y]) => x >= 0 && x < innerWidth && y >= 0 && y < innerHeight &&
        el.contains(document.elementFromPoint(x, y))) };
  });
  assert(geometry.visible, 'action stays visible without hover');
  assert(Math.abs(geometry.height - 28) <= 1 && (!icon || Math.abs(geometry.width - 28) <= 1), 'visible button remains 28px, not an enlarged painted target');
  if (phone) {
    assert.equal(geometry.expanded, true, 'coarse pointer uses a transparent absolute ::after hit area');
    const expected = icon ? [-8, -8, -8, -8] : [-8, 0, -8, 0];
    assert(geometry.insets.every((n, i) => Math.abs(n - expected[i]) <= .5), 'icon expands all edges; Send now expands vertically only');
    assert(Math.abs(geometry.targetHeight - 44) <= 1 && Math.abs(geometry.targetWidth - (icon ? 44 : geometry.width)) <= 1);
  } else assert.equal(geometry.expanded, false, 'desktop does not acquire coarse-pointer hit expansion');
  assert(geometry.edgesHit, 'all four target edges are on-screen and hit the actual button, including outside its visible bounds on touch');
}

async function readActions(row, phone) {
  assert.equal(await row.getByRole('button', { name: 'Edit', exact: true }).count(), 0, 'read rows have no Edit');
  assert.equal(await row.getByRole('button', { name: 'Message actions', exact: true }).count(), 0, 'user actions are direct even on phones');
  for (const name of ['Rewind', 'Ask']) {
    const button = row.getByRole('button', { name, exact: true });
    assert.equal(await button.isEnabled(), true);
    assert.equal(await button.getAttribute('title'), name);
    assert.equal(await button.locator('svg').count(), 1);
    assert.equal((await button.textContent()).trim(), '', `${name} is icon-only`);
    await actionTarget(button, phone);
  }
}

async function checkRewind(browser, origin, phone, outcome) {
  let progress, socket, firstRewindRequest, sequence = 100, connected = deferred();
  const release = deferred(), firstStarted = deferred(), calls = [], mutations = [];
  const replaying = outcome === 'rewind-replay';
  const early = ['rewind-early', 'rewind-reset-error', 'rewind-api-error'].includes(outcome);
  const files = outcome === 'rewind-attachments' ? [upload] : [];
  let attempts = 0;
  const originalEvent = { seq: 72, ts: 72, type: 'user', command_id: 'original', text: ownText,
    ...(files.length ? { attachments: files.map(a => ({ name: a.name, mime: a.mime, size: Buffer.from(a.data, 'base64').length })) } : {}) };
  const f = await fixture(browser, origin, phone ? { width: 390, height: 844 } : { width: 1440, height: 1000 },
    'dark', { choice: 'off', effective: 'off', os: 'reduce' }, p => { progress = p; }, {}, {
      instrumentation: false, events: [originalEvent],
      setup: async ({ page, context }) => {
        page.on('request', request => {
          const path = new URL(request.url()).pathname;
          if (/^(POST|PUT|DELETE)$/.test(request.method()) && path.startsWith('/api/v1/sessions/parent/') &&
              !/\/(seen|viewed|catchup|view)$/.test(path)) mutations.push({ method: request.method(), path });
        });
        // Explicitly return an AttachmentUpload ARRAY even for no attachments;
        // the motion fixture's generic fallback is not the recovery contract.
        await page.route('**/api/v1/sessions/parent/messages/*/draft', route => route.fulfill({ json: files }));
        await page.route('**/api/v1/sessions/parent/attachments/72/0', route => route.fulfill({
          contentType: upload.mime, body: Buffer.from(upload.data, 'base64'),
        }));
        await page.route('**/api/v1/sessions/parent/messages/*/take-back', route => {
          calls.push('unexpected-take-back');
          return route.fulfill({ status: 409, json: { error: 'The agent has this message' } });
        });
        await page.route('**/api/v1/sessions/parent/messages', route => {
          calls.push({ send: route.request().postDataJSON() });
          return route.fulfill({ json: { command_id: 'rewound-send', text: route.request().postDataJSON().text, created_at: 1, rejected: null } });
        });
        await page.route('**/api/v1/sessions/parent/rewind', async route => {
          calls.push({ rewind: route.request().postDataJSON() });
          const attempt = ++attempts;
          if (attempt === 1) { firstRewindRequest = route.request(); firstStarted.resolve(); if (early) await release.promise; }
          if (attempt === 1 && outcome === 'rewind-api-error') return route.fulfill({ status: 503, json: { error: 'Fixture rewind unavailable' } });
          return route.fulfill({ status: outcome === 'rewind' ? 204 : 202 });
        });
        if (replaying) await context.routeWebSocket(/\/sessions\/parent\/stream(?:\?|$)/, stream => {
          const first = !socket;
          socket = stream;
          if (first) {
            stream.send(JSON.stringify({ type: 'event', event: { seq: 1, ts: 1, type: 'started', connection_id: 'test', model: 'test', cwd: '/tmp', system_prompt: '', tools: [], context_window: 128000 } }));
            stream.send(JSON.stringify({ type: 'event', event: originalEvent }));
            stream.send(JSON.stringify({ type: 'synced' }));
          } else connected.resolve();
        });
      },
    }).catch(async error => { await progress?.context.close(); throw error; });
  try {
    const { page } = f;
    const send = event => {
      if (!replaying) return f.send(event);
      const seq = sequence++;
      socket.send(JSON.stringify({ type: 'event', event: { seq, ts: seq, ...event } }));
      return seq;
    };
    const barrier = async label => {
      const seq = send({ type: 'assistant', turn: 74, blocks: [{ type: 'text', text: label }], stop_reason: 'end_turn' });
      await page.locator(`#parent-e${seq}`).waitFor();
    };
    const disconnect = async () => {
      connected = deferred();
      socket.close();
      await page.getByText('Session stream disconnected. Reconnecting…', { exact: true }).waitFor();
      await connected.promise;
    };
    const composer = page.locator('textarea').first(), node = await composer.elementHandle();
    await composer.fill(existingDraft);
    const original = page.locator('#parent-e72');
    await original.scrollIntoViewIfNeeded();
    const originalNode = await original.elementHandle();
    await readActions(original, phone);
    // Rewind is intentionally removed when a durable marker makes this row
    // historical. Ask remains mounted: use its SVG as the no-reparse sentinel.
    const originalIcon = await original.getByRole('button', { name: 'Ask', exact: true }).locator('svg').elementHandle();
    await original.getByRole('button', { name: 'Rewind', exact: true }).click();
    const warning = page.getByText(warningText, { exact: true }), panel = warning.locator('..');
    await warning.waitFor();
    assert.equal(await panel.getByRole('checkbox').isChecked(), false, 'file restore is opt-in');
    assert.deepEqual(calls, [], 'opening explicit confirmation changes no server state');
    assert.equal(await composer.inputValue(), existingDraft, 'confirmation alone does not insert text');
    await panel.getByRole('button', { name: 'Cancel', exact: true }).click();
    await warning.waitFor({ state: 'hidden' });
    assert.equal(await composer.inputValue(), existingDraft, 'Cancel preserves the draft');
    if (outcome === 'rewind-cancel') { assert.deepEqual(mutations, []); assert.deepEqual(f.errors, []); return; }
    await original.getByRole('button', { name: 'Rewind', exact: true }).click();
    await warning.waitFor();
    const stale = send({ type: 'rewound', to_seq: 72, restored_files: false, restore_error: '' });
    await page.locator(`#parent-e${stale}`).waitFor();
    assert(await original.locator('button[aria-label="Ask"] svg').evaluate((el, kept) => el === kept, originalIcon),
      'processing a durable marker retains the original always-mounted SVG');
    if (replaying) {
      await disconnect();
      assert.equal(await panel.getByRole('button', { name: 'Rewind', exact: true }).isDisabled(), true, 'unsynced stream cannot rewind');
      send({ type: 'rewound', to_seq: 72, restored_files: false, restore_error: '' });
      socket.send(JSON.stringify({ type: 'synced' }));
      await barrier('Pre-click replay processed');
      assert.equal(await composer.inputValue(), existingDraft, 'replay before confirmation cannot insert text');
    }
    f.phase = 'explicit rewind request and gated worker confirmation';
    const response = page.waitForResponse(r => r.request() === firstRewindRequest);
    await panel.getByRole('button', { name: 'Rewind', exact: true }).click();
    await firstStarted.promise;
    if (early) {
      send({ type: 'rewound', to_seq: 72, restored_files: false, restore_error: '' });
      await barrier('Worker result arrived before HTTP');
      assert.equal(await composer.inputValue(), existingDraft, 'worker result before HTTP acknowledgment cannot insert yet');
      assert.equal(await warning.isVisible(), true);
    }
    if (outcome === 'rewind-reset-error') {
      send({ type: 'error', message: resetError });
      await panel.getByText(/Claude Code history reset failed; input is blocked/).waitFor();
      assert.equal(await composer.inputValue(), existingDraft, 'worker refusal leaves draft unchanged');
      // Retry while the prior HTTP is held: its later acknowledgment must not
      // overwrite the new attempt's highwater/pending state.
      const retry = page.waitForResponse(r => r.url().endsWith('/parent/rewind'));
      await panel.getByRole('button', { name: 'Rewind', exact: true }).click();
      await retry;
    }
    release.resolve();
    assert.equal((await response).status(), outcome === 'rewind-api-error' ? 503 : outcome === 'rewind' ? 204 : 202);
    if (outcome === 'rewind-api-error') {
      await panel.getByText(/Fixture rewind unavailable/).waitFor();
      assert.equal(await composer.inputValue(), existingDraft, 'HTTP refusal ignores even an early worker marker');
      const retry = page.waitForResponse(r => r.url().endsWith('/parent/rewind'));
      await panel.getByRole('button', { name: 'Rewind', exact: true }).click();
      await retry;
    }
    if (replaying) {
      await disconnect();
      await panel.getByText(/Rewind was not confirmed.*Your draft is kept; nothing was resent/).waitFor();
      assert.equal(await panel.getByRole('button', { name: 'Rewind', exact: true }).isDisabled(), true);
      send({ type: 'rewound', to_seq: 72, restored_files: false, restore_error: '' });
      socket.send(JSON.stringify({ type: 'synced' }));
      await barrier('Post-disconnect replay processed');
      assert.equal(await composer.inputValue(), existingDraft, 'reconnect replay cannot confirm the interrupted attempt');
      assert.equal(attempts, 1, 'reconnect never retries implicitly');
      const retry = page.waitForResponse(r => r.url().endsWith('/parent/rewind'));
      await panel.getByRole('button', { name: 'Rewind', exact: true }).click();
      await retry;
    }
    if (outcome !== 'rewind-early') {
      await settle(page);
      assert.equal(await warning.isVisible(), true, 'HTTP enqueue alone cannot confirm rewind');
      assert.equal(await composer.inputValue(), existingDraft);
      send({ type: 'rewound', to_seq: 71, restored_files: false, restore_error: '' });
      send({ seq: stale, type: 'rewound', to_seq: 72, restored_files: false, restore_error: '' });
      await barrier('Mismatched and stale worker results processed');
      assert.equal(await composer.inputValue(), existingDraft, 'only a matching durable event above click highwater can restore text');
      assert.equal(await warning.isVisible(), true);
      send({ type: 'rewound', to_seq: 72, restored_files: false, restore_error: '' });
    }
    await warning.waitFor({ state: 'hidden' });
    await sameComposer(composer, node, `${ownText}\n\n${existingDraft}`, ownText.length);
    assert(await original.evaluate((el, kept) => el === kept, originalNode), 'rewind retains the original transcript row');
    assert(await original.locator('button[aria-label="Ask"] svg').evaluate((el, kept) => el === kept, originalIcon),
      'rewind does not reparse the original always-mounted SVG');
    assert.equal(calls.filter(c => c.send).length, 0, 'confirmed rewind does not implicitly send');
    assert.equal(await page.locator('[data-message-edit]').count(), 0, 'no obsolete second editor');
    assert.deepEqual(calls.filter(c => c.rewind), Array.from({ length: ['rewind-reset-error', 'rewind-api-error', 'rewind-replay'].includes(outcome) ? 2 : 1 },
      () => ({ rewind: { to_seq: 72, restore_files: false } })));
    if (files.length) await composerBox(composer).getByText(upload.name, { exact: true }).waitFor();
    assert(!calls.includes('unexpected-take-back'), 'durable user Rewind never calls queue withdrawal');
    assert(mutations.every(m => m.path === '/api/v1/sessions/parent/rewind'), 'no tail pull, correction send, interrupt or side-chat mutation');
    assert.deepEqual(f.errors, []);
  } catch (error) {
    console.error('REWIND DIAGNOSTIC', JSON.stringify({ phase: f.phase, calls, mutations, errors: f.errors,
      console: f.consoleMessages, requests: f.requests.slice(-12) }));
    throw error;
  } finally { release.resolve(); await f.context.close(); }
}

async function checkRejected(browser, origin, phone) {
  let progress, dismissed = 0;
  const rows = [
    ...queued(),
    { ...queued()[0], command_id: 'with-agent', with_agent: true },
    { command_id: 'rejected-own', text: `Recover my original message ${'unbroken'.repeat(40)}`,
      rejected: `Delivery unavailable. ${'A long readable explanation. '.repeat(12)}${'unbroken'.repeat(40)}`, with_agent: false, created_at: 1 },
    { command_id: 'rejected-report', text: 'Subagent report delivery failed…',
      rejected: 'The parent is unavailable.', with_agent: false, created_at: 2 },
  ];
  const f = await fixture(browser, origin, phone ? { width: 320, height: 568 } : { width: 1440, height: 1000 },
    'dark', { choice: 'off', effective: 'off', os: 'reduce' }, p => { progress = p; }, {}, {
      instrumentation: false,
      setup: async ({ page }) => {
        await page.route('**/api/v1/sessions/parent/messages/*/draft', route => route.fulfill({ json: [] }));
        await page.route('**/api/v1/sessions/parent/queue/rejected', async route => {
          assert.equal(route.request().method(), 'DELETE');
          dismissed++;
          f.stream({ type: 'queue', queued: rows.filter(row => !row.rejected) });
          await route.fulfill({ status: 204 });
        });
      },
    }).catch(async error => { await progress?.context.close(); throw error; });
  try {
    const { page } = f;
    f.stream({ type: 'queue', queued: rows });
    const rejected = page.locator('[data-rejected-command="rejected-own"]');
    await rejected.waitFor();
    await settle(page);
    const waiting = page.locator('[data-waiting-messages]');
    assert.equal(await waiting.locator('[data-rejected-command]').count(), 0);
    assert.equal(await waiting.locator('[data-queued-command]').count(), 3, 'waiting and with-agent rows remain at the transcript end');
    await waitingInFlow(waiting);
    assert.equal(await rejected.getByText(rows[3].text, { exact: true }).count(), 1, 'original input remains recoverable');
    assert.deepEqual(await rejected.getByRole('button').allTextContents(), ['Dismiss'], 'rejections are not queue work');
    assert(await rejected.evaluate(el => {
      const durable = document.querySelectorAll('.c6-transcript-row');
      return durable.length && !!(durable[durable.length - 1].compareDocumentPosition(el) & Node.DOCUMENT_POSITION_FOLLOWING);
    }), 'rejection follows the durable transcript rows');
    assert(await rejected.evaluate(el => {
      for (let p = el.parentElement; p; p = p.parentElement) {
        if (p.scrollHeight > p.clientHeight && getComputedStyle(p).overflowY === 'auto') return true;
      }
      return false;
    }), 'rejection is inside the scrolling transcript');
    const documentTop = () => rejected.evaluate(el => {
      let p = el.parentElement;
      while (p && !(p.scrollHeight > p.clientHeight && getComputedStyle(p).overflowY === 'auto')) p = p.parentElement;
      return el.getBoundingClientRect().top - p.getBoundingClientRect().top + p.scrollTop;
    });
    const before = await documentTop();
    f.stream({ type: 'text', turn: 100, seq: 9999, text: 'Growing streamed reply.\n\n'.repeat(100) });
    const stream = page.locator('.md').filter({ hasText: 'Growing streamed reply.' }).last();
    await stream.waitFor();
    assert(await rejected.evaluate((el, reply) => !!(el.compareDocumentPosition(reply) & Node.DOCUMENT_POSITION_FOLLOWING),
      await stream.elementHandle()), 'rejection precedes the growing streaming reply');
    await settle(page);
    assert(Math.abs(await documentTop() - before) <= 1, 'streaming after the rejection cannot move its transcript position');
    for (const width of phone ? [320, 390] : [1440, 800]) {
      await page.setViewportSize({ width, height: phone ? 568 : 1000 });
      const dismiss = rejected.getByRole('button', { name: 'Dismiss', exact: true });
      await dismiss.scrollIntoViewIfNeeded();
      assert(await rejected.evaluate(el => [el, ...el.querySelectorAll('*')].every(node => node.scrollWidth <= node.clientWidth + 1)),
        `reason and original text wrap without horizontal scrolling at ${width}px`);
      assert(await dismiss.evaluate(el => {
        const r = el.getBoundingClientRect();
        return r.left >= 0 && r.right <= innerWidth && r.top >= 0 && r.bottom <= innerHeight &&
          el.contains(document.elementFromPoint(r.x + r.width / 2, r.y + r.height / 2));
      }), 'Dismiss stays on screen and hit-testable');
      const bounds = await dismiss.boundingBox();
      assert(phone ? bounds.height >= 44 && bounds.width >= 44 : bounds.height < 44, 'only touch gets the 44px target');
      assert(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), 'no page overflow');
    }
    await rejected.getByRole('button', { name: 'Dismiss', exact: true }).click();
    await rejected.waitFor({ state: 'hidden' });
    assert.equal(await page.locator('[data-rejected-command]').count(), 0, 'Dismiss clears all rejected inputs');
    assert.equal(dismissed, 1);
    assert.equal(await waiting.locator('[data-queued-command]').count(), 3, 'Dismiss leaves waiting work intact');
    assert.deepEqual(f.errors, []);
  } finally { await f.context.close(); }
}


async function checkQueued(browser, origin, phone, outcome) {
  let progress;
  const release = deferred(), recoveryRelease = deferred(), started = deferred(), recoveryStarted = deferred();
  const secondRelease = deferred(), secondStarted = deferred();
  const calls = { takeBack: [], draft: [], send: [], now: [] }, mutations = [], feedReads = [];
  const target = { command_id: 'queued-single', text: ownText, created_at: 1, rejected: null, with_agent: false };
  const second = { ...target, command_id: 'queued-second', text: 'Second returned message 🪁', created_at: 2 };
  const withAgent = { ...target, command_id: 'with-agent', text: 'Already handed to worker', with_agent: true };
  const attachments = outcome === 'queued-attachments';
  const failed = outcome.startsWith('queued-failure') || outcome === 'queued-stacking-failure';
  const edited = ['queued-failure-edited', 'queued-failure-reverted'].includes(outcome);
  const stacking = outcome.startsWith('queued-stacking');
  const taken = outcome === 'queued-taken-race';
  const responseLost = outcome === 'queued-response-lost';
  const base = outcome === 'queued-empty' ? '' : existingDraft;
  const tail = outcome === 'queued-layout' ? Array.from({ length: 15 }, (_, i) => ({ ...target,
    command_id: `waiting-${i}`, text: `Waiting ${i} ${'longword'.repeat(24)}` })) : queued();
  const rows = [target, ...(stacking ? [second] : []), ...tail, withAgent];
  const f = await fixture(browser, origin, phone ? { width: 390, height: 844 } : { width: 1440, height: 1000 },
    'dark', { choice: 'off', effective: 'off', os: 'reduce' }, p => { progress = p; }, {}, {
      instrumentation: false,
      setup: async ({ page, context }) => {
        page.on('request', request => {
          const path = new URL(request.url()).pathname;
          if (request.method() === 'POST' && path === '/api/v1/memex/rpc') { feedReads.push(request.postDataJSON()); return; }
          if (/^(POST|PUT|DELETE)$/.test(request.method()) && path.startsWith('/api/') &&
              !/^\/api\/v1\/sessions\/parent\/(seen|viewed|catchup|view)$/.test(path)) mutations.push({ method: request.method(), path });
        });
        await context.routeWebSocket(/\/live(?:\?|$)/, socket => {
          if (new URL(socket.url()).host !== new URL(origin).host) {
            progress.errors.push(`Blocked external WebSocket: ${socket.url()}`); socket.close(); return;
          }
          socket.send(JSON.stringify({ type: 'sessions', sessions: [session()] }));
          socket.send(JSON.stringify({ type: 'workers', workers: [{
            id: session().worker_id, name: 'Return-to-box fixture', hostname: 'fixture', os: 'linux', arch: 'x86_64',
            version: 'test', status: 'online', enrolled_at: 1, last_seen: 1, connected_since: 1, rtt_us: 0,
            capabilities: ['queue_ops', 'send_now'],
          }] }));
          socket.send(JSON.stringify({ type: 'marks', marks: {} }));
        });
        await page.route('**/api/v1/sessions/parent/messages/*/draft', async route => {
          const cid = new URL(route.request().url()).pathname.split('/').at(-2);
          assert.equal(route.request().method(), 'GET');
          calls.draft.push(cid);
          if (cid === target.command_id) {
            recoveryStarted.resolve();
            if (attachments) await recoveryRelease.promise;
          }
          await route.fulfill({ json: attachments && cid === target.command_id ? [upload] : [] });
        });
        await page.route('**/api/v1/sessions/parent/messages/*/take-back', async route => {
          assert.equal(route.request().method(), 'POST');
          const cid = new URL(route.request().url()).pathname.split('/').at(-2);
          calls.takeBack.push(cid);
          if (cid === second.command_id) { secondStarted.resolve(); await secondRelease.promise; }
          else { assert.equal(cid, target.command_id); started.resolve(); await release.promise; }
          if (responseLost) return route.abort('failed');
          if (taken) return route.fulfill({ status: 409, json: { error: 'The agent has this message' } });
          if (failed && cid === target.command_id &&
              (outcome !== 'queued-failure-retry' || calls.takeBack.filter(id => id === cid).length === 1)) {
            return route.fulfill({ status: 503, json: { error: 'Fixture cancellation unavailable' } });
          }
          return route.fulfill({ status: 204 });
        });
        await page.route('**/api/v1/sessions/parent/messages', route => {
          const body = route.request().postDataJSON();
          calls.send.push(body);
          if (outcome === 'queued-send-error' && calls.send.length === 1) return route.fulfill({ status: 503, json: { error: 'Fixture send unavailable' } });
          return route.fulfill({ json: { command_id: 'appended-message', text: body.text, created_at: 99, rejected: null, with_agent: false } });
        });
        await page.route('**/api/v1/sessions/parent/queue/*/now', route => {
          assert.equal(route.request().method(), 'POST');
          calls.now.push(new URL(route.request().url()).pathname);
          return route.fulfill({ status: 204 });
        });
      },
    }).catch(async error => { await progress?.context.close(); throw error; });
  try {
    const { page } = f;
    const composer = page.locator('textarea').first(), node = await composer.elementHandle();
    await composer.fill(base);
    if (attachments) {
      await composerBox(composer).locator('input[type="file"]').setInputFiles({
        name: draftUpload.name, mimeType: draftUpload.mime, buffer: Buffer.from(draftUpload.data, 'base64'),
      });
      await composerBox(composer).getByText(draftUpload.name, { exact: true }).waitFor();
    }
    f.stream({ type: 'queue', queued: rows });
    const row = queueRow(page, target.command_id), waiting = page.locator('[data-waiting-messages]');
    await row.waitFor();
    for (const q of rows) {
      const actions = queueRow(page, q.command_id).locator('[data-queue-actions]');
      assert.equal(await actions.getByRole('button').count(), q.with_agent ? 2 : 1, 'only × plus optional Send now, never bulk editing');
      assert.equal(await actions.getByRole('button', { name: 'Take this back', exact: true }).getAttribute('title'), 'Take this back');
      assert.equal(await actions.getByRole('button', { name: 'Send now', exact: true }).count(), q.with_agent ? 1 : 0);
      assert.equal(await actions.getByRole('button', { name: /Edit/ }).count(), 0);
    }
    for (const width of phone ? [320, 390] : [800, 1440]) {
      await page.setViewportSize({ width, height: phone ? 844 : 1000 });
      const button = row.getByRole('button', { name: 'Take this back', exact: true });
      await actionTarget(button, phone);
      const bounds = await row.boundingBox(), actionBounds = await row.locator('[data-queue-actions]').boundingBox();
      assert(actionBounds.x >= bounds.x - 1 && actionBounds.x + actionBounds.width <= bounds.x + bounds.width + 1, 'actions stay right-aligned inside their row');
      assert(Math.abs(actionBounds.x + actionBounds.width - bounds.x - bounds.width) <= 1, 'action row aligns to the waiting bubble edge');
      // The transparent touch target extends 8px into the reading gutter.
      // Without the old padded inner scroller that counts in the group's
      // scrollWidth, but is not overflowing text or a horizontal scroller.
      assert(await waiting.evaluate(el => [...el.querySelectorAll('.whitespace-pre-wrap')]
        .every(text => text.scrollWidth <= text.clientWidth + 1)), 'every waiting message wraps without horizontal text overflow');
      assert(await page.evaluate(() => document.documentElement.scrollWidth <= innerWidth), 'no page overflow');
    }
    await actionTarget(queueRow(page, withAgent.command_id).getByRole('button', { name: 'Send now', exact: true }), phone, false);
    await waitingInFlow(waiting);
    f.stream({ type: 'text', turn: 100, seq: 9999, text: 'Growing streamed reply.\n\n'.repeat(100) });
    await page.locator('.md').filter({ hasText: 'Growing streamed reply.' }).last().waitFor();
    await settle(page);
    await waitingAtEnd(page);
    assert(await waiting.evaluate(el => el.previousElementSibling?.textContent.includes('Growing streamed reply.')),
      'waiting rows follow the streaming reply in transcript order');
    if (outcome === 'queued-send-now') {
      const response = page.waitForResponse(r => r.url().endsWith('/queue/with-agent/now'));
      await queueRow(page, withAgent.command_id).getByRole('button', { name: 'Send now', exact: true }).click();
      assert.equal((await response).status(), 204);
      assert.deepEqual(calls.now, ['/api/v1/sessions/parent/queue/with-agent/now']);
      assert.deepEqual(calls.takeBack, []);
      assert.equal(await composer.inputValue(), base);
      assert.deepEqual(mutations, [{ method: 'POST', path: calls.now[0] }]);
      assert.deepEqual(f.errors, []);
      return;
    }
    if (attachments) await recoveryStarted.promise;
    // Observe the same trusted gesture after WASM's delegated click handler.
    // This captures focus/value/caret before ANY cancellation HTTP can finish.
    await page.evaluate(() => {
      window.__returnClicks = [];
      document.addEventListener('click', event => {
        if (!event.target.closest('[data-queue-actions] button[aria-label="Take this back"]')) return;
        const el = document.querySelector('textarea');
        __returnClicks.push({ text: el.value, focus: document.activeElement === el, start: el.selectionStart, end: el.selectionEnd });
      });
    });
    f.phase = 'trusted × inserts immediately while take-back is gated';
    await row.getByRole('button', { name: 'Take this back', exact: true }).click();
    await row.waitFor({ state: 'hidden' });
    const combined = base ? `${ownText}\n\n${base}` : ownText;
    await sameComposer(composer, node, combined, ownText.length);
    assert.deepEqual(await page.evaluate(() => __returnClicks[0]), { text: combined, focus: true, start: ownText.length, end: ownText.length }, 'synchronous trusted gesture value/focus/UTF-16 caret');
    assert.equal(calls.send.length, 0, 'taking back never sends');
    if (attachments) {
      assert.deepEqual(calls.takeBack, [], 'recover attachment bytes before POST, not after losing them');
      recoveryRelease.resolve();
      await composerBox(composer).getByText(upload.name, { exact: true }).waitFor();
    }
    await started.promise;
    assert.deepEqual(calls.takeBack, [target.command_id], 'only selected command is cancelled, not its tail');
    if (stacking) {
      await queueRow(page, second.command_id).getByRole('button', { name: 'Take this back', exact: true }).click();
      await secondStarted.promise;
      await queueRow(page, second.command_id).waitFor({ state: 'hidden' });
      const restored = `${ownText}\n\n${second.text}`;
      await sameComposer(composer, node, `${restored}\n\n${base}`, restored.length);
      const secondResponse = page.waitForResponse(r => r.url().endsWith(`/messages/${second.command_id}/take-back`));
      secondRelease.resolve();
      await secondResponse;
      assert.equal(await composer.inputValue(), `${restored}\n\n${base}`, 'reverse HTTP completion preserves click order');
    }
    if (edited) {
      await composer.fill(`${combined}\nUser edited this while cancellation was pending`);
      if (outcome === 'queued-failure-reverted') await composer.fill(combined);
    }
    const saved = await composer.inputValue();
    const response = responseLost
      ? page.waitForEvent('requestfailed', { predicate: request => request.method() === 'POST' &&
        new URL(request.url()).pathname === `/api/v1/sessions/parent/messages/${target.command_id}/take-back` })
      : page.waitForResponse(r => r.url().endsWith(`/messages/${target.command_id}/take-back`));
    release.resolve();
    const result = await response;
    if (responseLost) {
      assert(result.failure()?.errorText, 'observe only the intentionally aborted take-back request, not an HTTP refusal');
      await row.waitFor();
      await page.getByText(/couldn[’']t (?:be taken|take (?:this|it|the message)) back/i).waitFor();
      await sameComposer(composer, node, combined);
      assert.deepEqual(calls.takeBack, [target.command_id], 'response loss never retries implicitly');
      assert.equal(calls.send.length, 0, 'an ambiguous cancellation never sends the restored text');
      for (const later of [...tail, withAgent]) assert.equal(await queueRow(page, later.command_id).count(), 1,
        'response loss itself does not mutate later inputs');
      // The head may actually have cancelled the command before its response
      // was lost. An authoritative empty queue cannot erase the only draft.
      f.stream({ type: 'queue', queued: [] });
      const barrier = f.send({ type: 'assistant', turn: 100, blocks: [{ type: 'text', text: 'Empty queue after response loss processed' }], stop_reason: 'end_turn' });
      await page.locator(`#parent-e${barrier}`).waitFor();
      await sameComposer(composer, node, combined);
      assert.equal(calls.send.length, 0, 'empty server queue is not permission to send or roll back an untouched draft');
    } else if (failed) {
      assert.equal(result.status(), 503);
      await row.waitFor();
      await page.getByText(/couldn[’']t (?:be taken|take (?:this|it|the message)) back/i).waitFor();
      const rolledBack = stacking ? `${second.text}\n\n${base}` : base;
      assert.equal(await composer.inputValue(), edited ? saved : rolledBack, 'rollback only its inserted block, only if the user never edited (even edit-and-revert)');
      assert.equal(calls.send.length, 0);
      if (outcome === 'queued-failure-retry') {
        const retry = page.waitForResponse(r => r.url().endsWith(`/messages/${target.command_id}/take-back`));
        await row.getByRole('button', { name: 'Take this back', exact: true }).click();
        assert.equal((await retry).status(), 204);
        await row.waitFor({ state: 'hidden' });
        await sameComposer(composer, node, combined, ownText.length);
        assert.deepEqual(calls.takeBack, [target.command_id, target.command_id], 'only the user explicitly retries; insert original once');
        assert.equal(calls.send.length, 0);
      }
    } else if (taken) {
      assert.equal(result.status(), 409);
      await sameComposer(composer, node, combined);
      const seq = f.send({ type: 'user', command_id: target.command_id, text: ownText, origin: { kind: 'human' } });
      const durable = page.locator(`#parent-e${seq}`);
      await durable.waitFor();
      await page.getByText(tooLateNote, { exact: true }).waitFor();
      assert.equal(await durable.getByText(ownText, { exact: true }).count(), 1, 'too-late delivery is an ordinary durable User, not a pretend withdrawn row');
      await readActions(durable, phone);
      assert.equal(await composer.inputValue(), combined, 'durable delivery never takes text back out of the box');
    } else {
      assert.equal(result.status(), 204);
      f.stream({ type: 'queue', queued: rows });
      const barrier = f.send({ type: 'assistant', turn: 100, blocks: [{ type: 'text', text: 'Stale queue snapshot processed' }], stop_reason: 'end_turn' });
      await page.locator(`#parent-e${barrier}`).waitFor();
      await row.waitFor({ state: 'hidden' });
      if (stacking) assert.equal(await queueRow(page, second.command_id).count(), 0, 'stale snapshots cannot resurrect either successful retraction');
      assert.equal(await page.locator('[data-message-edit]').count(), 0, 'only the ordinary Composer exists');
      if (!stacking && outcome !== 'queued-layout') {
        const text = await composer.inputValue();
        const send = async () => {
          const sent = page.waitForResponse(r => r.url().endsWith('/parent/messages'));
          await page.locator('.c6-send-slot').first().getByRole('button', { name: /^Send/ }).click();
          return sent;
        };
        if (outcome === 'queued-send-error') {
          assert.equal((await send()).status(), 503);
          await page.getByText('Fixture send unavailable', { exact: true }).waitFor();
          await sameComposer(composer, node, text);
        }
        assert.equal((await send()).status(), 200, 'only explicit ordinary Send appends a fresh queue message');
        assert.equal(await composer.inputValue(), '');
        assert(calls.send.every(body => body.text === text));
        if (attachments) assert.deepEqual(calls.send.at(-1).attachments, [upload, draftUpload], 'ordinary Send retains recovered then existing attachments with exact bytes/name/MIME');
        f.stream({ type: 'queue', queued: [...tail, withAgent, { command_id: 'appended-message', text, created_at: 99, rejected: null, with_agent: false }] });
        await queueRow(page, 'appended-message').waitFor();
      }
    }
    if (!responseLost) {
      for (const later of tail) assert.equal(await queueRow(page, later.command_id).count(), 1, 'all later queued inputs are untouched');
      assert.equal(await queueRow(page, withAgent.command_id).count(), 1);
    }
    const allowed = new Set([
      `/api/v1/sessions/parent/messages/${target.command_id}/take-back`,
      ...(stacking ? [`/api/v1/sessions/parent/messages/${second.command_id}/take-back`] : []),
      ...(calls.send.length ? ['/api/v1/sessions/parent/messages'] : []),
    ]);
    assert(mutations.every(m => m.method === 'POST' && allowed.has(m.path)), 'no bulk pull/cancel, implicit send, interrupt, rewind or side-chat mutation');
    assert.equal(mutations.filter(m => m.path.endsWith('/messages')).length, calls.send.length);
    for (const body of feedReads) assert.deepEqual(body, { method: 'memex.feeds', params: {} }, 'automatic sidebar feed reads only');
    assert.deepEqual(f.errors, []);
  } catch (error) {
    console.error('RETURN TO BOX DIAGNOSTIC', JSON.stringify({ phase: f.phase, calls, mutations, feedReads, errors: f.errors,
      console: f.consoleMessages, requests: f.requests.slice(-12) }));
    throw error;
  } finally { release.resolve(); recoveryRelease.resolve(); secondRelease.resolve(); await f.context.close(); }
}

// Separate gates reproduce draft ownership changes while recovery/Send is in
// flight, not just different completion orders for an untouched returned batch.
async function checkQueuedRecoveryRace(browser, origin, phone, outcome) {
  let progress;
  const editing = outcome === 'queued-attachments-edited-stacking';
  const recoveryStarted = deferred(), recoveryRelease = deferred();
  const aStarted = deferred(), aRelease = deferred(), bStarted = deferred(), bRelease = deferred();
  const sendStarted = deferred(), sendRelease = deferred();
  const calls = { draft: [], takeBack: [], send: [] }, mutations = [], feedReads = [];
  const a = { command_id: 'race-a', text: ownText, created_at: 1, rejected: null, with_agent: false };
  const b = { ...a, command_id: 'race-b', text: 'Second returned message 🪁', created_at: 2 };
  const bUpload = { name: 'recovered-second.txt', mime: 'text/plain', data: Buffer.from('second attachment bytes 🪁').toString('base64') };
  const rows = [a, ...(editing ? [b] : []), ...queued()];
  const f = await fixture(browser, origin, phone ? { width: 390, height: 844 } : { width: 1440, height: 1000 },
    'dark', { choice: 'off', effective: 'off', os: 'reduce' }, p => { progress = p; }, {}, {
      instrumentation: false,
      setup: async ({ page }) => {
        page.on('request', request => {
          const path = new URL(request.url()).pathname;
          if (request.method() === 'POST' && path === '/api/v1/memex/rpc') { feedReads.push(request.postDataJSON()); return; }
          if (/^(POST|PUT|DELETE)$/.test(request.method()) && path.startsWith('/api/') &&
              !/^\/api\/v1\/sessions\/parent\/(seen|viewed|catchup|view)$/.test(path)) {
            mutations.push({ method: request.method(), path });
          }
        });
        await page.route('**/api/v1/sessions/parent/messages/*/draft', async route => {
          assert.equal(route.request().method(), 'GET');
          const cid = new URL(route.request().url()).pathname.split('/').at(-2);
          calls.draft.push(cid);
          if (cid === a.command_id) {
            recoveryStarted.resolve();
            if (editing) await recoveryRelease.promise;
          }
          await route.fulfill({ json: cid === a.command_id ? [upload] : cid === b.command_id ? [bUpload] : [] });
        });
        await page.route('**/api/v1/sessions/parent/messages/*/take-back', async route => {
          assert.equal(route.request().method(), 'POST');
          const cid = new URL(route.request().url()).pathname.split('/').at(-2);
          calls.takeBack.push(cid);
          if (cid === a.command_id) { aStarted.resolve(); await aRelease.promise; }
          else { assert.equal(cid, b.command_id); bStarted.resolve(); await bRelease.promise; }
          await route.fulfill({ status: 204 });
        });
        await page.route('**/api/v1/sessions/parent/messages', async route => {
          calls.send.push(route.request().postDataJSON());
          sendStarted.resolve();
          if (!editing) {
            await sendRelease.promise;
            return route.fulfill({ status: 503, json: { error: 'Fixture in-flight Send unavailable' } });
          }
          return route.fulfill({ json: { command_id: 'race-appended', text: calls.send.at(-1).text,
            created_at: 99, rejected: null, with_agent: false } });
        });
      },
    }).catch(async error => { await progress?.context.close(); throw error; });
  try {
    const { page } = f;
    const composer = page.locator('textarea').first(), node = await composer.elementHandle();
    const box = composerBox(composer);
    const filesPresent = async files => {
      for (const file of files) await box.getByText(file.name, { exact: true }).waitFor();
      assert.equal(await box.getByTitle('Remove', { exact: true }).count(), files.length, 'all current attachment chips survive, with no replacements or duplicates');
    };
    await composer.fill(existingDraft);
    await box.locator('input[type="file"]').setInputFiles({ name: draftUpload.name,
      mimeType: draftUpload.mime, buffer: Buffer.from(draftUpload.data, 'base64') });
    await filesPresent([draftUpload]);
    f.stream({ type: 'queue', queued: rows });
    await queueRow(page, a.command_id).waitFor();
    await recoveryStarted.promise;
    let sendResponse;
    if (!editing) {
      f.phase = 'ordinary Send S is pending with its own text and attachment';
      sendResponse = page.waitForResponse(r => r.url().endsWith('/parent/messages'));
      await page.locator('.c6-send-slot').first().getByRole('button', { name: /^Send/ }).click();
      await sendStarted.promise;
      await sameComposer(composer, node, '');
      await filesPresent([]);
      assert.equal(calls.send.length, 1);
      assert.equal(calls.send[0].text, existingDraft);
      assert.deepEqual(calls.send[0].attachments, [draftUpload]);
    }
    f.phase = 'take back A while attachment recovery or ordinary Send is pending';
    await queueRow(page, a.command_id).getByRole('button', { name: 'Take this back', exact: true }).click();
    await queueRow(page, a.command_id).waitFor({ state: 'hidden' });
    const initial = editing ? `${a.text}\n\n${existingDraft}` : a.text;
    await sameComposer(composer, node, initial, a.text.length);
    let expected = initial;
    if (editing) {
      assert.deepEqual(calls.takeBack, [], 'A cannot cancel before recovering its gated bytes');
      const userEdited = `${initial}\nUser edit while A files are pending 🧶`;
      await composer.fill(userEdited);
      await queueRow(page, b.command_id).getByRole('button', { name: 'Take this back', exact: true }).click();
      await bStarted.promise;
      await queueRow(page, b.command_id).waitFor({ state: 'hidden' });
      // The intervening edit makes A part of the base. B is restored ahead of
      // that edited base; A's late files must survive its retired batch ticket.
      expected = `${b.text}\n\n${userEdited}`;
      await sameComposer(composer, node, expected, b.text.length);
      await filesPresent([bUpload, draftUpload]);
      recoveryRelease.resolve();
      await aStarted.promise;
      await filesPresent([bUpload, draftUpload, upload]);
      await sameComposer(composer, node, expected);
      const bResponse = page.waitForResponse(r => r.url().endsWith(`/messages/${b.command_id}/take-back`));
      bRelease.resolve();
      assert.equal((await bResponse).status(), 204);
      await sameComposer(composer, node, expected);
      await filesPresent([bUpload, draftUpload, upload]);
    } else {
      await aStarted.promise;
      await filesPresent([upload]);
    }
    const aResponse = page.waitForResponse(r => r.url().endsWith(`/messages/${a.command_id}/take-back`));
    aRelease.resolve();
    assert.equal((await aResponse).status(), 204);
    await sameComposer(composer, node, expected);
    if (editing) {
      await filesPresent([bUpload, draftUpload, upload]);
      assert.equal(calls.send.length, 0, 'neither recovery nor either cancellation implicitly sends');
      const accepted = page.waitForResponse(r => r.url().endsWith('/parent/messages'));
      await page.locator('.c6-send-slot').first().getByRole('button', { name: /^Send/ }).click();
      assert.equal((await accepted).status(), 200);
      assert.equal(calls.send.length, 1, 'only the explicit final Send submits the combined draft');
      assert.equal(calls.send[0].text, expected);
      assert.deepEqual(calls.send[0].attachments, [bUpload, draftUpload, upload], 'A late recovery retains exact bytes even after edit plus B retires its ticket');
      await sameComposer(composer, node, '');
      await filesPresent([]);
      assert.deepEqual(calls.takeBack, [b.command_id, a.command_id]);
    } else {
      f.phase = 'release S failure after A has been recovered and cancellation confirmed';
      assert.equal(calls.send.length, 1, 'take-back confirmation does not retry the pending Send');
      sendRelease.resolve();
      assert.equal((await sendResponse).status(), 503);
      await page.getByText('Fixture in-flight Send unavailable', { exact: true }).waitFor();
      expected = `${a.text}\n\n${existingDraft}`;
      await sameComposer(composer, node, expected);
      await filesPresent([upload, draftUpload]);
      const barrier = f.send({ type: 'assistant', turn: 100, blocks: [{ type: 'text', text: 'Failed Send recovery processed' }], stop_reason: 'end_turn' });
      await page.locator(`#parent-e${barrier}`).waitFor();
      await sameComposer(composer, node, expected);
      await filesPresent([upload, draftUpload]);
      assert.equal(calls.send.length, 1, 'failed S restores after current A without implicit send or retry');
      assert.deepEqual(calls.takeBack, [a.command_id]);
    }
    for (const later of queued()) assert.equal(await queueRow(page, later.command_id).count(), 1, 'races do not touch later queued work');
    const expectedPaths = [
      ...(editing ? [`/api/v1/sessions/parent/messages/${b.command_id}/take-back`] : ['/api/v1/sessions/parent/messages']),
      `/api/v1/sessions/parent/messages/${a.command_id}/take-back`,
      ...(editing ? ['/api/v1/sessions/parent/messages'] : []),
    ];
    assert.deepEqual(mutations, expectedPaths.map(path => ({ method: 'POST', path })), 'only explicit Send and selected single-command cancellations mutate anything');
    for (const body of feedReads) assert.deepEqual(body, { method: 'memex.feeds', params: {} }, 'automatic sidebar feed reads only');
    assert.deepEqual(f.errors, []);
  } catch (error) {
    console.error('QUEUED RECOVERY RACE DIAGNOSTIC', JSON.stringify({ phase: f.phase, calls, mutations,
      errors: f.errors, console: f.consoleMessages, requests: f.requests.slice(-12) }));
    throw error;
  } finally { recoveryRelease.resolve(); aRelease.resolve(); bRelease.resolve(); sendRelease.resolve(); await f.context.close(); }
}

// Unlike seeded transcript and return-to-box cases, this starts with a
// real composer Send while a turn is still running. Actions belong to the
// durable User event, not the queue receipt, a reply, or a finished turn.
async function checkSentDuringTurn(browser, origin, phone, outcome) {
  const provenance = outcome.slice('sent-during-turn-'.length);
  const text = `Composer message sent during a working turn (${provenance})`;
  const receipt = { command_id: 'composer-during-turn', text, created_at: 1, rejected: null, with_agent: false };
  const calls = { send: [], rewind: [] }, mutations = [];
  let progress;
  const f = await fixture(browser, origin, phone ? { width: 390, height: 844 } : { width: 1440, height: 1000 },
    'dark', { choice: 'off', effective: 'off', os: 'reduce' }, p => { progress = p; }, {}, {
      instrumentation: false,
      setup: async ({ page }) => {
        page.on('request', request => {
          const path = new URL(request.url()).pathname;
          if (/^(POST|PUT|DELETE)$/.test(request.method()) && path.startsWith('/api/v1/sessions/parent/') &&
              !/\/(seen|viewed|catchup)$/.test(path)) mutations.push({ method: request.method(), path });
        });
        await page.route('**/api/v1/sessions/parent/messages/*/draft', route => route.fulfill({ json: [] }));
        await page.route('**/api/v1/sessions/parent/messages', route => {
          calls.send.push(route.request().postDataJSON());
          return route.fulfill({ json: receipt });
        });
        await page.route('**/api/v1/sessions/parent/rewind', route => {
          calls.rewind.push(route.request().postDataJSON());
          return route.fulfill({ status: 202 });
        });
      },
    }).catch(async error => { await progress?.context.close(); throw error; });
  try {
    const { page } = f;
    f.phase = 'composer Send during a running turn';
    await f.setState('working');
    const turn = f.send({ type: 'turn_started', turn: 100 });
    await page.locator(`#parent-e${turn}`).waitFor({ state: 'attached' });
    const slot = page.locator('.c6-send-slot').first();
    await slot.getByRole('button', { name: /^Stop/ }).waitFor();
    const composer = page.locator('textarea').first();
    await composer.fill(text);
    const sent = page.waitForResponse(r => r.url().endsWith('/parent/messages'));
    await slot.getByRole('button', { name: /^Send/ }).click();
    assert.equal((await sent).status(), 200);
    assert.equal(await composer.inputValue(), '', 'real composer Send clears the accepted draft');
    assert.equal(calls.send.length, 1);
    assert.equal(calls.send[0].text, text);
    await slot.getByRole('button', { name: /^Stop/ }).waitFor();
    f.stream({ type: 'queue', queued: [receipt] });
    const waiting = page.locator(`[data-queued-command="${receipt.command_id}"]`);
    await waiting.waitFor();
    assert.equal(await page.locator('.c6-transcript-row').filter({ hasText: text }).count(), 0,
      'HTTP receipt is queued, not a fabricated durable transcript event');
    assert.deepEqual(calls.rewind, []);

    f.phase = 'queue becomes taken, then the durable User arrives without ending the turn';
    f.stream({ type: 'queue', queued: [{ ...receipt, with_agent: true }] });
    await waiting.getByRole('button', { name: 'Send now', exact: true }).waitFor();
    // Unrelated live events make the durable seq differ from the turn and any
    // queue position. Neither is a valid substitute for this message's seq.
    f.send({ type: 'assistant', turn: 100, blocks: [{ type: 'text', text: 'Still working before delivery' }], stop_reason: '' });
    const actualSeq = f.send({ type: 'user', command_id: receipt.command_id, text,
      ...(provenance === 'legacy' ? {} : { origin: { kind: provenance } }) });
    const original = page.locator(`#parent-e${actualSeq}`);
    f.phase = `waiting for durable User ${actualSeq}`;
    await original.waitFor();
    assert.equal(await slot.getByRole('button', { name: /^Stop/ }).isVisible(), true,
      'the agent is still working when the durable User offers actions');
    // Durable User identity alone makes direct icons available, even before
    // another queue snapshot or any assistant reply/turn completion.
    await original.scrollIntoViewIfNeeded();
    await readActions(original, phone);
    const action = async name => {
      f.phase = `${name} on the delivered User while the turn is still running`;
      await original.scrollIntoViewIfNeeded();
      await original.getByRole('button', { name, exact: true }).click();
    };
    if (provenance === 'unknown') await original.getByText('Message · origin unknown', { exact: true }).waitFor();
    assert.deepEqual(calls.rewind, []);
    f.stream({ type: 'queue', queued: [] });
    await waiting.waitFor({ state: 'hidden' });
    await action('Ask');
    const question = page.getByPlaceholder(/^Ask about this in a side chat/);
    await question.waitFor();
    await question.locator('..').getByRole('button', { name: 'Cancel', exact: true }).click();
    await question.waitFor({ state: 'hidden' });

    await composer.fill(existingDraft);
    const composerNode = await composer.elementHandle();
    f.phase = 'explicit direct Rewind targets the actual delivered User sequence';
    await action('Rewind');
    const warning = page.getByText(warningText, { exact: true });
    await warning.waitFor();
    const confirmation = warning.locator('..');
    assert.equal(await confirmation.getByRole('checkbox').isChecked(), false);
    assert.deepEqual(calls.rewind, [], 'Rewind entry still needs a separate confirmation');
    assert.equal(await confirmation.getByRole('button', { name: 'Rewind', exact: true }).isEnabled(), true,
      'a running turn is not a rewind blocker');
    const accepted = page.waitForResponse(r => r.url().endsWith('/parent/rewind'));
    await confirmation.getByRole('button', { name: 'Rewind', exact: true }).click();
    assert.equal((await accepted).status(), 202);
    assert.deepEqual(calls.rewind, [{ to_seq: actualSeq, restore_files: false }],
      'the request uses the durable User seq, not a queue id, index, or turn seq');
    assert.equal(await warning.isVisible(), true, 'HTTP enqueue alone cannot confirm rewind');
    assert.equal(await composer.inputValue(), existingDraft, 'HTTP acknowledgment does not restore yet');
    f.send({ type: 'rewound', to_seq: actualSeq, restored_files: false, restore_error: '' });
    await warning.waitFor({ state: 'hidden' });
    await sameComposer(composer, composerNode, `${text}\n\n${existingDraft}`, text.length);
    assert.equal(calls.send.length, 1, 'Ask cancellation and confirmed rewind never resend');
    assert.deepEqual(mutations, [
      { method: 'POST', path: '/api/v1/sessions/parent/messages' },
      { method: 'POST', path: '/api/v1/sessions/parent/rewind' },
    ], 'no queue cancellation/pull, interrupt, side-chat creation, or implicit send');
    assert(!f.requests.some(r => /^(PUT|POST|DELETE) .*\/settings\//.test(r)), 'no settings writes');
    assert.deepEqual(f.errors, []);
  } catch (error) {
    console.error('SENT DURING TURN DIAGNOSTIC', JSON.stringify({ phase: f.phase, calls, mutations,
      errors: f.errors, console: f.consoleMessages, requests: f.requests.slice(-12) }));
    throw error;
  } finally { await f.context.close(); }
}

async function main() {
  const playwright = require(process.env.PLAYWRIGHT_CORE || 'playwright-core');
  const { server, origin } = await serve();
  try {
    const engine = process.env.ENGINE || 'chromium';
    assert(['chromium', 'webkit', 'both'].includes(engine), 'ENGINE=chromium,webkit,both');
    let checked = 0;
    const failures = [];
    for (const name of engine === 'both' ? ['chromium', 'webkit'] : [engine]) {
      for (const phone of [false, true]) for (const outcome of [
        'rewind', 'rewind-cancel', 'rewind-early', 'rewind-reset-error', 'rewind-api-error', 'rewind-replay', 'rewind-attachments',
        'queued-ready', 'queued-empty', 'queued-taken-race', 'queued-failure', 'queued-failure-edited', 'queued-failure-reverted', 'queued-failure-retry',
        'queued-attachments', 'queued-stacking', 'queued-stacking-failure', 'queued-send-error', 'queued-send-now', 'queued-layout', 'queued-response-lost',
        'queued-attachments-edited-stacking', 'queued-send-failure-recovery',
        'waiting-short', 'waiting-empty', 'rejected', 'sent-during-turn-human', 'sent-during-turn-unknown', 'sent-during-turn-legacy',
      ]) {
        const label = `${name}/${phone ? 'phone' : 'desktop'}/${outcome}`;
        if (process.env.CASE_EXACT && label !== process.env.CASE_EXACT) continue;
        if (process.env.CASE && !label.includes(process.env.CASE)) continue;
        const browser = await launchBrowser(playwright, name);
        try {
          console.log(`RUN ${label}`);
          await within(label, () => outcome === 'rejected' ? checkRejected(browser, origin, phone)
            : outcome.startsWith('waiting-') ? checkWaitingPlacement(browser, origin, phone, outcome === 'waiting-empty')
            : outcome.startsWith('sent-during-turn-') ? checkSentDuringTurn(browser, origin, phone, outcome)
            : ['queued-attachments-edited-stacking', 'queued-send-failure-recovery'].includes(outcome) ? checkQueuedRecoveryRace(browser, origin, phone, outcome)
            : outcome.startsWith('queued-') ? checkQueued(browser, origin, phone, outcome) : checkRewind(browser, origin, phone, outcome));
          checked++;
          console.log(`PASS ${label}`);
        } catch (error) {
          failures.push(label);
          console.error(`FAIL ${label}`, error.stack || error);
        } finally { await browser.close(); }
      }
    }
    assert(checked + failures.length, 'CASE/CASE_EXACT must select at least one case');
    console.log(`SUMMARY ${checked} passed; ${failures.length} failed`);
    assert.deepEqual(failures, [], 'all selected cases must pass; isolated reruns do not erase cohort failures');
  } finally { await new Promise(resolve => server.close(resolve)); }
}
main().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
