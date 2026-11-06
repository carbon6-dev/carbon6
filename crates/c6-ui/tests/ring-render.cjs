#!/usr/bin/env node
'use strict';

// Production UI, private fake head. BASELINE=1 records the old inner-SVG
// animation without asserting the new compositor invariant. Timings are evidence,
// never pass/fail thresholds: shared machines do not have a fixed frame budget.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, launchBrowser } = require('./motion.cjs');
const output = process.env.EVIDENCE || '/tmp/q3-ring';
const baseline = process.env.BASELINE === '1';
const phase = process.env.LABEL || (baseline ? 'before' : 'after');
const paragraphs = Number(process.env.PARAGRAPHS || 12);
const playwright = require(process.env.PLAYWRIGHT_CORE || '/home/zheilbron/.bun/install/cache/playwright-core@1.50.1@@@1');
const delay = ms => new Promise(resolve => setTimeout(resolve, ms));
const prose = (i, count = paragraphs) => `Q3 item ${i}\n\n` + ('A **rendered** paragraph with `inline code`, a [local link](/r/home/sessions/parent), and some ordinary transcript text.\n\n'.repeat(count)) +
  '\n```rust\nfn main() {\n    println!("a highlighted code block");\n}\n```\n\n| Column | Value |\n| --- | --- |\n| entry | measured |\n';

function instrument() {
  const NativeSocket = window.WebSocket;
  window.WebSocket = class extends NativeSocket {
    constructor(...args) {
      super(...args);
      this.addEventListener('message', message => {
        const data = JSON.parse(message.data);
        if (data.type === 'sessions' && window.__q3) __q3.snapshots++;
        const text = data.type === 'event' && data.event.type === 'assistant' && data.event.blocks.find(b => b.type === 'text')?.text;
        const match = text && /^Q3 item (\d+)/.exec(text);
        if (!match || !window.__q3?.measuring) return;
        const id = Number(match[1]);
        __q3.pending = { id, start: performance.now(), mutations: 0 };
        performance.mark(`q3-receive-${id}`);
      });
    }
  };
  window.__q3 = { tasks: [], frames: [], renders: [], snapshots: 0, measuring: false, pending: null };
  new PerformanceObserver(list => __q3.tasks.push(...list.getEntries().map(e => ({ start: e.startTime, duration: e.duration })))).observe({ type: 'longtask' });
  new MutationObserver(records => {
    const p = __q3.pending;
    if (!p) return;
    p.mutations += records.length;
    if (![...document.querySelectorAll('.md')].some(el => el.textContent.startsWith(`Q3 item ${p.id}`))) return;
    p.dom = performance.now();
    performance.mark(`q3-dom-${p.id}`);
    __q3.pending = null;
    requestAnimationFrame(() => {
      p.frame = performance.now();
      __q3.renders.push(p);
      performance.mark(`q3-frame-${p.id}`);
    });
  }).observe(document, { childList: true, subtree: true, attributes: true, characterData: true });
  let last;
  function frame(now) {
    if (__q3.measuring && last >= __q3.start) __q3.frames.push({ at: now, gap: now - last });
    last = now;
    requestAnimationFrame(frame);
  }
  requestAnimationFrame(frame);
}

async function layers(cdp, page) {
  let tree = [];
  cdp.on('LayerTree.layerTreeDidChange', e => { tree = e.layers || []; });
  await cdp.send('LayerTree.enable');
  await page.evaluate(() => new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))));
  const { root } = await cdp.send('DOM.getDocument');
  const { nodeId } = await cdp.send('DOM.querySelector', { nodeId: root.nodeId, selector: '.c6-ring-working' });
  const { node } = await cdp.send('DOM.describeNode', { nodeId });
  const matching = tree.filter(layer => layer.backendNodeId === node.backendNodeId);
  const result = [];
  for (const layer of matching) result.push({ ...layer, reasons: await cdp.send('LayerTree.compositingReasons', { layerId: layer.layerId }) });
  await cdp.send('LayerTree.disable');
  return result;
}

async function traceEnd(cdp, file) {
  const complete = new Promise(resolve => cdp.once('Tracing.tracingComplete', resolve));
  await cdp.send('Tracing.end');
  const { stream } = await complete;
  const fd = fs.openSync(file, 'w');
  for (;;) {
    const chunk = await cdp.send('IO.read', { handle: stream });
    fs.writeSync(fd, chunk.base64Encoded ? Buffer.from(chunk.data, 'base64') : chunk.data);
    if (chunk.eof) break;
  }
  fs.closeSync(fd);
  await cdp.send('IO.close', { handle: stream });
}

async function blockedFrames(cdp, page, name) {
  // Separate diagnostic, outside the natural workload's timings. Screencast
  // frames come from the compositor while the page's main thread is blocked;
  // rAF alone cannot tell whether a composited ring kept moving.
  const frames = [];
  const capture = frame => {
    frames.push({ timestamp: frame.metadata.timestamp * 1000, data: frame.data });
    cdp.send('Page.screencastFrameAck', { sessionId: frame.sessionId }).catch(() => {});
  };
  const bounds = await page.locator('.c6-ring-working').first().boundingBox();
  cdp.on('Page.screencastFrame', capture);
  await cdp.send('Page.startScreencast', { format: 'png', everyNthFrame: 1 });
  const block = await page.evaluate(() => {
    const start = performance.now();
    performance.mark('q3-block-start');
    while (performance.now() - start < 600) { /* Deliberate main-thread challenge, not app code. */ }
    performance.mark('q3-block-end');
    return { start: performance.timeOrigin + start, end: performance.timeOrigin + performance.now() };
  });
  await cdp.send('Page.stopScreencast');
  cdp.off('Page.screencastFrame', capture);
  const during = frames.filter(frame => frame.timestamp >= block.start && frame.timestamp <= block.end);
  during.forEach((frame, i) => fs.writeFileSync(path.join(output, `${name}-blocked-${i}.png`), Buffer.from(frame.data, 'base64')));
  return { ...block, bounds, captured: during.map(frame => frame.timestamp) };
}

async function run(browser, origin, theme, run) {
  const motion = { choice: 'full', os: 'no-preference', effective: 'full' };
  const events = Array.from({ length: 30 }, (_, i) => ({ seq: i + 66, ts: i + 66, type: 'assistant', turn: i + 66,
    blocks: [{ type: 'text', text: prose(i + 1000, 12) }], stop_reason: 'end_turn' }));
  const f = await fixture(browser, origin, { width: 1600, height: 1000 }, theme, motion, () => {}, {}, { events });
  const { page, context } = f;
  try {
    await context.addInitScript(instrument);
    await page.evaluate(() => {
      const views = [{ view: 'session', id: 'parent' }, { view: 'sessions' }, { view: 'workers' }];
      const tiles = views.map((view, i) => ({ key: i + 10, columns: [{ key: i + 20, view }], cursor: 0,
        active: i + 20, used: i + 20, back: [], forward: [], stack: i === 0 ? 0 : 1, minimized: false }));
      localStorage.setItem('c6.home.workspace.v2', JSON.stringify({ tiles, focus: 0, next: 40, full: false }));
    });
    await page.reload();
    await page.locator('textarea').first().waitFor();
    await page.getByText('Q3 item 1029', { exact: false }).first().waitFor();
    await page.waitForFunction(() => document.querySelectorAll('[data-tile-motion]').length === 3);
    await settle(page);
    // Capture before idle -> working, then verify both the rim and context stay.
    await page.evaluate(() => {
      window.__rim = document.querySelector('circle[r="10.5"]');
      window.__contextArc = __rim.closest('span').querySelector('circle[r="7"]');
    });
    await f.setState('working');
    await page.locator('.c6-ring-working').first().waitFor();
    await page.evaluate(() => {
      window.__spin = document.querySelector('.c6-ring-working');
      window.__animation = __spin.getAnimations().find(a => a.animationName === 'spin');
      window.assertRing = () => __rim.isConnected && __contextArc.isConnected && __spin.isConnected &&
        __spin === document.querySelector('.c6-ring-working') && __spin.getAnimations().includes(__animation);
    });
    const retainedChecks = [await page.evaluate(() => assertRing())];
    for (const status of ['starting', 'running']) {
      const snapshots = await page.evaluate(() => __q3.snapshots);
      await f.setState('working', { status });
      await page.waitForFunction(count => __q3.snapshots > count, snapshots);
      retainedChecks.push(await page.evaluate(() => assertRing()));
    }
    const cdp = await context.newCDPSession(page);
    await cdp.send('Emulation.setCPUThrottlingRate', { rate: Number(process.env.CPU || 4) });
    const compositor = await layers(cdp, page);
    const shape = await page.evaluate(() => ({ tag: __spin.tagName, animations: __spin.getAnimations().map(a => ({ name: a.animationName,
      duration: a.effect.getTiming().duration, frames: a.effect.getKeyframes() })),
      contextAnimations: __contextArc.getAnimations().length, rimAnimations: __rim.getAnimations().length,
      contextRotates: !!__contextArc.closest('.c6-ring-working'), rimDash: getComputedStyle(__rim).strokeDasharray }));
    if (!baseline) {
      assert.equal(shape.tag, 'svg', 'rotate the outer SVG, not its inner circle');
      assert.equal(shape.animations.length, 1, 'no paint-bound arc morph');
      assert.equal(shape.rimAnimations, 0, 'no animation on an inner SVG element');
      assert.equal(shape.animations[0].duration, 1400, 'keep the spin period');
      assert.equal(shape.contextAnimations, 0, 'context does not spin');
      assert.equal(shape.contextRotates, false, 'the context is not inside the rotating SVG');
      assert(compositor.some(layer => layer.reasons.compositingReasons.some(r => /active.*transform animation/i.test(r))),
        `working SVG has its own active-transform compositor layer: ${JSON.stringify(compositor)}`);
    }
    if (run === 0) {
      await cdp.send('Profiler.enable');
      await cdp.send('Profiler.start');
      await cdp.send('Tracing.start', { categories: 'devtools.timeline,blink.animations,blink.user_timing,cc,benchmark,viz,disabled-by-default-devtools.timeline,disabled-by-default-devtools.timeline.frame', transferMode: 'ReturnAsStream' });
    }
    await page.evaluate(() => { __q3.measuring = true; __q3.start = performance.now(); performance.mark('q3-start'); });
    for (let i = 0; i < 12; i++) {
      // Actual WS deltas: grow Markdown, then commit a new durable transcript item.
      const turn = 200 + i;
      for (let chunk = 0; chunk < 8; chunk++) {
        f.stream({ type: 'text', turn, seq: 0, text: `Streaming ${i}/${chunk}. **Live** text and code. ` });
        await delay(35); // Workload pacing, not a test completion condition.
      }
      await page.locator('.md').filter({ hasText: `Streaming ${i}/7.` }).first().waitFor();
      f.send({ type: 'assistant', turn, blocks: [{ type: 'text', text: prose(i) }], stop_reason: 'end_turn' });
      await page.waitForFunction(id => __q3.renders.some(r => r.id === id), i);
      await f.setState('working', { last_seq: 100 + i, updated_at: 100 + i });
      retainedChecks.push(await page.evaluate(() => assertRing()));
    }
    await page.evaluate(() => { __q3.end = performance.now(); __q3.measuring = false; performance.mark('q3-end'); });
    let blocked;
    if (run === 0) {
      const { profile } = await cdp.send('Profiler.stop');
      fs.writeFileSync(path.join(output, `${phase}-${theme}-${run}.cpuprofile`), JSON.stringify(profile));
      blocked = await blockedFrames(cdp, page, `${phase}-${theme}`);
      await traceEnd(cdp, path.join(output, `${phase}-${theme}-trace.json`));
    }
    const data = await page.evaluate(() => ({ ...__q3, nodes: document.querySelectorAll('*').length }));
    data.tasks = data.tasks.filter(t => t.start >= data.start && t.start <= data.end);
    const sorted = data.frames.map(f => f.gap).sort((a, b) => a - b);
    const result = { phase, theme, run, chromium: browser.version(), cpu: Number(process.env.CPU || 4), paragraphs,
      compositor, shape, retainedChecks, blocked, ...data, summary: { elapsed: data.end - data.start, frames: sorted.length,
        missedRafSlots: data.frames.reduce((n, f) => n + Math.max(0, Math.round(f.gap / (1000 / 60)) - 1), 0),
        rafP95: sorted[Math.floor(sorted.length * .95)], rafMax: sorted.at(-1), longTasks: data.tasks.length,
        longTaskMs: data.tasks.reduce((n, t) => n + t.duration, 0), renderMs: data.renders.map(r => r.dom - r.start),
        renderFrameMs: data.renders.map(r => r.frame - r.start) } };
    fs.writeFileSync(path.join(output, `${phase}-${theme}-${run}.json`), JSON.stringify(result, null, 2));
    console.log(JSON.stringify({ phase, theme, run, ...result.summary }));
    if (!baseline) assert(retainedChecks.every(Boolean), 'idle -> working, new items and live snapshots retain the same nodes AND spin animation');
    await page.screenshot({ path: path.join(output, `${phase}-${theme}-${run}.png`) });
    // No benchmark threshold here. Correctness covers all states/preferences.
    for (const state of ['waiting', 'failed', 'idle', 'working']) {
      await f.setState(state, { status: state === 'failed' ? 'failed' : state === 'working' ? 'running' : 'idle' });
      await page.waitForFunction(state => [...document.querySelectorAll('svg[role="img"]')].some(el => el.getAttribute('aria-label').startsWith(state)),
        { waiting: 'Waiting on you.', failed: 'Last turn failed.', idle: 'Idle.', working: 'Working.' }[state]);
      assert(await page.evaluate(() => __rim.isConnected && __contextArc.isConnected), 'state transitions retain the drawing');
      if (run === 0) await page.getByRole('button', { name: /Open session sheet/ }).first().screenshot({ path: path.join(output, `${phase}-${theme}-${state}.png`) });
    }
    for (const choice of ['reduced', 'off']) {
      await page.evaluate(choice => { document.documentElement.dataset.motion = choice; }, choice);
      assert.equal(await page.locator('.c6-ring-working').first().evaluate(el => el.getAnimations().length), 0, `${choice}: stationary ring`);
      assert.equal(await page.evaluate(() => getComputedStyle(__rim).strokeDasharray), 'none', `${choice}: keep the static full working rim`);
      if (!baseline) assert.equal(await page.locator('.c6-ring-working').first().evaluate(el => getComputedStyle(el).willChange), 'auto', 'static rings do not reserve a compositor layer');
    }
    assert.deepEqual(f.errors, [], 'no crashes, invalid streams or external requests');
  } finally { await context.close(); }
}

(async () => {
  fs.mkdirSync(output, { recursive: true });
  const { server, origin } = await serve();
  const browser = await launchBrowser(playwright, 'chromium');
  try {
    for (const theme of (process.env.THEME || 'light,dark').split(',')) {
      for (let runIndex = 0; runIndex < Number(process.env.RUNS || 3); runIndex++) await run(browser, origin, theme, runIndex);
    }
  } finally { await browser.close(); await new Promise(resolve => server.close(resolve)); }
})().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
