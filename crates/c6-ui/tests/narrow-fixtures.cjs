'use strict';

// Install AFTER motion.fixture(), BEFORE navigating to the audited views.
// These are read-only, loopback-only additions to that fixture, not a head.
// Unhandled methods/paths fall through to its API and external-network guards.
// Shapes follow c6-api/{docs,docs_archive,work,projects,starts}.rs and lib.rs;
// mail/calendar RPC returns the result directly, without a JSON-RPC envelope.

const DOC = 'narrow-doc';
const WORK = 'narrow-work';
const PROJECT = 'narrow-project';
const COLLECTION = 'release_verification_with_a_deliberately_long_collection_name';
const ACCOUNT = 'narrow-audit@example.test';
const WORKER = '00000000-0000-0000-0000-000000000000';
const LONG_PATH = '/home/auditor/projects/carbon6/worktrees/responsive-tiles-and-representative-content/crates/c6-ui/tests/narrow-fixtures.cjs';
const LABELS = ['responsive-layout-audit', 'release:phone-and-narrow-desktop', 'accessibility-and-localisation'];

// Paths relative to /r/home/. `explorer` is NOT a production route.
// For the comments-panel case, visit docs/narrow-doc then click
// page.locator('button[title="Show the comments"]').
// Mail rows and calendar event chips also open populated detail panels.
const views = [
  'docs', `docs/${DOC}`, 'decisions', 'work', `work/${WORK}`,
  'projects', `projects/${PROJECT}`, 'mail', 'calendar', 'realms',
  'realms/home', `realms/home/in/${encodeURIComponent(`collection:${COLLECTION}`)}`,
  `realms/home/collection/${COLLECTION}`,
];

function data(now) {
  const summary = (id, title, kind = 'doc', extra = {}) => ({
    id, realm: 'home', kind, title, labels: LABELS, rev: 4,
    created_by: 'you', created_at: now - 86400000,
    updated_by: 'session:parent', updated_at: now - 300000,
    ...extra,
  });
  const summaries = [
    summary(DOC, 'Responsive tiles: release readiness, accessibility findings, and cross-device verification plan'),
    summary('narrow-deck', 'Design review — narrow workspaces, long labels, and phone landscape', 'deck', { theme: 'paper', size: '16:9' }),
    summary('narrow-notes', 'Investigation notes for /home/auditor/projects/carbon6/worktrees/very-long-branch-name'),
    summary('narrow-archived', 'Superseded responsive layout notes retained for the audit trail', 'doc', { archived_at: now - 60000 }),
  ];
  const block = (id, kind, text, attrs = {}) => ({ id, kind, text, attrs, hash: `fixture-${id}`, by: 'session:parent', rev: 4 });
  const docs = summaries.map(s => ({ ...s, tabs: [{
    id: 'audit-tab', title: 'Release readiness and verification',
    blocks: s.kind === 'deck' ? [{
      ...block('audit.slide', 'slide', '', { layout: 'body' }),
      children: [
        { ...block('audit.slide-title', 'heading', '# Narrow workspace design review'), region: 'title' },
        { ...block('audit.slide-body', 'paragraph', 'Readable labels, reachable controls, and retained context on phones.'), region: 'body' },
      ],
    }] : [
      block('audit.heading', 'heading', '# Release readiness across narrow tiles'),
      block('audit.intro', 'paragraph', 'Verify long titles, action groups, labels, and comments without losing the reading position. The release is ready when the same content remains usable at every audited width.'),
      block('audit.checklist', 'item', '- Check both light and dark themes at 240, 320, 400, 480, and 640 pixels.'),
      block('audit.path', 'code', `\`\`\`text\n${LONG_PATH}\n\`\`\``),
      block('audit.table', 'table', '| View | Evidence |\n| --- | --- |\n| Document and comments | Long quoted text and multiple authors |\n| Work and projects | Labels, linked documents, and to-dos |'),
      block('audit.quote', 'quote', '> Keep all controls reachable; do not replace meaningful content with an empty state.'),
    ],
  }, ...(s.kind === 'doc' ? [{ id: 'evidence-tab', title: 'Browser screenshots and follow-up observations', blocks: [block('audit.evidence', 'paragraph', 'Evidence includes desktop tiles and portrait / landscape phone layouts in both themes.')] }] : [])] }));
  const threads = [{
    id: 'narrow-thread', doc: DOC,
    anchor: { tab: 'audit-tab', block: 'audit.intro', quote: 'long titles, action groups, labels, and comments' },
    state: 'open', about: 'Verify long titles, action groups, labels, and comments without losing the reading position.',
    to_agent: true, agent: 'parent', needs_you: true, created_by: 'you', created_at: now - 600000,
    edits: [4], comments: [
      { id: 'narrow-comment-1', by: 'you', at: now - 600000, text: 'Can the comments stay readable beside a long document title, including a quoted passage and the controls for replying or resolving the discussion?' },
      { id: 'narrow-comment-2', by: 'session:parent', at: now - 300000, text: `The representative audit covers both themes and all requested widths. The source is \`${LONG_PATH}\`. Please confirm the phone landscape result before closing this thread.` },
    ],
  }, {
    id: 'narrow-unsent', doc: DOC, anchor: { tab: 'audit-tab' }, state: 'open',
    about: 'Release readiness and verification', unsent: true,
    created_by: 'you', created_at: now - 120000,
    comments: [{ id: 'narrow-comment-3', by: 'you', at: now - 120000, editable: true, text: 'Follow up on keyboard focus and long unbroken labels: release_candidate_2026_responsive_workspace_verification.' }],
  }];
  const attached = [
    { ref: `doc:${DOC}`, title: summaries[0].title, by: 'you', at: now - 600000 },
    { ref: 'session:parent', title: 'Browser verification for responsive workspace tiles', by: 'you', at: now - 600000 },
    { ref: 'decision:7101', title: 'Choose the cross-device release verification approach', by: 'you', at: now - 600000 },
  ];
  const work = [{
    id: WORK, realm: 'home', version: 3,
    title: 'Verify responsive workspace tiles, long content, and phone landscape before release',
    done_when: 'Every representative view remains readable at all audited widths, with reachable actions in both themes.',
    status: 'open', state: 'needs_you', why: 'Browser evidence is ready; one release decision needs your review.',
    attached, live: attached.map(a => ({ ref: a.ref, title: a.title, state: a.ref.startsWith('decision:') ? 'open' : 'idle', at: now - 300000 })),
    note: { done: 'Representative documents, discussions, and project relationships are ready.', left: 'Review narrow-tile and phone screenshots.', next: 'Resolve the release verification decision.', at: now - 300000 },
    todos: [
      { id: 'narrow-todo-1', text: 'Review long labels and comments on phone portrait and landscape', by: 'you', at: now - 600000 },
      { id: 'narrow-todo-2', text: 'Check the desktop matrix in light and dark themes', by: 'session:parent', at: now - 600000, done_at: now - 300000, done_by: 'session:parent', evidence: 'Representative cases captured.' },
    ],
    project: PROJECT, project_title: 'Responsive workspaces and cross-device release readiness', phase: 'verification',
    place: { worker_id: WORKER, worker: 'Motion fixture', path: LONG_PATH.replace('/crates/c6-ui/tests/narrow-fixtures.cjs', ''), branch: 'audit/responsive-workspace-release-candidate-with-long-labels', repo_root: '/home/auditor/projects/carbon6', cleanup: 'ask', session: 'parent', at: now - 86400000 },
    labels: LABELS, moved_at: now - 300000, created_at: now - 86400000, updated_at: now - 300000,
  }, {
    id: 'narrow-work-followup', realm: 'home', version: 1,
    title: 'Follow up on accessibility, keyboard focus, and unusually long repository paths',
    done_when: 'Keyboard navigation and overflow checks have documented evidence.',
    status: 'open', state: 'waiting', why: 'Waiting for the release review.',
    attached: [], live: [], labels: LABELS.slice(1),
    project: PROJECT, project_title: 'Responsive workspaces and cross-device release readiness', phase: 'follow-up',
    created_at: now - 86400000, updated_at: now - 120000,
  }];
  const projects = [{
    id: PROJECT, realm: 'home', version: 2,
    title: 'Responsive workspaces and cross-device release readiness',
    purpose: 'Keep documents, work, decisions, and connected tools useful in narrow tiles and on phones without hiding important actions.',
    status: 'active', state: 'needs_you', why: 'Release verification is ready for review.',
    phases: [
      { id: 'verification', title: 'Cross-device browser verification with representative populated views', done_when: 'Review the complete width and theme matrix.', state: 'current', target: new Date(now + 86400000).toISOString().slice(0, 10) },
      { id: 'follow-up', title: 'Accessibility follow-up and documented release evidence', state: 'upcoming', done_when: 'Record keyboard and screen-reader checks.' },
    ],
    documents: [{ doc: DOC, role: 'plan', by: 'you', at: now - 86400000 }, { doc: 'narrow-deck', role: 'review', by: 'you', at: now - 86400000 }],
    docs: summaries.slice(0, 2).map((s, i) => ({ doc: s.id, title: s.title, kind: s.kind, role: i ? 'review' : 'plan', updated_at: s.updated_at, answered: i ? 0 : 1 })),
    links: [{ ref: 'decision:7101', title: 'Cross-device release verification approach', by: 'you', at: now - 600000 }],
    note: { text: 'The verification phase has representative browser content. Review narrow widths and phone landscape, then plan the accessibility follow-up.', at: now - 300000 },
    proposed: [{ id: 'narrow-proposed-work', title: 'Audit long localised labels and unbroken repository identifiers', done_when: 'Controls remain reachable with realistic translated labels.', phase: 'follow-up', by: 'session:parent', at: now - 120000 }],
    work: work.map(w => ({ id: w.id, title: w.title, state: w.state, why: w.why, phase: w.phase, moved_at: w.updated_at })),
    labels: LABELS, moved_at: now - 300000, created_at: now - 86400000, updated_at: now - 300000,
  }, {
    id: 'narrow-project-followup', realm: 'home', version: 1,
    title: 'Long-term accessibility and internationalisation follow-up for workspace navigation',
    purpose: 'Collect follow-up findings without interrupting the current release.',
    status: 'active', state: 'quiet', labels: LABELS.slice(1),
    created_at: now - 86400000, updated_at: now - 600000,
  }];
  const decision = (id, title, extra) => ({
    id, realm: 'home', kind: 'choose', title, source: 'session:parent', priority: 'normal',
    state: 'open', created_at: now - 600000, updated_at: now - 300000, ...extra,
  });
  const decisions = [
    decision(7101, 'Choose how to verify the responsive workspace release across narrow tiles and phones', {
      document: 'The browser matrix covers **both themes** and populated views. Choose whether to review the complete screenshot set or prioritise the smallest widths first.\n\nLong paths and labels are included; this is not an empty-state audit.',
      group: 'responsive-workspace-release-verification',
      options: [{ label: 'Review the complete cross-device evidence', does: 'Inspect every supported width in both themes.' }, { label: 'Prioritise phone portrait and landscape', does: 'Review the constrained layouts before desktop tiles.' }],
    }),
    decision(7102, 'Archive superseded responsive workspace drafts after preserving the release evidence', {
      kind: 'approve', priority: 'high', waiting: true,
      context: { op: 'docs.archive_many', reason: 'These drafts are superseded by the current release plan; preserve linked evidence and keep the selection explicit.', archive_docs: [
        { id: 'narrow-notes', title: summaries[2].title, kind: 'doc', reasons: ['Linked from the cross-device release readiness project', 'Contains a discussion that still needs review'] },
        { id: 'narrow-deck', title: summaries[1].title, kind: 'deck', reasons: [] },
      ] },
      options: [{ label: 'Allow', does: 'Archive only the listed documents, once.' }, { label: 'Not now', does: 'Leave every document unchanged.' }],
    }),
    decision(7103, 'Confirm the release reviewer and include any remaining phone landscape observations', {
      kind: 'provide', document: 'Record who will sign off the browser evidence and any remaining concerns.',
      form: [{ name: 'reviewer', label: 'Release reviewer with a deliberately long descriptive field label', kind: 'text', required: true }, { name: 'observations', label: 'Phone landscape observations', kind: 'long' },
        { name: 'strategy', label: 'Review strategy', kind: 'choice', choices: ['Prioritise narrow tiles', 'Review every layout'] }],
    }),
  ];
  const mailThreads = [
    { account: ACCOUNT, thread_id: 'narrow-mail-thread', subject: 'Release review: responsive workspace screenshots, long labels, and phone landscape follow-up', snippet: 'Please review the populated document, work, and project views before signing off the release evidence.', date: now - 300000, count: 3, unread: true, starred: true, attachments: true, people: ['Alex Morgan — release and accessibility reviewer', 'me'], labels: ['INBOX', 'UNREAD', 'narrow-release'] },
    { account: ACCOUNT, thread_id: 'narrow-mail-followup', subject: 'Accessibility follow-up and keyboard navigation verification results', snippet: 'The follow-up checklist includes long repository paths and localised labels.', date: now - 3600000, count: 1, unread: false, starred: false, attachments: false, people: ['Sam Rivera — internationalisation engineering'], labels: ['INBOX', 'narrow-release'] },
  ];
  const messages = [{
    item_id: 'narrow-mail-message', gmail_id: 'narrow-gmail-message',
    from: 'alex.morgan@example.test', from_name: 'Alex Morgan — release and accessibility reviewer',
    to: [[ACCOUNT, 'Workspace audit reviewer']], cc: [['sam.rivera@example.test', 'Sam Rivera — internationalisation engineering']],
    date: now - 300000, subject: mailThreads[0].subject, unread: false, labels: ['INBOX'],
    text: `Please review the release evidence in both themes, including narrow tiles and phone landscape.\n\nSource path: ${LONG_PATH}\n\nKeep the document comments and the project follow-up actions reachable.`,
    attachments: [{ index: 0, name: 'responsive-workspace-cross-device-release-verification-evidence.pdf', mime: 'application/pdf', size: 248320 }],
  }];
  const calendars = [
    { account: ACCOUNT, id: 'narrow-calendar', summary: 'Release readiness, accessibility, and cross-device verification', color: '#0284c7', primary: true, writable: true, selected: true },
    { account: ACCOUNT, id: 'narrow-team-calendar', summary: 'Engineering follow-up and internationalisation reviews', color: '#7c3aed', primary: false, writable: false, selected: true },
  ];
  const realm = { id: 'home', name: 'Home — responsive workspace release verification', description: 'Documents, work, decisions, and release evidence with realistic long names and connected objects.', created_at: now - 86400000, created_by: 'you', reads_commons: true };
  const entries = [
    { group: 'data', section: `collection:${COLLECTION}`, label: 'Cross-device release verification evidence and observations', count: 12 },
    { group: 'knowledge', section: 'things:topic', label: 'Accessibility and responsive layout topics', count: 7 },
    { group: 'knowledge', section: 'items:document', label: 'Release evidence documents and review conversations', count: 18 },
    { group: 'work', section: 'sessions', label: 'Browser verification sessions', count: 1, link: '/sessions' },
    { group: 'work', section: 'decisions:open', label: 'Open release decisions and document cleanup approvals', count: 3, link: '/decisions' },
  ].map(e => ({ ...e, changed_at: now - 300000 }));
  const refs = [
    { kind: 'collection', id: COLLECTION, title: entries[0].label, sub: '12 records · release-verification evidence', at: now - 300000 },
    { kind: 'decision', id: '7101', title: decisions[0].title, sub: 'Needs your review · responsive-workspace-release-verification', at: now - 300000 },
    { kind: 'session', id: 'parent', title: 'Browser verification for narrow tiles and long repository paths', sub: LONG_PATH, at: now - 300000 },
  ];
  return { summaries, docs, threads, work, projects, decisions, mailThreads, messages, calendars, realm, entries, refs };
}

async function install(context, origin) {
  // Setup before first navigation may supply the fixture's declared loopback
  // origin. Existing callers still capture their already-open local origin.
  // Never intercept another origin: its external-request guard stays effective.
  const origins = new Set((origin ? [origin] : context.pages().map(page => page.url())).map(value => {
    try {
      const u = new URL(value);
      return ['127.0.0.1', 'localhost', '[::1]'].includes(u.hostname) ? u.origin : null;
    } catch { return null; }
  }).filter(Boolean));
  if (!origins.size) throw new Error('narrow-fixtures.install requires a declared or open loopback motion fixture');
  const d = data(Date.now());
  await context.route('**/api/**', async route => {
    const request = route.request(), url = new URL(request.url()), p = url.pathname;
    if (!origins.has(url.origin)) return route.fallback();
    const get = request.method() === 'GET';
    let json;
    if (get && p === '/api/v1/docs') {
      json = d.summaries.filter(s => Boolean(s.archived_at) === (url.searchParams.get('archived') === 'true'));
      for (const key of ['kind', 'label', 'by', 'q']) {
        const value = url.searchParams.get(key);
        if (!value) continue;
        json = json.filter(s => key === 'kind' ? s.kind === value : key === 'label' ? s.labels.includes(value) : key === 'by' ? [s.created_by, s.updated_by].includes(value) : s.title.toLowerCase().includes(value.toLowerCase()));
      }
    } else if (get && /^\/api\/v1\/docs\/[^/]+(?:\/(threads|exports|google|history))?$/.test(p)) {
      const [, id, part] = p.match(/^\/api\/v1\/docs\/([^/]+)(?:\/(.*))?$/);
      const doc = d.docs.find(x => x.id === decodeURIComponent(id));
      if (!doc) return route.fallback();
      json = part === 'threads' ? d.threads.map(t => ({ ...t, doc: doc.id })) : part === 'google' ? { accounts: [], app: 'Google Docs' } : part ? [] : doc;
    } else if (get && p === '/api/v1/work') {
      json = url.searchParams.get('closed') === 'true' ? [] : d.work;
    } else if (get && p === '/api/v1/work/of') {
      json = d.work.filter(w => w.attached.some(a => a.ref === url.searchParams.get('ref')));
    } else if (get && p.startsWith('/api/v1/work/')) {
      json = d.work.find(w => w.id === p.slice('/api/v1/work/'.length));
    } else if (get && p === '/api/v1/projects') {
      json = url.searchParams.get('closed') === 'true' ? [] : d.projects;
    } else if (get && p.startsWith('/api/v1/projects/of-session/')) {
      json = [];
    } else if (get && p.startsWith('/api/v1/projects/')) {
      json = d.projects.find(x => x.id === p.slice('/api/v1/projects/'.length));
    } else if (get && p === '/api/v1/decisions/count') {
      json = { open: d.decisions.length };
    } else if (get && p === '/api/v1/decisions') {
      json = !url.searchParams.get('state') || url.searchParams.get('state') === 'open' ? d.decisions : [];
    } else if (get && p === '/api/v1/google/accounts') {
      json = [{ id: ACCOUNT, email: ACCOUNT, name: 'Workspace audit reviewer', added_at: d.realm.created_at }];
    } else if (request.method() === 'POST' && p === '/api/v1/memex/rpc') {
      const { method, params = {} } = request.postDataJSON();
      if (method === 'memex.mail.threads') json = params.before ? [] : d.mailThreads;
      else if (method === 'memex.mail.labels') json = [[ACCOUNT, 'narrow-release', 'Release verification and accessibility follow-up']];
      else if (method === 'memex.mail.thread') json = d.messages;
      else if (method === 'memex.mail.outgoing') json = [];
      else if (method === 'memex.triage.get') json = null;
      else if (method === 'memex.triage.marks') json = [];
      else if (method === 'memex.calendar.calendars') json = d.calendars;
      else if (method === 'memex.calendar.events') {
        // Put events inside the browser's requested week/month/agenda range,
        // not a hardcoded historical date or the test runner's timezone.
        const day = 86400000, from = Number(params.from), to = Number(params.to);
        const start = Math.min(from + 2 * day + 10 * 3600000, to - 2 * 3600000);
        json = [0, 1].map(i => ({
          account: ACCOUNT, calendar_id: d.calendars[i].id, event_id: `narrow-event-${i}`,
          summary: i ? 'Accessibility and internationalisation follow-up with engineering reviewers' : 'Responsive workspace release readiness and cross-device screenshot review',
          description: `Review populated views in light and dark themes.\nSource path: ${LONG_PATH}`,
          location: 'Release review room — engineering and accessibility working group',
          start: start + i * day, end: start + i * day + 3600000,
          all_day: false, recurring: false, my_status: 'accepted', busy: true, conference: '', html_link: '',
          attendees: [{ email: ACCOUNT, name: 'Workspace audit reviewer', response: 'accepted', me: true }, { email: 'alex.morgan@example.test', name: 'Alex Morgan — release and accessibility reviewer', response: 'tentative', me: false }],
        }));
      }
    } else if (get && p === '/api/v1/explorer/map') {
      json = { realms: [
        { realm: d.realm, counts: { collections: 1, records: 12, things: 7, items: 18, sessions: 1, open_decisions: 3 }, health: 3, spend_month: 12.34, last_activity: d.realm.created_at + 60000 },
        { realm: { ...d.realm, id: 'release-evidence', name: 'Release evidence and internationalisation follow-up' }, counts: { collections: 2, records: 24, items: 9 }, health: 0, spend_month: 2.5, last_activity: d.realm.created_at },
      ], grants: [{ from_realm: 'home', to_realm: 'release-evidence', mode: 'read', kind: 'collection', state: 'active' }] };
    } else if (get && /^\/api\/v1\/realms\/[^/]+\/(summary|activity|list|search|object|neighborhood)$/.test(p)) {
      const realm = decodeURIComponent(p.split('/')[4]), part = p.split('/')[5];
      if (!['home', 'release-evidence'].includes(realm)) return route.fallback();
      if (part === 'summary') json = { realm: { ...d.realm, id: realm }, entries: d.entries, health: [d.refs[1]], spend_month: 12.34, last_activity: d.refs[0].at, knowledge: true };
      else if (part === 'activity' || part === 'list') json = d.refs;
      else if (part === 'search') json = [{ label: 'Release evidence and connected work', items: d.refs }];
      else if (part === 'object') json = {
        kind: url.searchParams.get('kind') || 'collection', id: url.searchParams.get('id') || COLLECTION,
        title: d.entries[0].label, sub: COLLECTION,
        body: 'Release verification evidence links populated documents, browser sessions, and decisions.\n\nAll fixture data is local; no account or external service is required.',
        fields: [['Repository path', LONG_PATH], ['Labels', LABELS.join(', ')], ['Verification status', 'Ready for cross-device review']],
        connected: [{ label: 'Release review and browser sessions', items: d.refs.slice(1) }], data: { name: COLLECTION, labels: LABELS },
      };
      else json = { nodes: d.refs.map((r, i) => ({ ref: r, depth: i ? 1 : 0 })), edges: [{ from: 0, to: 1, label: 'needs review' }, { from: 0, to: 2, label: 'verified by' }] };
    }
    if (json === undefined) return route.fallback();
    return route.fulfill({ json });
  });
}

module.exports = { install, views };
