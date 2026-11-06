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
// LABEL is deliberately caller-owned: use distinct labels for scale 1/2 runs.
const scale = Number(process.env.SCALE || 1);
assert([1, 2].includes(scale), 'SCALE must be 1 or 2');
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
  const result = {};
  for (const [name, selector] of [['spin', '.c6-ring-working'], ['sweep', '.c6-ring-working .c6-ring-sweep']]) {
    const { nodeId } = await cdp.send('DOM.querySelector', { nodeId: root.nodeId, selector });
    if (!nodeId) continue; // The original/fixed baseline has no sweep box.
    const { node } = await cdp.send('DOM.describeNode', { nodeId });
    const matching = tree.filter(layer => layer.backendNodeId === node.backendNodeId);
    result[name] = { backendNodeId: node.backendNodeId, layers: [] };
    for (const layer of matching) result[name].layers.push({ ...layer, reasons: await cdp.send('LayerTree.compositingReasons', { layerId: layer.layerId }) });
  }
  // Paint.nodeId uses backend IDs, not DOM.querySelector's frontend IDs.
  // Include the exact 24px ring parent: the original inner-SVG dash morph
  // paints that SPAN, not necessarily the circle. Exclude broader ancestors
  // such as the button, tile and document, which also contain unrelated work.
  const parent = await cdp.send('Runtime.evaluate', { expression: '__rim.closest("span")' });
  const { node: subtree } = await cdp.send('DOM.describeNode', { objectId: parent.result.objectId, depth: -1 });
  result.paintNodes = [];
  const visit = node => {
    if (node.nodeType === 1) result.paintNodes.push({ id: node.backendNodeId, name: node.nodeName });
    for (const child of node.children || []) visit(child);
  };
  visit(subtree);
  await cdp.send('Runtime.releaseObject', { objectId: parent.result.objectId });
  await cdp.send('LayerTree.disable');
  return result;
}

function ringPaints(file, paintNodes) {
  const events = JSON.parse(fs.readFileSync(file, 'utf8')).traceEvents;
  const start = events.find(e => e.name === 'q3-start');
  const end = events.find(e => e.name === 'q3-end' && e.pid === start?.pid && e.ts > start.ts);
  assert(start && end, 'trace contains the natural q3-start/end window');
  const ids = new Set(paintNodes.map(node => node.id));
  const paints = events.filter(e => e.name === 'Paint' && e.pid === start.pid && e.ts >= start.ts && e.ts <= end.ts);
  const ring = paints.filter(e => ids.has(e.args?.data?.nodeId) || /\bc6-ring-(?:motion|working|sweep|clip|rim)\b/.test(e.args?.data?.nodeName || ''));
  return { start: start.ts, end: end.ts, totalPaints: paints.length, ringPaints: ring.length,
    events: ring.map(e => ({ ts: e.ts, duration: e.dur, ...e.args.data })) };
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
    frames.push({ timestamp: frame.metadata.timestamp * 1000, metadata: frame.metadata, data: frame.data });
    cdp.send('Page.screencastFrameAck', { sessionId: frame.sessionId }).catch(() => {});
  };
  // The rotating box's boundingBox grows/shrinks with its angle. Crop the
  // stationary 24px parent instead, and do all decoding after the challenge.
  const geometry = await page.evaluate(() => {
    const { x, y, width, height } = __rim.closest('span').getBoundingClientRect();
    return { bounds: { x, y, width, height }, viewport: { width: innerWidth, height: innerHeight } };
  });
  assert.equal(geometry.bounds.width, 24, 'stable ring parent width');
  assert.equal(geometry.bounds.height, 24, 'stable ring parent height');
  cdp.on('Page.screencastFrame', capture);
  await cdp.send('Page.startScreencast', { format: 'png', everyNthFrame: 1 });
  const block = await page.evaluate(async () => {
    // Put the breathing arc early in its growth half-cycle. Two frames commit
    // this diagnostic phase change before we block; never seek in q3's window.
    if (__sweepAnimation) __sweepAnimation.currentTime = 100;
    if (__animation) __animation.currentTime = 0;
    await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve)));
    const start = performance.now();
    performance.mark('q3-block-start');
    while (performance.now() - start < 600) { /* Deliberate main-thread challenge, not app code. */ }
    performance.mark('q3-block-end');
    return { start: performance.timeOrigin + start, end: performance.timeOrigin + performance.now() };
  });
  await cdp.send('Page.stopScreencast');
  cdp.off('Page.screencastFrame', capture);
  // Exclude boundary frames, which may still reflect the last pre-block commit.
  const during = frames.filter(frame => frame.timestamp >= block.start + 50 && frame.timestamp <= block.end - 50);
  const decoded = await page.evaluate(async ({ frames, bounds }) => {
    const result = [];
    for (const frame of frames) {
      const image = new Image();
      image.src = `data:image/png;base64,${frame.data}`;
      await image.decode();
      // CDP can return CSS-pixel images even at DSF 2, and its captured viewport
      // may be shorter than innerHeight (e.g. 913 vs 1000). Use the image's own
      // DIP viewport metadata, not devicePixelRatio or the layout height.
      const sx = image.width / frame.metadata.deviceWidth, sy = image.height / frame.metadata.deviceHeight;
      const zoom = frame.metadata.pageScaleFactor;
      const canvas = document.createElement('canvas');
      canvas.width = Math.round(bounds.width * sx);
      canvas.height = Math.round(bounds.height * sy);
      const ctx = canvas.getContext('2d', { willReadFrequently: true });
      ctx.drawImage(image, bounds.x * zoom * sx, (bounds.y * zoom + frame.metadata.offsetTop) * sy,
        bounds.width * zoom * sx, bounds.height * zoom * sy, 0, 0, canvas.width, canvas.height);
      const pixels = ctx.getImageData(0, 0, canvas.width, canvas.height).data;
      let area = 0, x = 0, y = 0;
      for (let i = 0; i < pixels.length; i += 4) {
        const [r, g, b] = pixels.subarray(i, i + 3);
        if (b > r + 45 && b > g + 25 && g > r + 20) {
          area++;
          x += (i / 4) % canvas.width + .5;
          y += Math.floor(i / 4 / canvas.width) + .5;
        }
      }
      const centroid = area ? { x: x / area / sx - bounds.width / 2, y: y / area / sy - bounds.height / 2 } : null;
      result.push({ timestamp: frame.timestamp, viewport: frame.metadata, imageScale: { x: sx, y: sy }, width: canvas.width, height: canvas.height,
        bluePixels: area, blueArea: area / (sx * sy), centroid,
        angle: centroid ? Math.atan2(centroid.y, centroid.x) * 180 / Math.PI : null, png: canvas.toDataURL('image/png').split(',')[1] });
    }
    return result;
  }, { frames: during, bounds: geometry.bounds });
  let previous, angle = 0;
  const stats = decoded.map((frame, i) => {
    const { png, ...stat } = frame;
    fs.writeFileSync(path.join(output, `${name}-blocked-${i}.png`), Buffer.from(png, 'base64'));
    if (frame.angle !== null) {
      angle += previous === undefined ? frame.angle : ((frame.angle - previous + 540) % 360) - 180;
      previous = frame.angle;
    }
    return { ...stat, unwrappedAngle: frame.angle === null ? null : angle };
  });
  const third = Math.max(1, Math.floor(stats.length / 3));
  const meanArea = rows => rows.reduce((n, row) => n + row.blueArea, 0) / rows.length;
  const lengthRatio = meanArea(stats.slice(-third)) / meanArea(stats.slice(0, third));
  // The centroid advances with spin AND half the changing arc angle; its
  // displacement is not a measurement of pure wrapper rotation.
  const centroidDegrees = stats.at(-1)?.unwrappedAngle - stats[0]?.unwrappedAngle;
  const result = { ...block, ...geometry, scale, captured: during.map(frame => frame.timestamp), frames: stats, lengthRatio, centroidDegrees };
  fs.writeFileSync(path.join(output, `${name}-blocked.json`), JSON.stringify(result, null, 2));
  return result;
}

async function visualFrames(page, name, states) {
  // Real production drawing beside the original dashed-circle animation.
  // Pause independent animations at equal times: test easing, both extrema,
  // growth/shrink, and cycle wrap at small sizes and fractional pixel positions.
  const boxes = await page.evaluate(states => {
    const original = __rim.closest('span'), gallery = document.createElement('div');
    Object.assign(gallery.style, { position: 'fixed', inset: '0', zIndex: '9999',
      background: getComputedStyle(document.body).backgroundColor, padding: '24px', overflow: 'auto' });
    gallery.id = 'q3-ring-gallery'; document.body.append(gallery);
    const heading = document.createElement('h1');
    heading.textContent = 'Full motion: candidate / original dashed reference'; gallery.append(heading);
    const caption = document.createElement('p');
    caption.textContent = 'Columns: 0 / 350 / 700 / 1050 / 1400 / 1750 / 2100 / 2450 / 2800ms. Each pair: candidate left, original right.';
    gallery.append(caption);
    const boxes = [];
    for (const size of [24, 20, 16]) for (const fraction of [0, .25, .5]) {
      const row = document.createElement('div');
      Object.assign(row.style, { display: 'flex', alignItems: 'center', height: '56px' }); gallery.append(row);
      const label = document.createElement('span'); label.textContent = `${size}px / offset ${fraction}px`;
      label.style.width = '190px'; row.append(label);
      for (const time of [0, 350, 700, 1050, 1400, 1750, 2100, 2450, 2800]) {
        const pair = document.createElement('div');
        Object.assign(pair.style, { position: 'relative', width: '104px', height: '48px' }); row.append(pair);
        const candidate = original.cloneNode(true);
        Object.assign(candidate.style, { position: 'absolute', left: `${fraction}px`, top: `${8 + fraction}px`,
          transformOrigin: 'top left', transform: `scale(${size / 24})` }); pair.append(candidate);
        const spin = candidate.querySelector('.c6-ring-working'), sweep = candidate.querySelector('.c6-ring-sweep');
        for (const el of [spin, sweep]) for (const a of el.getAnimations()) { a.pause(); a.currentTime = time; }
        const matrix = new DOMMatrix(getComputedStyle(sweep).transform);
        const arcDegrees = Math.atan2(matrix.b, matrix.a) * 180 / Math.PI + 180, length = arcDegrees * Math.PI / 180 * 10.5;
        const reference = candidate.cloneNode(true); reference.style.left = `${48 + fraction}px`; pair.append(reference);
        const refSpin = reference.querySelector('.c6-ring-working');
        const svg = document.createElementNS('http://www.w3.org/2000/svg', 'svg');
        for (const [key, value] of Object.entries({ width: 24, height: 24, viewBox: '0 0 24 24' })) svg.setAttribute(key, value);
        const circle = document.createElementNS(svg.namespaceURI, 'circle');
        for (const [key, value] of Object.entries({ cx: 12, cy: 12, r: 10.5, fill: 'none', 'stroke-width': 1.5,
          stroke: getComputedStyle(sweep.querySelector('path')).stroke })) circle.setAttribute(key, value);
        svg.append(circle); refSpin.replaceChildren(svg);
        const old = circle.animate([
          { strokeDasharray: '8 58', offset: 0, easing: 'ease-in-out' },
          { strokeDasharray: '30 36', offset: .5, easing: 'ease-in-out' },
          { strokeDasharray: '8 58', offset: 1 },
        ], { duration: 2800, iterations: Infinity });
        old.pause(); old.currentTime = time;
        const referenceLength = parseFloat(getComputedStyle(circle).strokeDasharray);
        for (const a of refSpin.getAnimations()) { a.pause(); a.currentTime = time; }
        const rect = el => { const r = el.getBoundingClientRect(); return { x: r.x, y: r.y, width: r.width, height: r.height }; };
        boxes.push({ size, fraction, time, arcDegrees, length, referenceLength, candidate: rect(candidate), reference: rect(reference) });
      }
    }
    for (const { state, html } of states) {
      const row = document.createElement('div');
      Object.assign(row.style, { display: 'flex', alignItems: 'center', height: '40px', gap: '24px' }); gallery.append(row);
      const label = document.createElement('span'); label.style.width = '166px'; label.textContent = `${state}: 24 / 20 / 16px`; row.append(label);
      for (const size of [24, 20, 16]) {
        const holder = document.createElement('span'); holder.innerHTML = html; row.append(holder);
        holder.firstElementChild.style.transform = `scale(${size / 24})`;
        for (const a of holder.getAnimations({ subtree: true })) { a.pause(); a.currentTime = 700; }
      }
    }
    return boxes;
  }, states);
  try {
    const png = await page.screenshot({ path: path.join(output, `${name}-visual-full.png`) });
    const measurements = await page.evaluate(async ({ src, boxes, scale }) => {
      const image = new Image(); image.src = src; await image.decode();
      const canvas = document.createElement('canvas'); canvas.width = image.width; canvas.height = image.height;
      const ctx = canvas.getContext('2d', { willReadFrequently: true }); ctx.drawImage(image, 0, 0);
      const pixels = ctx.getImageData(0, 0, canvas.width, canvas.height).data;
      return boxes.map(box => {
        const samples = rect => {
          let count = 0, ink = 0; const angles = [];
          for (let y = Math.floor(rect.y * scale); y < Math.ceil((rect.y + rect.height) * scale); y++) {
            for (let x = Math.floor(rect.x * scale); x < Math.ceil((rect.x + rect.width) * scale); x++) {
              const i = (y * canvas.width + x) * 4;
              const dx = (x + .5) / scale - rect.x - rect.width / 2, dy = (y + .5) / scale - rect.y - rect.height / 2;
              // Weight blue ink, not just thresholded pixels: at 16px a single
              // antialiased endpoint pixel is a large fraction of the arc.
              if (pixels[i + 2] - pixels[i] > 20 && pixels[i + 2] - pixels[i + 1] > 10) ink += pixels[i + 2] - pixels[i];
              if (pixels[i + 2] - pixels[i] > 50 && pixels[i + 2] - pixels[i + 1] > 30) {
                count++; angles.push((Math.atan2(dy, dx) * 180 / Math.PI - box.time / 1400 * 360 + 1080) % 360);
              }
            }
          }
          return { count, ink, angles };
        };
        const candidate = samples(box.candidate), reference = samples(box.reference);
        // Two physical pixels' footprint at the butt endpoints is allowed;
        // every interior angular bin must contain blue, with no stray piece.
        const tolerance = 2 / (box.size * scale * 10.5 / 24) * 180 / Math.PI;
        const strayPixels = candidate.angles.filter(a => a > box.arcDegrees + tolerance && a < 360 - tolerance).length;
        const bins = Math.max(1, Math.floor((box.arcDegrees - tolerance * 2) / tolerance)), missing = [];
        for (let i = 0; i < bins; i++) {
          const from = tolerance + i * (box.arcDegrees - tolerance * 2) / bins;
          const to = tolerance + (i + 1) * (box.arcDegrees - tolerance * 2) / bins;
          if (!candidate.angles.some(a => a >= from && a <= to)) missing.push([from, to]);
        }
        return { ...box, candidatePixels: candidate.count, referencePixels: reference.count,
          inkRatio: candidate.ink / reference.ink, strayPixels, missing };
      });
    }, { src: 'data:image/png;base64,' + png.toString('base64'), boxes, scale });
    fs.writeFileSync(path.join(output, `${name}-visual.json`), JSON.stringify(measurements, null, 2));
    for (const m of measurements) {
      assert(Math.abs(m.length - m.referenceLength) < .001, `original easing/arc length: ${JSON.stringify(m)}`);
      assert(m.inkRatio > .85 && m.inkRatio < 1.15, `reference raster ink: ${JSON.stringify(m)}`);
      assert.equal(m.strayPixels, 0, `no detached piece: ${JSON.stringify(m)}`);
      assert.deepEqual(m.missing, [], `no interior seam: ${JSON.stringify(m)}`);
    }
    for (const choice of ['reduced', 'off']) {
      await page.evaluate(choice => {
        document.documentElement.dataset.motion = choice;
        document.querySelector('#q3-ring-gallery h1').textContent = `${choice}: stationary rims, all states and sizes`;
      }, choice);
      await page.screenshot({ path: path.join(output, `${name}-visual-${choice}.png`) });
    }
  } finally {
    await page.evaluate(() => { document.querySelector('#q3-ring-gallery').remove(); document.documentElement.dataset.motion = 'full'; });
  }
}

async function run(browser, origin, theme, run) {
  const motion = { choice: 'full', os: 'no-preference', effective: 'full' };
  const events = Array.from({ length: 30 }, (_, i) => ({ seq: i + 66, ts: i + 66, type: 'assistant', turn: i + 66,
    blocks: [{ type: 'text', text: prose(i + 1000, 12) }], stop_reason: 'end_turn' }));
  const f = await fixture(browser, origin, { width: 1600, height: 1000 }, theme, motion, () => {}, { deviceScaleFactor: scale }, { events });
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
    await page.evaluate(baseline => {
      window.__spin = document.querySelector('.c6-ring-working');
      window.__animation = __spin.getAnimations().find(a => a.animationName === 'spin');
      window.__sweep = __spin.querySelector('.c6-ring-sweep');
      window.__clip = __spin.querySelector('.c6-ring-clip');
      window.__sweepAnimation = __sweep?.getAnimations().find(a => a.animationName === 'c6-ring-sweep');
      window.assertRing = () => __rim.isConnected && __contextArc.isConnected && __spin.isConnected &&
        __spin === document.querySelector('.c6-ring-working') && !!__animation && __spin.getAnimations().includes(__animation) &&
        (baseline || (__clip?.isConnected && __sweep?.isConnected && __clip === __spin.querySelector('.c6-ring-clip') &&
          __sweep === __spin.querySelector('.c6-ring-sweep') && !!__sweepAnimation && __sweep.getAnimations().includes(__sweepAnimation)));
    }, baseline);
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
    const shape = await page.evaluate(() => {
      const animations = el => el ? el.getAnimations().map(a => ({ name: a.animationName,
        duration: a.effect.getTiming().duration, frames: a.effect.getKeyframes() })) : [];
      const size = el => el && ['width', 'height', 'left', 'bottom'].map(key => getComputedStyle(el)[key]);
      return { tag: __spin.tagName, animations: animations(__spin), sweepTag: __sweep?.tagName, sweepAnimations: animations(__sweep),
        clipTag: __clip?.tagName, clipOverflow: __clip && getComputedStyle(__clip).overflow,
        clipSize: size(__clip), sweepSize: size(__sweep),
        sweepParentIsClip: !!__sweep && __sweep.parentElement === __clip, clipParentIsSpin: !!__clip && __clip.parentElement === __spin,
        sweepIsWholeSvg: !!__sweep && __sweep.getAttribute('viewBox') === '0 0 24 24' && !__sweep.querySelector('svg'),
        contextAnimations: __contextArc.getAnimations().length, rimAnimations: __rim.getAnimations().length,
        contextRotates: !!__contextArc.closest('.c6-ring-working, .c6-ring-sweep'),
        contextTransforms: [__contextArc, __contextArc.closest('svg'), __rim.closest('span')].map(el => getComputedStyle(el).transform),
        rimDash: getComputedStyle(__rim).strokeDasharray };
    });
    if (!baseline) {
      assert.equal(shape.tag, 'SPAN', 'rotate the overlay box, not an inner SVG element');
      assert.equal(shape.sweepTag.toLowerCase(), 'svg', 'sweep rotates a whole outer SVG');
      assert.equal(shape.clipTag, 'SPAN', 'stationary lower-half clip inside the spinning box');
      assert.equal(shape.clipOverflow, 'hidden');
      assert.deepEqual(shape.clipSize, ['24px', '12px', '0px', '0px'], 'lower-half clip geometry');
      assert.deepEqual(shape.sweepSize, ['24px', '24px', '0px', '0px'], 'full SVG centered above the clip bottom');
      assert(shape.sweepParentIsClip && shape.clipParentIsSpin && shape.sweepIsWholeSvg, 'spin > clip > whole sweep SVG');
      for (const [name, animations, duration] of [['spin', shape.animations, 1400], ['c6-ring-sweep', shape.sweepAnimations, 2800]]) {
        assert.equal(animations.length, 1, `${name}: exactly one animation`);
        assert.equal(animations[0].name, name);
        assert.equal(animations[0].duration, duration, `${name}: keep the period`);
        for (const frame of animations[0].frames) {
          assert.deepEqual(Object.keys(frame).filter(key => !['offset', 'computedOffset', 'easing', 'composite'].includes(key)), ['transform'],
            `${name}: transform-only keyframes, no paint-bound arc morph`);
        }
      }
      assert.deepEqual(shape.sweepAnimations[0].frames.map(frame => frame.transform),
        ['rotate(-136.35deg)', 'rotate(-16.3deg)', 'rotate(-136.35deg)'], 'breathing sweep endpoints');
      assert.deepEqual(shape.sweepAnimations[0].frames.map(frame => [frame.offset, frame.easing]),
        [[0, 'ease-in-out'], [.5, 'ease-in-out'], [1, 'ease-in-out']], 'original per-half-cycle easing');
      assert(shape.animations[0].frames.every(frame => frame.easing === 'linear'), 'linear spin');
      assert.equal(shape.rimAnimations, 0, 'no animation on an inner SVG element');
      assert.equal(shape.contextAnimations, 0, 'context does not spin');
      assert.equal(shape.contextRotates, false, 'the context is not inside either moving box');
      // The context circle's fixed SVG rotate(-90) is intentional; its outer
      // SVG and the stable parent must have no CSS transform at all.
      assert.deepEqual(shape.contextTransforms.slice(1), ['none', 'none'], 'stationary context SVG and ring parent');
      for (const name of ['spin', 'sweep']) {
        assert(compositor[name]?.layers.some(layer => layer.reasons.compositingReasons.some(r => /active.*transform animation/i.test(r))),
          `${name} has its own active-transform compositor layer: ${JSON.stringify(compositor[name])}`);
      }
      assert.notEqual(compositor.spin.backendNodeId, compositor.sweep.backendNodeId, 'independent moving nodes');
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
      const snapshots = await page.evaluate(() => __q3.snapshots);
      await f.setState('working', { last_seq: 100 + i, updated_at: 100 + i });
      await page.waitForFunction(count => __q3.snapshots > count, snapshots);
      retainedChecks.push(await page.evaluate(() => assertRing()));
    }
    await page.evaluate(() => { __q3.end = performance.now(); __q3.measuring = false; performance.mark('q3-end'); });
    let blocked, paint;
    if (run === 0) {
      const { profile } = await cdp.send('Profiler.stop');
      fs.writeFileSync(path.join(output, `${phase}-${theme}-${run}.cpuprofile`), JSON.stringify(profile));
      blocked = await blockedFrames(cdp, page, `${phase}-${theme}`);
      const traceFile = path.join(output, `${phase}-${theme}-trace.json`);
      await traceEnd(cdp, traceFile);
      paint = ringPaints(traceFile, compositor.paintNodes);
      fs.writeFileSync(path.join(output, `${phase}-${theme}-paints.json`), JSON.stringify(paint, null, 2));
    }
    const data = await page.evaluate(() => ({ ...__q3, nodes: document.querySelectorAll('*').length }));
    data.tasks = data.tasks.filter(t => t.start >= data.start && t.start <= data.end);
    const sorted = data.frames.map(f => f.gap).sort((a, b) => a - b);
    const result = { phase, theme, run, scale, chromium: browser.version(), cpu: Number(process.env.CPU || 4), paragraphs,
      compositor, shape, retainedChecks, blocked, paint, ...data, summary: { elapsed: data.end - data.start, frames: sorted.length,
        missedRafSlots: data.frames.reduce((n, f) => n + Math.max(0, Math.round(f.gap / (1000 / 60)) - 1), 0),
        rafP95: sorted[Math.floor(sorted.length * .95)], rafMax: sorted.at(-1), longTasks: data.tasks.length,
        longTaskMs: data.tasks.reduce((n, t) => n + t.duration, 0), renderMs: data.renders.map(r => r.dom - r.start),
        renderFrameMs: data.renders.map(r => r.frame - r.start) } };
    fs.writeFileSync(path.join(output, `${phase}-${theme}-${run}.json`), JSON.stringify(result, null, 2));
    console.log(JSON.stringify({ phase, theme, run, ...result.summary }));
    if (!baseline) {
      assert(retainedChecks.every(Boolean), 'idle -> working, new items and live snapshots retain BOTH moving nodes and animation identities');
      if (paint) assert.equal(paint.ringPaints, 0, `no ring-specific Paint in the natural Q3 window: ${JSON.stringify(paint.events)}`);
      if (blocked) {
        assert(blocked.frames.length >= 3, `at least three interior compositor frames during the busy main thread (got ${blocked.frames.length})`);
        assert(blocked.frames.every(frame => frame.bluePixels > 0), 'every interior frame contains the blue arc');
        assert(blocked.centroidDegrees > 60, `blue centroid keeps moving during the block (${blocked.centroidDegrees.toFixed(1)}deg)`);
        assert(blocked.lengthRatio > 1.25, `blue area grows independently of rotation during the block (${blocked.lengthRatio.toFixed(2)}x)`);
      }
    }
    await page.screenshot({ path: path.join(output, `${phase}-${theme}-${run}.png`) });
    // No benchmark threshold here. Correctness covers all states/preferences.
    const states = [];
    for (const state of ['waiting', 'failed', 'idle', 'working']) {
      await f.setState(state, { status: state === 'failed' ? 'failed' : state === 'working' ? 'running' : 'idle' });
      await page.waitForFunction(state => __contextArc.closest('svg').getAttribute('aria-label').startsWith(state),
        { waiting: 'Waiting on you.', failed: 'Last turn failed.', idle: 'Idle.', working: 'Working.' }[state]);
      assert(await page.evaluate(() => __rim.isConnected && __contextArc.isConnected), 'state transitions retain the drawing');
      states.push({ state, html: await page.evaluate(() => __rim.closest('span').outerHTML) });
      if (run === 0) await page.getByRole('button', { name: /Open session sheet/ }).first().screenshot({ path: path.join(output, `${phase}-${theme}-${state}.png`) });
    }
    for (const choice of ['reduced', 'off']) {
      await page.evaluate(choice => { document.documentElement.dataset.motion = choice; }, choice);
      assert.equal(await page.locator('.c6-ring-working').first().evaluate(el => el.getAnimations().length), 0, `${choice}: stationary ring`);
      assert.equal(await page.evaluate(() => getComputedStyle(__rim).strokeDasharray), 'none', `${choice}: keep the static full working rim`);
      if (!baseline) {
        const staticRing = await page.locator('.c6-ring-working').first().evaluate(el => {
          const sweep = el.querySelector('.c6-ring-sweep');
          return { overlayDisplay: getComputedStyle(el).display, spinHint: getComputedStyle(el).willChange, spinName: getComputedStyle(el).animationName,
            sweepHint: getComputedStyle(sweep).willChange, sweepAnimations: sweep.getAnimations().length,
            sweepName: getComputedStyle(sweep).animationName,
            rimVisibility: getComputedStyle(__rim).visibility };
        });
        assert.deepEqual(staticRing, { overlayDisplay: 'none', spinHint: 'auto', spinName: 'none', sweepHint: 'auto', sweepAnimations: 0,
          sweepName: 'none', rimVisibility: 'visible' },
          `${choice}: hide the overlay, remove BOTH animations/hints, reveal the full rim`);
      }
    }
    await page.evaluate(() => { document.documentElement.dataset.motion = 'full'; });
    if (!baseline) {
      assert.deepEqual(await page.locator('.c6-ring-working').first().evaluate(el => ({ display: getComputedStyle(el).display,
        spin: el.getAnimations().length, sweep: el.querySelector('.c6-ring-sweep').getAnimations().length, rim: getComputedStyle(__rim).visibility })),
      { display: 'block', spin: 1, sweep: 1, rim: 'hidden' }, 'restore Full after static preferences');
    }
    if (!baseline && run === 0) await visualFrames(page, `${phase}-${theme}`, states);
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
