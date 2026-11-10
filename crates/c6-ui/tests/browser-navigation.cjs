'use strict';

// WebKit reports one *handled* native fetch refusal as pageerror while an old
// document is being replaced. Text/phase alone cannot distinguish it from CORS
// or JavaScript failures. Keep every raw diagnostic and fail closed on lost proof.
const diagnostic = origin => `Fetch API cannot load ${origin}/api/v1/attention due to access control checks.`;
const exactDiagnostic = (error, origin) => typeof error.stack === 'string' &&
  error.stack.split('\n')[0] === diagnostic(origin); // NOT Playwright's colon-split message/name.
const DEADLINE = Number(process.env.C6_TEST_DEADLINE_SECS || 15) * 1000;
async function bounded(promise) {
  let timer;
  try { return await Promise.race([promise, new Promise((_, reject) => {
    timer = setTimeout(() => reject(new Error('navigation observation: operation deadline')), DEADLINE);
  })]); } finally { clearTimeout(timer); }
}

function observeDocument() {
  const token = crypto.randomUUID(), rows = [], deliveries = [];
  let replacement = null, beforeunload = false, pagehide = false;
  const mark = (event, detail = {}) => {
    const row = { token, url: location.href, replacement, event, seq: rows.length + 1, ...detail };
    rows.push(row);
    // Only telemetry delivery can be cancelled on teardown. Never catch an app
    // failure, preventDefault, or install a rejection handler on its Promise.
    deliveries.push(window.__navigationObservation(row).catch(() => {}));
  };
  addEventListener('beforeunload', () => { beforeunload = true; mark('beforeunload'); });
  addEventListener('pagehide', () => { pagehide = true; mark('pagehide'); });
  addEventListener('error', event => mark('window.error', { message: event.message }));
  addEventListener('unhandledrejection', event => mark('window.unhandledrejection', {
    message: String(event.reason?.message ?? event.reason),
  }));
  window.__navigationDocument = {
    arm(id) { replacement = id; return this.snapshot(); },
    snapshot() { return { token, url: location.href, replacement, beforeunload, pagehide, rows }; },
    async flush() {
      // Explicit task turns dispatch pending unhandledrejection events, unlike
      // an animation/readiness wait. Await only our observer bridge deliveries.
      await new Promise(resolve => setTimeout(resolve, 0));
      await new Promise(resolve => setTimeout(resolve, 0));
      await Promise.all(deliveries);
      return this.snapshot();
    },
  };
  mark('installed');
}

async function navigationObserver({ page, engine, origin, baselineAttention, fatal, log = console.error }) {
  const ledger = [], rawPageErrors = [], pending = new Set();
  let active, serial = 0;
  const record = row => { ledger.push(row); return row; };
  await page.exposeBinding('__navigationObservation', ({ frame }, row) => {
    if (row.event === 'window.error' || row.event === 'window.unhandledrejection') {
      record({ kind: 'document', ...row });
      fatal(new Error(`${row.event}: ${row.message}`));
    } else if (frame === page.mainFrame()) {
      record({ kind: 'document', ...row });
    }
  });
  await page.addInitScript(observeDocument);
  page.on('request', request => {
    if (active && request.isNavigationRequest() && request.frame() === page.mainFrame()) {
      if (request.url() === active.destination && !request.redirectedFrom()) {
        active.requestObserved = true;
        record({ kind: 'destination-request', id: active.id, url: request.url() });
      } else active.otherNavigation = true;
    }
  });
  page.on('framenavigated', frame => {
    if (active && frame === page.mainFrame()) {
      active.committed = true;
      record({ kind: 'commit', id: active.id, url: frame.url() });
    }
  });

  function pageError(error) {
    const raw = record({ kind: 'pageerror', stack: String(error.stack || error), name: error.name, message: error.message });
    rawPageErrors.push(raw);
    log('PAGEERROR', error.stack || error); // Raw diagnostics are never removed or rewritten.
    const window = active;
    const before = window && ledger.some(row => row.kind === 'document' && row.event === 'beforeunload' &&
      row.token === window.token && row.url === window.url && row.replacement === window.id);
    if (engine !== 'webkit' || page.frames().length !== 1 || !baselineAttention() || !exactDiagnostic(error, origin) || !window ||
        !window.requestObserved || !before || window.committed || window.otherNavigation) {
      fatal(error); return;
    }
    // Retain the old execution-context handle: evaluating the current page here
    // could silently attribute a destination-document error to its predecessor.
    const verification = (async () => {
      try {
        const old = await bounded(window.document.evaluate(document => document.snapshot()));
        if (old.token !== window.token || old.url !== window.url || old.replacement !== window.id ||
            !old.beforeunload || old.pagehide ||
            old.rows.some(row => ['window.error', 'window.unhandledrejection'].includes(row.event))) {
          throw new Error('old-document identity/lifecycle proof missing or DOM failure observed');
        }
        raw.classification = 'webkit-native-attention-navigation-refusal';
        record({ kind: 'classified-native', id: window.id, token: old.token, url: old.url, stack: raw.stack });
        log('CLASSIFIED NATIVE NAVIGATION DIAGNOSTIC', raw.stack);
      } catch (proofError) {
        record({ kind: 'missing-attribution', id: window.id, reason: String(proofError) });
        fatal(error);
      }
    })();
    pending.add(verification);
    verification.finally(() => pending.delete(verification));
  }
  async function drain(document) {
    while (pending.size) await Promise.all([...pending]);
    const current = await bounded(document ? document.evaluate(probe => probe.flush()) :
      page.evaluate(() => window.__navigationDocument.flush()));
    // Independent local evidence also catches a delivery cancelled at teardown.
    for (const row of current.rows.filter(row => ['window.error', 'window.unhandledrejection'].includes(row.event))) {
      if (!ledger.some(item => item.kind === 'document' && item.token === row.token && item.seq === row.seq)) {
        record({ kind: 'document', ...row }); fatal(new Error(`${row.event}: ${row.message}`));
      }
    }
    while (pending.size) await Promise.all([...pending]);
  }
  async function navigate(destination, options) {
    if (active) throw new Error('overlapping explicit cold replacement');
    if (new URL(destination).origin !== origin) throw new Error('cold replacement must stay on owned origin');
    const document = await bounded(page.evaluateHandle(() => window.__navigationDocument));
    const id = `cold-replacement-${++serial}`;
    const old = await bounded(document.evaluate((probe, id) => probe.arm(id), id));
    if (typeof old.token !== 'string' || !old.token || new URL(old.url).origin !== origin || old.pagehide) {
      await document.dispose(); throw new Error('cold replacement requires positive live owned-document identity');
    }
    active = { id, document, token: old.token, url: old.url, destination,
      requestObserved: false, committed: false, otherNavigation: false };
    record({ kind: 'replacement-start', id, token: old.token, url: old.url, destination });
    try {
      return await page.goto(destination, options);
    } finally {
      // No deferred classification/fatal observation may outlive the caller's
      // errors assertion. Missing/destroyed old contexts remain fatal, not retries.
      try { await drain(); }
      finally { record({ kind: 'replacement-end', id }); active = undefined; await document.dispose(); }
    }
  }
  return { pageError, navigate, drain, ledger, rawPageErrors };
}

module.exports = { diagnostic, exactDiagnostic, navigationObserver };
