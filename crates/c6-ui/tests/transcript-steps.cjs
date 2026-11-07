#!/usr/bin/env node
'use strict';

// The production transcript with owned HTTP/WS fixtures, not recreated markup.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, launchBrowser } = require('./motion.cjs');
const out = process.env.EVIDENCE || '/tmp/q8-steps/after';
const baseline = process.env.BEFORE === '1';
const selected = (key, value) => !process.env[key] || process.env[key].split(',').includes(value);

async function frames(page, count = 2) {
  await page.evaluate(async count => {
    for (let i = 0; i < count; i++) await new Promise(resolve => requestAnimationFrame(resolve));
  }, count);
}

async function startSamples(page) {
  await page.evaluate(() => {
    const el = [...document.querySelectorAll('.overflow-y-auto')].find(el => el.querySelector('.c6-transcript-row'));
    if (!el) throw new Error('transcript scroll box missing');
    const content = el.firstElementChild;
    const anchor = el.querySelector('#parent-e64');
    const test = window.__steps = { el, content, anchor, frames: [], changes: [], effects: [], writes: [], phase: 'initial', active: true };
    const scroll = Element.prototype.scrollTo;
    Element.prototype.scrollTo = function (...args) {
      if (this === el) test.writes.push({ time: performance.now(), phase: test.phase, top: args[0]?.top, stack: new Error().stack });
      return scroll.apply(this, args);
    };
    const animate = Element.prototype.animate;
    Element.prototype.animate = function (...args) {
      const animation = animate.apply(this, args);
      if (this.hasAttribute('data-follow-content')) test.effects.push(animation);
      return animation;
    };
    const name = el => el.id || el.getAttribute('data-transcript-status') || `${el.tagName}.${String(el.className).slice(0, 50)}: ${el.textContent.slice(0, 90)}`;
    const watch = new ResizeObserver(entries => {
      for (const entry of entries) test.changes.push({ time: performance.now(), phase: test.phase, kind: 'size',
        node: name(entry.target), height: entry.contentRect.height });
    });
    const observe = node => {
      if (node.nodeType !== 1) return;
      if (node.matches('.c6-transcript-row, [data-transcript-status]')) watch.observe(node);
      node.querySelectorAll('.c6-transcript-row, [data-transcript-status]').forEach(row => watch.observe(row));
    };
    observe(el.parentElement);
    const mutations = new MutationObserver(entries => {
      for (const entry of entries) {
        if (entry.type === 'childList') for (const node of entry.addedNodes) observe(node);
        test.changes.push({ time: performance.now(), phase: test.phase, kind: entry.type, node: name(entry.target.parentElement || entry.target) });
      }
    });
    mutations.observe(el.parentElement, { childList: true, characterData: true, subtree: true });
    const tick = time => {
      if (!test.active) { watch.disconnect(); mutations.disconnect(); return; }
      const y = el.getBoundingClientRect().top;
      if (test.frames.length && test.frames.at(-1).scrollerY !== y) {
        const ancestors = [];
        for (let parent = el; parent; parent = parent.parentElement) ancestors.push({ tag: parent.tagName, class: parent.className,
          scroll: parent.scrollTop, height: parent.scrollHeight, viewport: parent.clientHeight, y: parent.getBoundingClientRect().top });
        test.changes.push({ time, phase: test.phase, kind: 'ancestor scroll', ancestors });
      }
      test.frames.push({ time, phase: test.phase, top: el.scrollTop, height: el.scrollHeight, viewport: el.clientHeight,
        anchor: anchor.getBoundingClientRect().top, content: content.getBoundingClientRect().top,
        scrollerY: y, pageY: scrollY,
        thumbSize: el.clientHeight ** 2 / el.scrollHeight,
        thumbTop: el.scrollTop * el.clientHeight / el.scrollHeight });
      requestAnimationFrame(tick);
    };
    requestAnimationFrame(tick);
  });
}

async function idle(page) {
  await settle(page);
  await page.waitForFunction(() => {
    const s = window.__steps;
    return !s || s.el.scrollHeight - s.el.clientHeight - s.el.scrollTop < 2;
  }, null, { polling: 'raf' });
  await frames(page);
}

async function scenario(f) {
  const { page } = f;
  const phase = name => {
    if (process.env.DEBUG_FIXTURE) console.log(`  STEP ${name}`);
    return page.evaluate(name => { __steps.phase = name; }, name);
  };
  const send = async (name, action) => {
    await phase(name);
    const seq = await action();
    if (typeof seq === 'number') await page.waitForFunction(seq => document.getElementById(`parent-e${seq}`), seq);
    await frames(page, 4);
    await f.onPhase?.(name);
  };
  const usage = { type: 'usage', model: 'test', input_tokens: 2400, output_tokens: 120, cache_read_tokens: 0, cache_write_tokens: 0, reasoning_tokens: 0 };
  await phase('send');
  await page.locator('textarea').first().fill('Measure one send and finish');
  await page.locator('.c6-send-slot button[data-active="true"]').first().click();
  await page.waitForFunction(() => document.querySelector('textarea').value === '');
  await send('user', () => f.send({ type: 'user', command_id: 'steps-user', text: 'Measure one send and finish' }));
  await send('start', () => f.send({ type: 'turn_started', turn: 70 }));
  await send('usage', () => f.send(usage));
  await send('tools burst', () => {
    f.send({ type: 'assistant', turn: 70, stop_reason: 'tool_use', blocks: [
      { type: 'text', text: 'Checking the transcript in a tool call.' },
      { type: 'tool_use', id: 'steps-tool', name: 'shell', input_json: JSON.stringify({ command: ['echo', 'hello'] }) },
    ] });
    return f.send({ type: 'tool_result', call_id: 'steps-tool', tool: 'shell', output: 'hello', is_error: false, truncated: false, duration_ms: 4 });
  });
  await send('background', () => f.send({ type: 'background_task', task_id: 'steps-bg', kind: 'shell', description: 'Background measurement', state: 'started', notify: true }));
  await send('queued', async () => {
    await page.locator('textarea').first().fill('A queued follow-up');
    await page.locator('.c6-send-slot button[data-active="true"]').first().click();
    f.stream({ type: 'queue', queued: [{ command_id: 'steps-queue', text: 'A queued follow-up', created_at: 1, rejected: null }] });
    await page.getByText('A queued follow-up', { exact: true }).waitFor();
  });
  await send('text', async () => {
    f.stream({ type: 'text', turn: 70, text: 'The measured reply is ready.' });
    await page.getByText('The measured reply is ready.', { exact: true }).waitFor();
  });
  await send('finish burst', () => {
    f.send({ type: 'assistant', turn: 70, stop_reason: 'end_turn', blocks: [{ type: 'text', text: 'The measured reply is ready.' }] });
    f.send(usage);
    f.send({ type: 'turn_ended', turn: 70, reason: 'completed' });
    f.send({ type: 'background_task', task_id: 'steps-bg', kind: 'shell', description: 'Background measurement', state: 'finished', notify: true });
    f.stream({ type: 'queue', queued: [] });
  });
  await page.waitForFunction(() => document.body.textContent.includes('Turn 70 ·'));
  await idle(page);
}

function summarize(data) {
  const changes = [];
  let previous;
  for (const frame of data.frames) {
    if (previous) {
      const movement = frame.anchor - previous.anchor;
      if (Math.abs(movement) > .5) changes.push({ time: frame.time, phase: frame.phase, movement,
        scroll: frame.top - previous.top, growth: frame.height - previous.height, viewport: frame.viewport - previous.viewport,
        gapMs: frame.time - previous.time });
    }
    previous = frame;
  }
  const jumps = changes.filter(frame => Math.abs(frame.movement) > 12);
  const corrections = data.frames.slice(1).filter((frame, i) => Math.abs(frame.top - data.frames[i].top) > .5);
  const heightFrames = data.frames.slice(1).flatMap((frame, i) => {
    const before = data.frames[i];
    return frame.height !== before.height || frame.viewport !== before.viewport ? [{ phase: frame.phase,
      growth: frame.height - before.height, viewport: frame.viewport - before.viewport }] : [];
  });
  return { frames: data.frames.length, movingFrames: changes.length, largeVisualFrames: jumps.length,
    scrollCorrections: corrections.length, heightFrames,
    shellMovement: Math.max(...data.frames.map(frame => frame.scrollerY)) - Math.min(...data.frames.map(frame => frame.scrollerY)),
    largest: Math.max(0, ...changes.map(frame => Math.abs(frame.movement))), jumpFrames: jumps,
    heights: [...new Set(data.frames.map(frame => frame.height))].length,
    maxFrameMs: Math.max(0, ...data.frames.slice(1).map((f, i) => f.time - data.frames[i].time)) };
}

async function check(browser, origin, theme, view, choice) {
  const viewport = view === 'phone' ? { width: 390, height: 844 } : { width: 1440, height: 1000 };
  const motion = { choice, effective: choice, os: 'no-preference' };
  const name = `${theme}-${view}-${choice}`;
  let started;
  const f = await fixture(browser, origin, viewport, theme, motion, progress => { started = progress; }, {}, { instrumentation: false })
    .catch(async error => {
      // Hydration can fail before the normal per-case finally owns the context.
      // Close its sockets too, so the server can stop and report the failure.
      fs.writeFileSync(path.join(out, `${name}.failure.json`), JSON.stringify({ error: String(error.stack || error),
        phase: started?.phase, errors: started?.errors, consoleMessages: started?.consoleMessages }, null, 2));
      await started?.context.close();
      throw error;
    });
  const { page, context } = f;
  let cdp, layers = [], compositor;
  try {
    if (process.env.CPU_RATE || process.env.TRACE || process.env.BLOCK_MS) {
      cdp = await context.newCDPSession(page);
      if (process.env.CPU_RATE) await cdp.send('Emulation.setCPUThrottlingRate', { rate: Number(process.env.CPU_RATE) });
      if (process.env.TRACE) {
        cdp.on('LayerTree.layerTreeDidChange', event => { layers = event.layers || []; });
        await cdp.send('LayerTree.enable');
        f.onPhase = async phase => {
          if (phase !== 'tools burst') return;
          const remote = await cdp.send('Runtime.evaluate', { expression: 'window.__steps.anchor.parentElement.parentElement' });
          const { node } = await cdp.send('DOM.describeNode', { objectId: remote.result.objectId });
          const matching = layers.filter(layer => layer.backendNodeId === node.backendNodeId);
          compositor = await Promise.all(matching.map(async layer => ({ ...layer,
            reasons: await cdp.send('LayerTree.compositingReasons', { layerId: layer.layerId }) })));
        };
        await cdp.send('Tracing.start', { categories: 'devtools.timeline,blink.animations,blink.user_timing,cc,disabled-by-default-devtools.timeline', transferMode: 'ReturnAsStream' });
      }
      if (process.env.BLOCK_MS) {
        const prior = f.onPhase;
        f.onPhase = async phase => {
          await prior?.(phase);
          if (phase !== (process.env.BLOCK_PHASE || 'tools burst')) return;
          const film = [];
          const capture = event => {
            cdp.send('Page.screencastFrameAck', { sessionId: event.sessionId }).catch(() => {});
            const file = `${name}-block-${String(film.length).padStart(3, '0')}.png`;
            fs.writeFileSync(path.join(out, file), Buffer.from(event.data, 'base64'));
            film.push({ file, metadata: event.metadata });
          };
          cdp.on('Page.screencastFrame', capture);
          await cdp.send('Page.startScreencast', { format: 'png', everyNthFrame: 1 });
          const blocked = await page.evaluate(ms => {
            const r = __steps.anchor.querySelector('.md p').getBoundingClientRect();
            const content = __steps.el.querySelector('[data-follow-content]');
            const effects = content?.getAnimations().map(a => ({ time: a.currentTime, state: a.playState, pending: a.pending,
              timing: a.effect.getComputedTiming(), frames: a.effect.getKeyframes() }));
            const start = performance.timeOrigin + performance.now();
            const until = performance.now() + ms;
            while (performance.now() < until) {} // Diagnostic workload, never a correctness wait.
            return { start, end: performance.timeOrigin + performance.now(), effects, anchor: { x: r.x, y: r.y, width: r.width, height: r.height },
              viewport: { width: innerWidth, height: innerHeight } };
          }, Number(process.env.BLOCK_MS));
          await cdp.send('Page.stopScreencast');
          cdp.off('Page.screencastFrame', capture);
          fs.writeFileSync(path.join(out, `${name}-block.json`), JSON.stringify({ ...blocked, film }, null, 2));
        };
      }
    }
    if (view === 'tile') await page.addStyleTag({ content: '[data-tile-motion] { width: 400px !important; }' });
    await settle(page);
    await page.evaluate(() => {
      const el = [...document.querySelectorAll('.overflow-y-auto')].find(el => el.querySelector('.c6-transcript-row'));
      el.scrollTop = el.scrollHeight;
    });
    await frames(page, 3);
    await startSamples(page);
    await scenario(f);
    if (process.env.TRACE) {
      const complete = new Promise(resolve => cdp.once('Tracing.tracingComplete', resolve));
      await cdp.send('Tracing.end');
      const { stream } = await complete;
      let trace = '';
      for (;;) {
        const part = await cdp.send('IO.read', { handle: stream });
        trace += part.data;
        if (part.eof) break;
      }
      await cdp.send('IO.close', { handle: stream });
      fs.writeFileSync(path.join(out, `${name}-trace.json`), trace);
    }
    const data = await page.evaluate(() => {
      __steps.active = false;
      return { frames: __steps.frames, changes: __steps.changes, writes: __steps.writes, effects: __steps.effects.map(a => ({
        duration: a.effect.getTiming().duration, composite: a.effect.composite, frames: a.effect.getKeyframes(), state: a.playState,
      })) };
    });
    const summary = summarize(data);
    fs.writeFileSync(path.join(out, `${name}.json`), JSON.stringify({ summary, cpuRate: Number(process.env.CPU_RATE || 1), compositor, ...data }, null, 2));
    await page.screenshot({ path: path.join(out, `${name}.png`) });
    // Trusted browser input and native scrolling, not a synthetic event plus
    // a scrollTop assignment. Mobile WebKit has no Playwright wheel support;
    // use the same native PageUp route as the existing phone regression.
    if (view === 'phone') {
      await page.evaluate(() => __steps.el.focus({ preventScroll: true }));
      await page.keyboard.press('PageUp');
    } else {
      const box = await page.evaluate(() => {
        const r = __steps.el.getBoundingClientRect();
        return { x: r.x, y: r.y, width: r.width, height: r.height };
      });
      await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
      await page.mouse.wheel(0, -400);
    }
    await page.waitForFunction(() => __steps.el.scrollHeight - __steps.el.clientHeight - __steps.el.scrollTop > 100);
    await settle(page); await frames(page);
    // WebKit's native input scroll continues after the first nonzero delta
    // and is not a document.getAnimations() effect. Finish that input before
    // measuring whether a later message steals the reading position.
    await page.waitForFunction(() => {
      const top = __steps.el.scrollTop;
      __steps.inputStable = top === __steps.inputTop ? (__steps.inputStable || 0) + 1 : 0;
      __steps.inputTop = top;
      return __steps.inputStable >= 4;
    }, null, { polling: 'raf' });
    const before = await page.evaluate(() => __steps.el.scrollTop);
    f.send({ type: 'user', command_id: 'steps-up', text: 'Arrives while reading above' });
    f.send({ type: 'turn_started', turn: 71 });
    f.send({ type: 'assistant', turn: 71, stop_reason: 'end_turn', blocks: [{ type: 'text', text: 'Do not move the reader.' }] });
    f.send({ type: 'turn_ended', turn: 71, reason: 'completed' });
    await page.getByText('Do not move the reader.', { exact: true }).waitFor({ state: 'attached' });
    await settle(page);
    await frames(page, 15); // Negative assertion: delayed following must not steal position.
    const after = await page.evaluate(() => __steps.el.scrollTop);
    data.reader = { inputStart: data.frames.at(-1).top, before, after, drift: after - before };
    fs.writeFileSync(path.join(out, `${name}.json`), JSON.stringify({ summary, cpuRate: Number(process.env.CPU_RATE || 1), compositor, ...data }, null, 2));
    assert(Math.abs(after - before) < 2, `scrolled-up position changed ${before} -> ${after}`);
    if (!baseline && theme === 'light' && view === 'desktop' && choice === 'off') {
      const usage = { type: 'usage', model: 'background-fixture', input_tokens: 1, output_tokens: 1,
        cache_read_tokens: 0, cache_write_tokens: 0, reasoning_tokens: 0 };
      await page.evaluate(() => {
        __steps.originalRAF = window.requestAnimationFrame;
        __steps.originalCancel = window.cancelAnimationFrame;
        __steps.heldFrames = []; __steps.canceledFrames = [];
        Object.defineProperty(document, 'hidden', { configurable: true, value: false });
        window.requestAnimationFrame = callback => { __steps.heldFrames.push(callback); return 2147483000; };
        window.cancelAnimationFrame = id => __steps.canceledFrames.push(id);
      });
      try {
        const first = f.send(usage);
        await page.waitForFunction(() => __steps.heldFrames.length === 1, null, { polling: 20 });
        assert.equal(await page.evaluate(seq => document.getElementById(`parent-e${seq}`), first), null, 'visible delivery waits for its presentation frame');
        await page.evaluate(() => Object.defineProperty(document, 'hidden', { configurable: true, value: true }));
        const second = f.send(usage);
        await page.waitForFunction(seq => document.getElementById(`parent-e${seq}`), second, { polling: 20 });
        assert(await page.evaluate(([a, b]) => Boolean(document.getElementById(`parent-e${a}`).compareDocumentPosition(document.getElementById(`parent-e${b}`)) & Node.DOCUMENT_POSITION_FOLLOWING),
          [first, second]), 'hiding flushes the entire pending queue in wire order');
        assert.deepEqual(await page.evaluate(() => __steps.canceledFrames), [2147483000], 'hidden flush cancels and clears the pending handle');
      } finally {
        await page.evaluate(() => {
          window.requestAnimationFrame = __steps.originalRAF; window.cancelAnimationFrame = __steps.originalCancel;
          __steps.heldFrames = []; delete document.hidden;
        });
      }
      // Model hidden-tab delivery without requiring a real browser window or
      // letting rAF accidentally make the check pass. The visible failure case
      // also verifies the fallback when requesting a frame is unavailable.
      for (const hidden of [true, false]) {
        await page.evaluate(hidden => {
          __steps.originalRAF = window.requestAnimationFrame;
          __steps.frameRequests = 0;
          Object.defineProperty(document, 'hidden', { configurable: true, value: hidden });
          window.requestAnimationFrame = () => { __steps.frameRequests++; throw new Error('fixture: frame unavailable'); };
        }, hidden);
        try {
          const seq = f.send(usage);
          await page.waitForFunction(seq => document.getElementById(`parent-e${seq}`), seq, { polling: 20 });
          assert.equal(await page.evaluate(() => __steps.frameRequests), hidden ? 0 : 1, 'hidden delivery bypasses rAF; visible registration failure still flushes');
        } finally {
          await page.evaluate(() => { window.requestAnimationFrame = __steps.originalRAF; delete document.hidden; });
        }
      }
    }
    if (!baseline && theme === 'light' && view === 'tile' && choice === 'full') {
      // After a width change while paused, resuming must refresh intrinsic
      // heights before rows leave the live tail. A width-old fallback must
      // not return on the first tail exit after native End resumes following.
      await page.addStyleTag({ content: '[data-tile-motion] { width: 520px !important; }' });
      await settle(page); await frames(page, 4);
      await page.evaluate(() => __steps.el.focus({ preventScroll: true }));
      await page.keyboard.press('End'); await idle(page);
      const height = await page.evaluate(() => __steps.el.scrollHeight);
      const seq = f.send({ type: 'usage', model: 'pause-resize', input_tokens: 1, output_tokens: 1,
        cache_read_tokens: 0, cache_write_tokens: 0, reasoning_tokens: 0 });
      await page.waitForFunction(seq => document.getElementById(`parent-e${seq}`), seq);
      await idle(page);
      const next = await page.evaluate(() => __steps.el.scrollHeight);
      data.pauseResize = { before: height, after: next };
      fs.writeFileSync(path.join(out, `${name}.json`), JSON.stringify({ summary, cpuRate: Number(process.env.CPU_RATE || 1), compositor, ...data }, null, 2));
      assert.equal(next, height, 'resuming after a paused width change must not restore a stale row height');
    }
    assert.deepEqual(f.errors, [], 'no browser crashes or resize loops');
    console.log(JSON.stringify({ name, ...summary, readerDrift: after - before }));
    if (!baseline) {
      // One correction per actual content addition/removal, not for Working,
      // usage, task counts, guessed 6rem heights, or animation playback.
      const budget = view === 'phone' ? 7 : 5;
      assert(summary.scrollCorrections <= budget, `${name}: ${summary.scrollCorrections} separate scroll jumps (budget ${budget})`);
      assert(summary.shellMovement < 1, 'composer focus must not pan the app shell around the transcript');
      assert(!summary.heightFrames.some(frame => ['usage', 'background'].includes(frame.phase)), 'usage/task status must not change transcript height');
      assert(await page.evaluate(() => [...__steps.el.querySelectorAll('.c6-transcript-row')].slice(-16)
        .every(row => getComputedStyle(row).contentVisibility === 'visible')), 'the live tail counts event rows, not queued/status siblings');
      if (choice === 'full') {
        assert(data.effects.length > 0, 'Full must glide content, not just snap fewer times');
        assert(data.effects.every(a => a.duration === 200 && a.composite === 'replace' && a.frames.every(frame =>
          frame.transform === 'none' || /^translateY\(-?[\d.]+px\)$/.test(frame.transform))), 'bounded native vertical translations only, never scale');
      } else assert.equal(data.effects.length, 0, 'Reduced/Off follow instantly without translation');
    }
  } finally { await cdp?.detach(); await context.close(); }
}

async function main() {
  fs.mkdirSync(out, { recursive: true });
  const { server, origin } = await serve();
  const playwright = require(process.env.PLAYWRIGHT_CORE || '/home/zheilbron/.bun/install/cache/playwright-core@1.50.1@@@1');
  const browser = await launchBrowser(playwright, process.env.ENGINE || 'chromium');
  let count = 0;
  try {
    for (const theme of ['light', 'dark'].filter(x => selected('THEMES', x)))
      for (const view of ['desktop', 'tile', 'phone'].filter(x => selected('VIEWS', x)))
        for (const choice of ['full', 'reduced', 'off'].filter(x => selected('MOTIONS', x))) {
          await check(browser, origin, theme, view, choice);
          count++;
        }
    assert(count > 0, 'filters must select at least one case');
  } finally { await browser.close(); await new Promise(resolve => server.close(resolve)); }
}

module.exports = { summarize };
if (require.main === module) main().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
