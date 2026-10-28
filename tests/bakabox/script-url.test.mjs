// SPDX-License-Identifier: MIT
// ALB-03 · the browser runtime's URL rule — the sink for a `SetAttr` frame and
// for a slot painted into an attribute, both of which can carry a value another
// visitor wrote.
//
// The same cases as `runtime::jsx_attributes::tests` on the Rust side; the
// table and the replacement string are pinned equal to the server's by
// `the_client_url_rule_matches_this_one`.
//
// Run with: node --test tests/bakabox/script-url.test.mjs

import { strict as assert } from 'node:assert';
import { test } from 'node:test';

import { safeAttributeValue } from '../../assets/albedo-runtime.js';

const BLOCKED =
  "javascript:throw new Error('ALBEDO blocked a javascript: URL as a security precaution.')";

const SCRIPT_URLS = [
  'javascript:alert(1)',
  ' JaVaScRiPt:alert(1)',
  'java\tscript:alert(1)',
  'jav\nascr\ript:alert(1)',
  '\u0000\u001f javascript:alert(1)',
  '\njavascript:alert(1)',
  'VBScript:msgbox(1)',
];

const SAFE_URLS = [
  'https://example.com/?q=javascript:',
  '/relative/javascript:alert(1)',
  'javascript',
  '&#106;avascript:alert(1)',
  'mailto:ada@example.com',
  '',
  'jаvascript:alert(1)', // Cyrillic а — not a scheme the URL parser accepts
];

test('a script URL is replaced in every URL attribute, case-insensitively', () => {
  for (const url of SCRIPT_URLS) {
    for (const name of ['href', 'HREF', 'src', 'action', 'formAction', 'xlink:href']) {
      assert.equal(safeAttributeValue(name, url), BLOCKED, `${name}=${JSON.stringify(url)}`);
    }
  }
});

test('a text attribute and an ordinary URL pass through unchanged', () => {
  for (const url of SCRIPT_URLS) {
    assert.equal(safeAttributeValue('title', url), url);
  }
  for (const url of SAFE_URLS) {
    assert.equal(safeAttributeValue('href', url), url, JSON.stringify(url));
  }
});

test('replacing twice is a no-op', () => {
  assert.equal(safeAttributeValue('href', BLOCKED), BLOCKED);
});
