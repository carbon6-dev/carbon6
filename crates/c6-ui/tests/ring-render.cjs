#!/usr/bin/env node
'use strict';

// Production UI, private fake head. BASELINE=1 records the old inner-SVG
// animation without asserting the new compositor invariant. Timings are evidence,
// never pass/fail thresholds: shared machines do not have a fixed frame budget.
// PHONE=1 instead checks 320/360/390/430px, light/dark, Full/Reduced/Off,
// DSF 1/2, with screenshots/JSON in /tmp/q11-ring/row-link. THEME, MOTION, WIDTHS,
// SCALE and EVIDENCE can narrow/label a diagnostic run; ROOT selects the build.
// Both modes are Chromium-only: layers() requires CDP. CHROMIUM selects a
// browser binary (133 by default); this is not a WebKit/Firefox touch proof.
const assert = require('node:assert/strict');
const fs = require('node:fs');
const path = require('node:path');
const { serve, fixture, settle, launchBrowser } = require('./motion.cjs');
const phone = process.env.PHONE === '1';
const output = process.env.EVIDENCE || (phone ? '/tmp/q11-ring/row-link' : '/tmp/q3-ring');
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
    // Wording is correctness-only: never add these events to the measured
    // workload, CPU profile, natural trace window or compositor challenge.
    if (!baseline) await desktopTaskWords(f, `${phase}-${theme}-${run}`);
    assert.deepEqual(f.errors, [], 'no crashes, invalid streams or external requests');
  } finally { await context.close(); }
}

async function desktopTaskWords(f, name) {
  const { page } = f;
  const button = page.locator('button[title="Open the background tasks"]:visible');
  const cases = [
    ['task1', true, 'started', '1 background task'],
    ['task2', true, 'started', '2 background tasks'],
    ['server1', false, 'started', '2 background tasks · 1 server'],
    ['server2', false, 'started', '2 background tasks · 2 servers'],
    ['task1', true, 'finished', '1 background task · 2 servers'],
    ['task2', true, 'finished', '2 servers'],
    ['server2', false, 'finished', '1 server'],
  ];
  for (const [i, [id, notify, state, words]] of cases.entries()) {
    f.send({ type: 'background_task', task_id: `q11-desktop-${id}`, kind: 'shell', description: `Q11 desktop ${id}`,
      state, notify, exit_code: 0, elapsed_ms: 0, native_type: '', foreground: false });
    await page.waitForFunction(words => [...document.querySelectorAll('button[title="Open the background tasks"]')].some(el =>
      el.getBoundingClientRect().width && el.textContent.replace(/\s+/g, ' ').trim() === words), words);
    await settle(page);
    assert.equal((await button.textContent()).replace(/\s+/g, ' ').trim(), words, 'desktop task/server wording has no redundant running');
    if (!i) await button.evaluate(el => {
      window.__desktopTasks = { button: el, spinner: el.querySelector('.animate-spin'), chevron: el.querySelector(':scope > svg') };
    });
    assert(await button.evaluate(el => {
      const p = __desktopTasks;
      return p.button === el && p.spinner?.isConnected && p.chevron?.isConnected &&
        p.spinner === el.querySelector('.animate-spin') && p.chevron === el.querySelector(':scope > svg') &&
        el.querySelectorAll('svg').length === 2 && p.spinner.getAnimations().some(a => a.animationName === 'spin') &&
        !!p.chevron.querySelector('path[d="m9 18 6-6-6-6"]');
    }), 'desktop wording updates retain the drawn spinner and chevron');
    await button.scrollIntoViewIfNeeded();
    await page.screenshot({ path: path.join(output, `${name}-desktop-tasks-${i + 1}.png`), animations: 'disabled' });
  }
  f.send({ type: 'background_task', task_id: 'q11-desktop-server1', kind: 'shell', description: 'Q11 desktop server1',
    state: 'finished', notify: false });
  await button.waitFor({ state: 'detached' });
}

// Focused phone regression; reuse the real production page, WS fixtures and CDP
// layer probe, not the desktop transcript benchmark or a synthetic badge DOM.
const backgroundWords = count => `${count} background task${count === 1 ? '' : 's'}`;

async function phoneStatusRow(f, { state, task, setWaiting, shot, genuineCards }) {
  const { page } = f, row = page.locator('[data-phone-status]');
  const debug = step => { if (process.env.DEBUG_FIXTURE) console.log(`  ROW ${step}`); };
  const opener = row.getByRole('button', { name: /Open session status details$/ });
  const dialog = page.getByRole('dialog', { name: 'Session status', exact: true });
  const composer = page.locator('textarea').first();
  const variants = [
    { name: 'background', agent: 'idle', background: 2, labels: [backgroundWords(2)] },
    { name: 'working', agent: 'working', background: 0, labels: ['Working…'] },
    { name: 'working-background', agent: 'working', background: 2, labels: ['Working…', backgroundWords(2)] },
    { name: 'waiting', agent: 'waiting', background: 0, labels: ['Waiting on you 1'] },
    { name: 'waiting-background', agent: 'waiting', background: 2, labels: ['Waiting on you 1', backgroundWords(2)] },
    { name: 'failed', agent: 'failed', background: 0, labels: ['Last turn failed'] },
    { name: 'failed-background', agent: 'failed', background: 2, labels: ['Last turn failed', backgroundWords(2)] },
  ];
  const set = async variant => {
    debug(`variant ${variant.name || variant.agent}`);
    // Keep a visible reason alive while independent WS/API sources converge.
    // Removing the last task before starting the next primary would genuinely
    // create a quiet intermediate state, not a row-layout regression.
    for (let i = 1; i <= variant.background; i++) task(i, 'started');
    setWaiting(variant.agent === 'waiting' ? [{ key: 'q11-row-approval', kind: 'approval', title: 'Q11 row fixture',
      at: 1, session_id: 'parent', call_id: 'q11-row-approval' }] : []);
    f.send({ type: 'turn_ended', turn: 950, reason: ['failed', 'working'].includes(variant.agent) ? 'error' : 'completed' });
    // Waiting must win even while running; Working must beat a saved error.
    // A failed turn in an idle session keeps the composer usable. The separate
    // failed-session rim case already covers status=failed (disabled composer).
    await state(variant.agent === 'waiting' ? 'working' : variant.agent,
      variant.agent === 'waiting' ? { awaiting_approval: true } : {});
    if (variant.agent !== 'idle') await row.getByRole('status').getByText(variant.labels[0], { exact: true }).waitFor();
    for (let i = variant.background + 1; i <= 2; i++) task(i, 'finished');
    if (variant.labels.length) {
      for (const text of variant.labels) await row.getByRole('status').getByText(text, { exact: true }).waitFor();
      await page.waitForFunction(expected => {
        const status = document.querySelector('[data-phone-status] [role="status"]');
        return status && status.textContent.replace(/\s+/g, '') === expected.join('').replace(/\s+/g, '');
      }, variant.labels);
    } else await page.waitForFunction(() => !document.querySelector('[data-phone-status]')?.closest('[data-open="true"]'));
    await settle(page);
  };
  const inspect = async labels => {
    assert.equal(await row.locator('button').count(), 1, 'one native full-row opener; no nested/leftover Show buttons');
    const details = await row.evaluate((el, labels) => {
      const button = el.querySelector('button'), status = el.querySelector('[role="status"]');
      const box = window.__phone.box, r = box(el), b = box(button), s = box(status);
      const texts = [...status.querySelectorAll('*')].filter(node => labels.includes(node.textContent.trim()) &&
        ![...node.children].some(child => child.textContent.trim() === node.textContent.trim()));
      const textBoxes = texts.map(node => { const range = document.createRange(); range.selectNodeContents(node); return box(range); });
      const chevron = button.querySelector('svg');
      return { row: r, button: b, status: s, labels: status.textContent.replace(/\s+/g, ' ').trim(),
        name: button.getAttribute('aria-label'), tag: button.tagName, type: button.type,
        separateStatus: !button.contains(status), interactiveStatus: !!status.querySelector('button,a,input,[role="button"]'),
        overflow: status.scrollWidth > status.clientWidth || status.scrollHeight > status.clientHeight,
        scrollSize: { width: status.scrollWidth, height: status.scrollHeight, clientWidth: status.clientWidth, clientHeight: status.clientHeight },
        textOverflow: texts.some(node => node.scrollWidth > node.clientWidth || node.scrollHeight > node.clientHeight),
        textBoxes, lineCount: texts.length, icons: status.querySelectorAll('svg').length,
        chevrons: button.querySelectorAll('svg').length, chevronHidden: !!chevron?.closest('[aria-hidden="true"]') };
    }, labels);
    assert.equal(details.tag, 'BUTTON'); assert.equal(details.type, 'button');
    assert.equal(details.row.height, 44); assert.equal(details.button.height, 44);
    assert(Math.abs(details.button.width - details.row.width) < .1 && Math.abs(details.button.x - details.row.x) < .1 &&
      Math.abs(details.button.y - details.row.y) < .1, 'full-width 44px overlay, including its border');
    assert(details.separateStatus && !details.interactiveStatus, 'live status is a noninteractive sibling, not nested in the button');
    // A rotating loader's square box extends into empty corners (18px scroll
    // height for a 16px line); its circular ink and the text still fit. Check
    // actual label ranges, not that transformed box's scrollable overflow.
    assert.equal(details.textOverflow, false, `all status wording fits without overflow: ${JSON.stringify(details)}`);
    assert.equal(details.scrollSize.width, details.scrollSize.clientWidth, 'status has no horizontal overflow');
    assert.equal(details.lineCount, labels.length, 'one readable line per primary/background label');
    for (const b of details.textBoxes) assert(b.x >= details.status.x && b.right <= details.status.right && b.y >= details.status.y &&
      b.bottom <= details.status.bottom && b.height <= 16.5, 'full status text fits the live region in 16px lines');
    for (const text of labels) assert(details.name.includes(text), 'accessible opener name includes every current status');
    assert(details.name.endsWith('. Open session status details'));
    assert.equal(details.icons, labels.length, 'one drawn primary/hourglass SVG per status line');
    assert.equal(details.chevrons, 1); assert(details.chevronHidden, 'decorative drawn chevron is aria-hidden');
    return details;
  };
  await composer.fill('Q11 retain this draft and focus during every status variant'); await composer.focus();
  await set(variants[0]);
  await composer.evaluate(el => {
    const scroll = [...document.querySelectorAll('div')].filter(node => node.clientHeight > 100 &&
      node.scrollHeight > node.clientHeight + 300 && getComputedStyle(node).overflowY === 'auto')
      .sort((a, b) => b.scrollHeight - a.scrollHeight)[0];
    if (!scroll) throw new Error('real transcript scroll viewport missing');
    window.__row = { composer: el, scroll, samples: [], sampling: false, clicks: 0, openings: 0 };
    __row.position = () => ({ composer: __phone.box(el), viewport: __phone.box(scroll), top: scroll.scrollTop,
      gap: scroll.scrollHeight - scroll.clientHeight - scroll.scrollTop, focused: document.activeElement === el });
    let wasOpen = false;
    __row.observer = new MutationObserver(() => {
      // Reopening during retained exit correctly reuses the same dialog node.
      // Count closed -> open transitions, not allocations of new DOM nodes.
      const open = !!document.querySelector('[role="dialog"][aria-label="Session status"][data-open="true"]');
      if (open && !wasOpen) __row.openings++;
      wasOpen = open;
    });
    __row.observer.observe(document, { subtree: true, childList: true, attributes: true, attributeFilter: ['data-open'] });
    if (!window.__rowClickHookInstalled) {
      window.__rowClickHookInstalled = true;
      document.addEventListener('click', event => {
        if (event.target.closest('[data-phone-status] button')) __row.clicks++;
      });
    }
    __row.sample = () => {
      if (!__row.sampling) return;
      __row.samples.push(__row.position()); requestAnimationFrame(__row.sample);
    };
  });
  debug('initial end-follow');
  await page.waitForFunction(() => Math.abs(__row.position().gap) < 1);
  const layouts = [];
  // Sample every frame across all seven visible variants; the viewport and
  // nonempty composer must stay put, with focus and end-follow preserved.
  await page.evaluate(() => { __row.baseline = __row.position(); __row.sampling = true; requestAnimationFrame(__row.sample); });
  for (const variant of variants) {
    await set(variant); layouts.push({ variant: variant.name, ...await inspect(variant.labels) });
    await shot(`row-${variant.name}`);
  }
  const follow = await page.evaluate(() => { __row.sampling = false; return { baseline: __row.baseline, samples: __row.samples }; });
  assert(follow.samples.length);
  for (const sample of follow.samples) {
    for (const box of ['composer', 'viewport']) for (const key of ['x', 'y', 'width', 'height'])
      assert(Math.abs(sample[box][key] - follow.baseline[box][key]) < 1, `${box} geometry stays fixed through visible variants`);
    assert(sample.focused, 'status variants never take draft focus'); assert(Math.abs(sample.gap) < 1, 'visible variants keep end-follow');
  }
  debug('native PageUp reader');
  await page.evaluate(() => __row.scroll.focus()); await page.keyboard.press('PageUp');
  await page.waitForFunction(() => __row.position().gap > 100); await settle(page);
  await page.waitForFunction(() => {
    const top = __row.scroll.scrollTop; __row.stable = top === __row.top ? (__row.stable || 0) + 1 : 0; __row.top = top;
    return __row.stable >= 2;
  }, null, { polling: 'raf' });
  const readerTop = await page.evaluate(() => __row.scroll.scrollTop);
  await composer.focus();
  for (const variant of variants) {
    await set(variant);
    assert(Math.abs(await page.evaluate(() => __row.scroll.scrollTop) - readerTop) < 1, 'native PageUp reader is not snapped by visible status changes');
  }
  await page.evaluate(() => __row.scroll.focus()); await page.keyboard.press('End');
  await page.waitForFunction(() => Math.abs(__row.position().gap) < 1);
  const activation = async (name, action, keyboard = false) => {
    debug(`activation ${name}`);
    const before = await page.evaluate(() => ({ clicks: __row.clicks, openings: __row.openings }));
    await action(); await dialog.waitFor(); await settle(page);
    assert.equal(await dialog.count(), 1, `${name}: exactly one status dialog`);
    assert.deepEqual(await page.evaluate(() => ({ clicks: __row.clicks, openings: __row.openings })),
      { clicks: before.clicks + 1, openings: before.openings + 1 }, `${name}: one native activation opens once`);
    await page.keyboard.press('Escape'); await dialog.waitFor({ state: 'detached' });
    if (keyboard) assert(await opener.evaluate(el => document.activeElement === el), 'Escape restores the keyboard opener');
  };
  for (const key of ['Enter', 'Space']) {
    await composer.focus();
    let focused = false;
    for (let i = 0; i < 40 && !focused; i++) {
      await page.keyboard.press('Shift+Tab'); focused = await opener.evaluate(el => document.activeElement === el);
    }
    assert(focused, 'full-row opener is reachable by native keyboard traversal');
    const focus = await opener.evaluate(el => ({ visible: el.matches(':focus-visible'), shadow: getComputedStyle(el).boxShadow }));
    assert(focus.visible && /inset/.test(focus.shadow) && /2px/.test(focus.shadow), 'keyboard focus has a visible inset 2px ring');
    await shot(`row-focus-${key.toLowerCase()}`);
    await activation(key, () => page.keyboard.press(key), true);
  }
  for (const target of ['text', 'blank', 'chevron']) await activation(target, async () => {
    const point = await row.evaluate((el, target) => {
      let rect = (target === 'chevron' ? el.querySelector('button svg') : el).getBoundingClientRect();
      if (target === 'text') {
        const walker = document.createTreeWalker(el.querySelector('[role="status"]'), NodeFilter.SHOW_TEXT);
        let text; while ((text = walker.nextNode()) && !text.textContent.trim()) { /* skip whitespace */ }
        const range = document.createRange(); range.selectNodeContents(text); rect = range.getBoundingClientRect();
      }
      const x = target === 'blank' ? rect.right - 45 : rect.x + rect.width / 2;
      const y = target === 'blank' ? rect.top + 2 : rect.y + rect.height / 2;
      return { x, y, hit: document.elementFromPoint(x, y)?.closest('button') === el.querySelector('button') };
    }, target);
    assert(point.hit, `${target} hits the same full-row button`); await page.touchscreen.tap(point.x, point.y);
  });
  await set({ agent: 'idle', background: 0, labels: [] });
  assert(await page.evaluate(() => {
    const el = document.querySelector('[data-phone-status]');
    return !el || (!!el.closest('[inert]') && el.closest('[data-open]').parentElement.getBoundingClientRect().height === 0);
  }), 'quiet immediately yields its height and makes any retained exit inert');
  await row.waitFor({ state: 'detached' });
  assert.equal(await row.count(), 0, 'quiet has no status row');
  await shot('row-quiet');
  const cards = [];
  if (genuineCards) for (const action of ['Allow', 'Deny', 'Send', 'Cancel']) {
    debug(`genuine card ${action}`);
    const approval = ['Allow', 'Deny'].includes(action), id = `q11-${action.toLowerCase()}`, kind = approval ? 'approval' : 'question';
    const event = approval ? { type: kind, call_id: id, state: 'requested', tool: 'bash', input: '{"command":"echo q11-fixture"}', policy: 'ask', message: 'Q11 genuine approval card' } :
      { type: kind, call_id: id, state: 'asked', title: `Q11 genuine ${action} question`, fields: [{ name: 'answer', label: 'Q11 answer', type: 'text', required: true }], answer: null };
    f.send(event); setWaiting([{ key: id, kind, title: `Q11 ${action}`, at: 1, session_id: 'parent', call_id: id }]);
    await state('waiting'); await row.getByText('Waiting on you 1', { exact: true }).waitFor();
    await opener.tap(); await dialog.waitFor();
    await dialog.getByRole('button', { name: 'Show', exact: true }).tap(); await dialog.waitFor({ state: 'detached' });
    const card = page.locator(`#waiting-parent-${id}`); await card.waitFor();
    await page.waitForFunction(id => { const r = document.getElementById(`waiting-parent-${id}`)?.getBoundingClientRect(); return r && r.top >= 44 && r.bottom <= innerHeight; }, id);
    for (const name of approval ? ['Allow', 'Deny'] : ['Send', 'Cancel']) assert(await card.getByRole('button', { name, exact: true }).isEnabled(), 'genuine card actions remain independently actionable');
    if (action === 'Send') await card.locator('input').fill('Q11 concrete answer');
    const request = page.waitForRequest(request => request.method() === 'POST' && request.url().endsWith(`/${approval ? 'approvals' : 'answers'}/${id}`));
    await card.getByRole('button', { name: action, exact: true }).tap(); const sent = await request;
    const payload = sent.postDataJSON();
    assert.deepEqual(payload, approval ? { allow: action === 'Allow' } : { values: { answer: action === 'Send' ? 'Q11 concrete answer' : null }, cancelled: action === 'Cancel' }, 'real card sends the right mocked API action, not a row click');
    const response = await sent.response(); assert(response?.ok(), 'fixture accepts the genuine card request'); await response.finished();
    if (approval) await card.getByText(action === 'Allow' ? 'Approved.' : 'Denied.', { exact: true }).waitFor();
    else await page.waitForFunction(id => [...document.querySelectorAll(`#waiting-parent-${id} button`)].every(button => button.disabled), id);
    cards.push({ action, payload }); await shot(`row-card-${action.toLowerCase()}`);
    f.send({ ...event, state: approval ? (action === 'Allow' ? 'allowed' : 'denied') : (action === 'Send' ? 'answered' : 'cancelled') });
    setWaiting([]); await state('idle');
    await page.waitForFunction(() => !document.querySelector('[data-phone-status]')?.closest('[data-open="true"]')); await settle(page);
  }
  await composer.fill('');
  await page.evaluate(() => { __row.observer.disconnect(); __row.scroll.focus(); }); await page.keyboard.press('End');
  await page.waitForFunction(() => Math.abs(__row.position().gap) < 1);
  return { layouts, follow, readerTop, cards };
}

async function runPhone(browser, origin, theme, choice, deviceScaleFactor) {
  const widths = (process.env.WIDTHS || '320,360,390,430').split(',').map(Number);
  assert(widths.every(width => [320, 360, 390, 430].includes(width)), 'phone WIDTHS: 320,360,390,430');
  const motion = { choice, os: choice === 'full' ? 'no-preference' : 'reduce', effective: choice };
  const title = 'Q11 retained phone session with a deliberately very long title that must truncate without overlapping any controls';
  const tiles = Array.from({ length: 12 }, (_, i) => ({ key: i * 2 + 2,
    columns: [{ key: i * 2 + 3, view: i ? { view: 'sessions' } : { view: 'session', id: 'parent' } }],
    cursor: 0, active: i * 2 + 3, used: i * 2 + 3, back: [], forward: [], stack: 0, minimized: i > 0 }));
  let waiting = [];
  const f = await fixture(browser, origin, { width: widths[0], height: 844 }, theme, motion, () => {},
    { deviceScaleFactor }, { instrumentation: false, layout: { tiles, focus: 0, next: 30, full: false },
      events: [{ seq: 66, ts: 66, type: 'usage', model: 'test', input_tokens: 25600, output_tokens: 0,
        cache_read_tokens: 0, cache_write_tokens: 0, reasoning_tokens: 0, context_window: 128000 }],
      setup: async ({ page, context }) => {
        await page.addInitScript(() => {
          window.__phoneSnapshots = 0;
          const NativeSocket = window.WebSocket;
          window.WebSocket = class extends NativeSocket {
            constructor(...args) {
              super(...args);
              this.addEventListener('message', message => {
                if (JSON.parse(message.data).type === 'sessions') window.__phoneSnapshots++;
              });
            }
          };
        });
        await context.route('**/api/v1/sessions/parent/waiting', route => route.fulfill({ json: waiting }));
      } });
  const { page, context } = f;
  const records = [];
  let label;
  const step = name => { f.phase = `${label || 'phone startup'}: ${name}`; if (process.env.DEBUG_FIXTURE) console.log(`  PHONE ${f.phase}`); };
  const ring = page.getByRole('button', { name: /Open session sheet/ }).first();
  const row = page.locator('[data-phone-status]');
  const state = async (next, patch = {}) => {
    step(`live state ${next}/${patch.status || 'default'}`);
    const snapshots = await page.evaluate(() => __phoneSnapshots);
    await f.setState(next, { title, ...patch });
    await page.waitForFunction(previous => __phoneSnapshots > previous, snapshots);
  };
  const named = async text => page.waitForFunction(text => __phone.ring.getAttribute('aria-label').startsWith(text + '.'), text);
  const shot = async suffix => {
    // Playwright temporarily cancels infinite CSS animations for this capture
    // and restores them afterwards. Never freeze/seek the product during checks.
    await page.screenshot({ path: path.join(output, `${label}-${suffix}.png`), animations: 'disabled' });
  };
  const task = (i, state) => f.send({ type: 'background_task', task_id: `q11-task-${i}`, kind: 'shell',
    description: `Q11 real background fixture ${i}`, state, exit_code: 0, elapsed_ms: 0, notify: false,
    native_type: '', foreground: false });
  try {
    await state('idle');
    step('long title');
    // Minimized Sessions pages also render this title in hidden lists. Follow
    // the visible ring to its own bar rather than waiting on the first match.
    await ring.locator('..').locator('span.truncate').filter({ hasText: title }).waitFor();
    step('retain header');
    await ring.evaluate(el => {
      const bar = el.parentElement, root = el.querySelector('.c6-ring-rim')?.parentElement;
      if (!root || getComputedStyle(root).display === 'none' || !root.getBoundingClientRect().width) {
        throw new Error('PHONE=1 requires the rebuilt shared SessionRing on phones (not the old plain context ring)');
      }
      window.__rim = root.querySelector('circle[r="10.5"]');
      window.__contextArc = root.querySelector('circle[r="7"][pathLength]');
      window.__phone = { ring: el, root, bar, title: bar.querySelector(':scope > div > span.truncate'),
        actions: bar.querySelector('button[aria-label="Actions"]'),
        picker: bar.querySelector('button[aria-label^="Switch sessions and pages"]'),
        menu: bar.querySelector('button[aria-label^="Menu,"]'),
        badge: root.querySelector('.c6-ring-badge'), icons: [...bar.querySelectorAll('svg')] };
      __phone.box = el => { const r = el.getBoundingClientRect(); return { x: r.x, y: r.y, width: r.width, height: r.height, right: r.right, bottom: r.bottom }; };
      __phone.retained = () => [__phone.ring, __phone.root, __phone.bar, __phone.title, __rim, __contextArc, ...__phone.icons].every(el => el?.isConnected) &&
        __phone.icons.length === __phone.bar.querySelectorAll('svg').length &&
        __phone.ring === __phone.bar.querySelector('button[aria-label$="Open session sheet"]');
      // Watch all visible non-ring header elements on every rendered frame,
      // including the menu/ellipsis SVGs and the Sessions picker's own count.
      __phone.watch = () => {
        const bar = __phone.box(__phone.bar);
        __phone.watched = [...__phone.bar.querySelectorAll('*')].filter(el => {
          const r = __phone.box(el);
          // The retained, closed app drawer is also a descendant of the bar.
          // Its offscreen body is not a visible header node.
          return !__phone.ring.contains(el) && !el.closest('[inert]') && r.width && r.height &&
            r.x < bar.right && r.right > bar.x && r.y < bar.bottom && r.bottom > bar.y;
        });
        __phone.boxes = __phone.watched.map(__phone.box); __phone.moved = []; __phone.samples = 0; __phone.sampling = true;
        const sample = () => {
          if (!__phone.sampling) return;
          __phone.samples++;
          __phone.watched.forEach((el, i) => {
            const now = __phone.box(el), was = __phone.boxes[i];
            if (!el.isConnected || ['x', 'y', 'width', 'height'].some(key => Math.abs(now[key] - was[key]) > .1))
              __phone.moved.push({ tag: el.tagName, class: el.getAttribute('class'), was, now });
          });
          requestAnimationFrame(sample);
        };
        requestAnimationFrame(sample);
      };
    });
    await named('Idle');
    assert(await page.evaluate(() => !!__contextArc), 'fixture supplies a real, visible 20% context arc');
    const cdp = await context.newCDPSession(page);
    for (const width of widths) {
      label = `phone-${theme}-${choice}-${width}-dsf${deviceScaleFactor}`;
      f.phase = label;
      await page.setViewportSize({ width, height: 844 });
      await page.waitForFunction(width => innerWidth === width && Math.abs(__phone.bar.getBoundingClientRect().width - width) < 1, width);
      await settle(page);
      const geometry = await page.evaluate(() => {
        const p = __phone, style = getComputedStyle(p.title);
        return { retained: p.retained(), bar: p.box(p.bar), root: p.box(p.root), title: p.box(p.title),
          truncated: p.title.scrollWidth > p.title.clientWidth, overflow: style.overflowX, ellipsis: style.textOverflow,
          targets: [p.menu, p.ring, p.actions, p.picker].map(p.box),
          picker: p.picker.getAttribute('aria-label'), pickerCount: p.picker.querySelector('[data-phone-pages]').textContent,
          contextDash: __contextArc.getAttribute('stroke-dasharray') };
      });
      assert(geometry.retained, `${label}: resizing retains the header and every SVG`);
      assert.equal(geometry.bar.height, 44, 'one 44px phone bar');
      assert.equal(geometry.root.width, 24); assert.equal(geometry.root.height, 24);
      for (const box of geometry.targets) {
        assert.equal(box.width, 44, '44px interactive target width'); assert.equal(box.height, 44, '44px interactive target height');
        // The bar's 1px border can centre its 44px children on a half pixel.
        assert(box.x >= 0 && box.right <= width && box.y >= geometry.bar.y - .6 && box.bottom <= geometry.bar.bottom + .6);
      }
      assert(geometry.truncated && geometry.overflow === 'hidden' && geometry.ellipsis === 'ellipsis', `long title visibly truncates: ${JSON.stringify(geometry)}`);
      assert(geometry.title.right <= geometry.targets[1].x, 'title stays outside ring target');
      assert.equal(geometry.picker, 'Switch sessions and pages, 12 open');
      assert.equal(geometry.pickerCount, '12', 'independent Sessions count remains visible');
      assert.equal(geometry.contextDash, '20 100');
      await page.evaluate(() => __phone.watch());
      const counts = [];
      let current = 0;
      for (const count of [0, 1, 2, 12, 123]) {
        step(`background ${count}`);
        while (current < count) task(++current, 'started');
        await page.waitForFunction(count => {
          const p = __phone, text = count === 0 ? 'No background tasks.' : `${count} background task${count === 1 ? '' : 's'}.`;
          return p.ring.getAttribute('aria-label').includes(text) &&
            p.badge.parentElement.dataset.open === String(count > 0) &&
            (count === 0 || (p.badge.textContent === String(count) && !p.badge.querySelector('.c6-count-out')));
        }, count);
        await settle(page);
        if (count) await row.getByText(backgroundWords(count), { exact: true }).waitFor();
        const badge = await page.evaluate(() => {
          const p = __phone, b = p.box(p.badge), intersects = el => {
            const r = p.box(el); return b.x < r.right && b.right > r.x && b.y < r.bottom && b.bottom > r.y;
          };
          return { box: b, retained: p.retained(), state: p.ring.getAttribute('aria-label'),
            visible: getComputedStyle(p.badge).opacity, contextDash: __contextArc.getAttribute('stroke-dasharray'),
            overlaps: [p.title, p.actions, p.picker, p.picker.querySelector('[data-phone-pages]')].map(intersects),
            hit: document.elementFromPoint(p.root.getBoundingClientRect().x + 12, p.root.getBoundingClientRect().y + 12)?.closest('button') === p.ring };
        });
        assert(badge.retained && badge.hit, 'badge updates retain all SVGs and the ring hit target');
        assert(badge.state.startsWith('Idle.'), 'background count never becomes main-agent Working');
        assert.equal(badge.contextDash, geometry.contextDash, 'background count never changes context');
        assert.equal(badge.visible, count ? '1' : '0', 'positive-only badge');
        if (count) {
          assert.deepEqual(badge.overlaps, [false, false, false, false], 'badge is outside title, ellipsis, Sessions button and its count');
          assert(badge.box.x >= 0 && badge.box.right <= width && badge.box.y >= geometry.bar.y && badge.box.bottom <= geometry.bar.bottom, 'badge is not clipped by the phone bar');
        }
        counts.push({ count, ...badge });
        if (count) {
          const open = row.getByRole('button', { name: /Open session status details$/ });
          assert.equal(await row.locator('button').count(), 1, 'one full-row status opener');
          const rowBox = await row.boundingBox(), buttonBox = await open.boundingBox();
          assert.equal(rowBox.height, 44); assert.equal(buttonBox.height, 44);
          assert(Math.abs(rowBox.width - buttonBox.width) < .1, 'status opener spans the full row');
          assert.equal(await row.locator('svg').count(), 2, 'Background keeps its hourglass plus the drawn chevron');
        }
        await shot(`background-${count}`);
      }
      const movement = await page.evaluate(() => { __phone.sampling = false; return { samples: __phone.samples, moved: __phone.moved }; });
      assert(movement.samples > 0, 'observed rendered frames through all count transitions');
      assert.deepEqual(movement.moved, [], 'no other header node moves while 0 -> 1 -> 2 -> 12 -> 123');
      // A real touch on the visual ring must use the existing sheet action.
      await ring.tap();
      const dialog = page.getByRole('dialog', { name: 'Session details', exact: true });
      await dialog.waitFor(); await settle(page);
      assert.equal(await ring.getAttribute('aria-expanded'), 'true');
      for (const name of ['Changes', 'Terminal', 'Files', 'Outputs']) await dialog.getByRole('button', { name, exact: true }).waitFor();
      await shot('session-sheet');
      await dialog.getByRole('button', { name: 'Close', exact: true }).tap();
      await dialog.waitFor({ state: 'detached' });
      assert.equal(await ring.getAttribute('aria-expanded'), 'false');
      const states = [];
      for (const status of ['starting', 'running']) {
        await state('working', { status }); await named('Working');
        // Starting already lights the shared rim; the unchanged composer row
        // reports Working only once the session is actually running.
        if (status === 'running') await row.getByText('Working…', { exact: true }).waitFor();
        await row.getByText(backgroundWords(123), { exact: true }).waitFor();
        step(`${status} drawing/layers`);
        const drawing = await page.evaluate(() => {
          const p = __phone, spin = p.root.querySelector('.c6-ring-working'), sweep = spin.querySelector('.c6-ring-sweep');
          const animations = el => el.getAnimations().map(a => ({ name: a.animationName, duration: a.effect.getTiming().duration,
            iterations: a.effect.getTiming().iterations, frames: a.effect.getKeyframes() }));
          return { retained: p.retained(), spin: animations(spin), sweep: animations(sweep),
            spinTag: spin.tagName, sweepTag: sweep.tagName.toLowerCase(), clip: getComputedStyle(sweep.parentElement).overflow,
            rimVisibility: getComputedStyle(__rim.closest('svg')).visibility,
            blue: __rim.classList.contains('stroke-blue-500') && sweep.querySelector('path').classList.contains('stroke-blue-500'),
            contextTransforms: [__contextArc.closest('svg'), p.root].map(el => getComputedStyle(el).transform),
            contextAnimations: animations(__contextArc.closest('svg')),
            contextMoves: !!__contextArc.closest('.c6-ring-working, .c6-ring-sweep'),
            svgAnimations: [...p.root.querySelectorAll('svg *')].flatMap(el => animations(el)),
            contextDash: __contextArc.getAttribute('stroke-dasharray') };
        });
        assert(drawing.retained, 'starting/running preserves all header SVGs');
        assert.deepEqual(drawing.contextTransforms, ['none', 'none']); assert.equal(drawing.contextMoves, false);
        assert.equal(drawing.contextDash, geometry.contextDash); assert.deepEqual(drawing.svgAnimations, []);
        assert.deepEqual(drawing.contextAnimations, [], 'context SVG has no animation');
        assert.equal(drawing.spinTag, 'SPAN'); assert.equal(drawing.sweepTag, 'svg'); assert.equal(drawing.clip, 'hidden');
        assert(drawing.blue, 'Working keeps its blue cue in Full, Reduced and Off');
        assert.equal(drawing.rimVisibility, choice === 'full' ? 'hidden' : 'visible');
        let compositor;
        if (choice === 'full') {
          for (const [key, name, duration] of [['spin', 'spin', 1400], ['sweep', 'c6-ring-sweep', 2800]]) {
            assert.equal(drawing[key].length, 1); assert.equal(drawing[key][0].name, name);
            assert.equal(drawing[key][0].duration, duration); assert.equal(drawing[key][0].iterations, Infinity);
            for (const frame of drawing[key][0].frames) assert.deepEqual(Object.keys(frame).filter(key =>
              !['offset', 'computedOffset', 'easing', 'composite'].includes(key)), ['transform'], `${name} animates transform only`);
          }
          assert(drawing.spin[0].frames.every(frame => frame.easing === 'linear'), 'linear 1.4s spin');
          assert.deepEqual(drawing.sweep[0].frames.map(frame => [frame.transform, frame.offset, frame.easing]),
            [['rotate(-136.35deg)', 0, 'ease-in-out'], ['rotate(-16.3deg)', .5, 'ease-in-out'],
              ['rotate(-136.35deg)', 1, 'ease-in-out']], '2.8s breathing keeps its original extrema and half-cycle easing');
          compositor = await layers(cdp, page);
          for (const key of ['spin', 'sweep']) assert(compositor[key]?.layers.some(layer =>
            layer.reasons.compositingReasons.some(reason => /active.*transform animation/i.test(reason))),
          `${label}/${status}: ${key} has its own active-transform compositor layer: ${JSON.stringify(compositor[key])}`);
          assert.notEqual(compositor.spin.backendNodeId, compositor.sweep.backendNodeId);
        } else {
          assert.deepEqual(drawing.spin, [], `${choice} has no spin`); assert.deepEqual(drawing.sweep, [], `${choice} has no breathing`);
        }
        states.push({ status, drawing, compositor });
      }
      await settle(page); await shot('working');
      waiting = [{ key: 'q11-approval', kind: 'approval', title: 'Q11 fixture approval', detail: title,
        at: 1, session_id: 'parent', call_id: 'q11-approval' }];
      await state('waiting'); await named('Waiting on you');
      await row.getByText('Waiting on you 1', { exact: true }).waitFor(); await settle(page);
      const waitingCue = await page.evaluate(() => ({ dash: __rim.getAttribute('stroke-dasharray'), stroke: getComputedStyle(__rim).stroke }));
      assert.equal(waitingCue.dash, '2 2', 'Waiting is a dashed amber rim');
      assert(await page.evaluate(() => __rim.classList.contains('stroke-amber-500')));
      await shot('waiting');
      waiting = [];
      f.send({ type: 'turn_ended', turn: 900, reason: 'error' });
      await state('failed', { status: 'failed' }); await named('Last turn failed');
      await row.getByText('Last turn failed', { exact: true }).waitFor(); await settle(page);
      const failedCue = await page.evaluate(() => ({ dash: __rim.getAttribute('stroke-dasharray'),
        cross: !!__contextArc.closest('svg').querySelector('path[d="m10 10 4 4m0-4-4 4"]'), red: __rim.classList.contains('stroke-red-500') }));
      assert(failedCue.red && failedCue.cross && failedCue.dash === 'none', 'Failed has the red solid rim and drawn cross');
      await shot('failed');
      f.send({ type: 'turn_ended', turn: 901, reason: 'completed' });
      await state('idle'); await named('Idle');
      for (let i = 1; i <= 123; i++) task(i, 'finished');
      await page.waitForFunction(() => __phone.ring.getAttribute('aria-label').includes('No background tasks.') &&
        !document.querySelector('[data-phone-status]')?.closest('[data-open="true"]'));
      await settle(page);
      assert(await page.evaluate(() => __phone.retained() && __rim.getAttribute('stroke-dasharray') === 'none' &&
        __rim.classList.contains('stroke-zinc-400') && !__phone.root.querySelector('.c6-ring-working') &&
        !__contextArc.closest('svg').querySelector('path')), 'Idle restores neutral solid rim, no cross/spin, all SVGs retained');
      assert.equal(await row.count(), 0, 'no idle composer status row');
      await shot('idle');
      const statusRow = await phoneStatusRow(f, { state, task, setWaiting: value => { waiting = value; }, shot,
        genuineCards: width === widths[0] });
      assert(await page.evaluate(() => __phone.retained()), 'full-row interactions retain every original header SVG');
      records.push({ label, geometry, counts, movement, states, waitingCue, failedCue, statusRow });
      fs.writeFileSync(path.join(output, `phone-${theme}-${choice}-dsf${deviceScaleFactor}.json`), JSON.stringify(records, null, 2));
      assert.deepEqual(f.errors, [], 'no crashes, invalid streams or external requests');
      console.log(`PASS ${label}`);
    }
  } catch (error) {
    if (label) await page.screenshot({ path: path.join(output, `${label}-FAIL.png`), timeout: 5000 }).catch(() => {});
    throw new Error(`${label || 'phone startup'}: ${error.stack || error}`);
  } finally { await context.close(); }
}

(async () => {
  fs.mkdirSync(output, { recursive: true });
  const { server, origin } = await serve();
  const browser = await launchBrowser(playwright, 'chromium');
  try {
    for (const theme of (process.env.THEME || 'light,dark').split(',')) {
      if (phone) {
        for (const choice of (process.env.MOTION || 'full,reduced,off').split(',')) {
          assert(['full', 'reduced', 'off'].includes(choice), 'phone MOTION: full,reduced,off');
          for (const dsf of process.env.SCALE ? [scale] : [1, 2]) await runPhone(browser, origin, theme, choice, dsf);
        }
      } else for (let runIndex = 0; runIndex < Number(process.env.RUNS || 3); runIndex++) await run(browser, origin, theme, runIndex);
    }
  } finally { await browser.close(); await new Promise(resolve => server.close(resolve)); }
})().catch(error => { console.error(error.stack || error); process.exitCode = 1; });
