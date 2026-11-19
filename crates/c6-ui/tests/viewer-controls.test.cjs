'use strict';

const assert = require('node:assert/strict');
const { readFileSync } = require('node:fs');
const path = require('node:path');
const { test } = require('node:test');
const { runInNewContext } = require('node:vm');

const script = path.join(__dirname, '../js/viewer-controls.js');
const source = readFileSync(script, 'utf8');

async function loadMore(dataset, codeClass) {
  let click, input;
  const root = {
    dataset,
    querySelector(selector) {
      if (selector === ':scope > .fv-source') return { textContent: 'fixture data\n' };
      if (selector.includes('code[class*=')) return { className: codeClass };
      return null;
    },
  };
  const target = { closest: () => root, hasAttribute: () => false, dataset: { fvAction: 'more' } };
  runInNewContext(source, {
    window: { c6Viewers: { render(value) { input = value; return ''; } } },
    document: {
      addEventListener(event, callback) { if (event === 'click') click = callback; },
      createElement() { return {}; },
    },
  }, { filename: script });
  await click({ target: { closest: () => target } });
  return input;
}

test('file redraw preserves path and MIME dispatch even with a source-code grammar', async () => {
  for (const [kind, name, mime, grammar] of [
    ['jsonlines', 'fixture.jsonl', 'application/x-ndjson', 'json'],
    ['csv', 'malformed.csv', 'text/csv', 'text'],
  ]) {
    const input = await loadMore({ path: name, mime, kind, mode: 'full', language: '' }, `language-${grammar}`);
    assert.equal(input.language, '', 'code rows do not override file dispatch');
    assert.equal(input.path, name);
    assert.equal(input.mime, mime);
  }
});

test('numbered fences keep the declared renderer language and grow from 2000 to 4000 lines', async () => {
  for (const [language, grammar] of [['rust', 'rust'], ['text', 'text'], ['json-code', 'json']]) {
    const input = await loadMore({ kind: 'code', mode: 'inline', language }, `language-${grammar}`);
    assert.equal(input.language, language);
    assert.equal(input.limit, 4000);
  }
});
