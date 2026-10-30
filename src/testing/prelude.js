// albedo test — the API a test file imports from "albedo/test".
//
// Installed once per realm, after the engine's own prelude (h, hooks, module
// helpers) and before any test file loads. Everything a test can reach is on
// `globalThis.__albedo_test`; the import rewriter binds `import { … } from
// "albedo/test"` to that object, so none of these names become globals that
// project modules could see.
//
// The host bridge is one synchronous function, `__albedo_test_host(op, json)`.
// It answers before it returns — a request runs through the app's router, a
// render through the app's renderer — so `await visit(…)` settles on the next
// microtask and a test needs no event loop.
(function () {
  'use strict';
  if (globalThis.__albedo_test) { return; }

  const has = Object.prototype.hasOwnProperty;
  const hostCall = globalThis.__albedo_test_host;

  function call(op, payload) {
    const raw = hostCall(op, JSON.stringify(payload === undefined ? null : payload));
    const envelope = JSON.parse(raw);
    if (!envelope.ok) { throw new Error(envelope.error); }
    return envelope.value;
  }

  // ── formatting ─────────────────────────────────────────────────────────
  function fmt(value, depth) {
    depth = depth || 0;
    if (value === undefined) { return 'undefined'; }
    if (value === null) { return 'null'; }
    if (typeof value === 'string') {
      const shown = value.length > 240 ? value.slice(0, 240) + '…' : value;
      return JSON.stringify(shown);
    }
    if (typeof value === 'number' || typeof value === 'boolean' || typeof value === 'bigint') {
      return String(value);
    }
    if (typeof value === 'function') {
      return value.__albedo_ref
        ? '<' + componentName(value.__albedo_ref) + '>'
        : '[Function ' + (value.name || 'anonymous') + ']';
    }
    if (value instanceof View) { return value.describe(); }
    if (value instanceof RegExp) { return String(value); }
    if (depth > 3) { return Array.isArray(value) ? '[…]' : '{…}'; }
    if (Array.isArray(value)) {
      return '[' + value.map(function (item) { return fmt(item, depth + 1); }).join(', ') + ']';
    }
    const keys = Object.keys(value);
    return '{ ' + keys.map(function (key) {
      return key + ': ' + fmt(value[key], depth + 1);
    }).join(', ') + (keys.length ? ' }' : '}');
  }

  function componentName(ref) {
    const file = String(ref.module).split('/').pop().replace(/\.[^.]+$/, '');
    return ref.export === 'default' ? file : ref.export;
  }

  // ── equality ───────────────────────────────────────────────────────────
  function equals(a, b) {
    if (Object.is(a, b)) { return true; }
    if (typeof a !== 'object' || typeof b !== 'object' || a === null || b === null) { return false; }
    if (Array.isArray(a) !== Array.isArray(b)) { return false; }
    if (a instanceof Date || b instanceof Date) {
      return a instanceof Date && b instanceof Date && a.getTime() === b.getTime();
    }
    if (a instanceof RegExp || b instanceof RegExp) { return String(a) === String(b); }
    if (Array.isArray(a)) {
      if (a.length !== b.length) { return false; }
      for (let i = 0; i < a.length; i++) { if (!equals(a[i], b[i])) { return false; } }
      return true;
    }
    // Like `toEqual` elsewhere: a key holding `undefined` is a key not there.
    const keysA = Object.keys(a).filter(function (k) { return a[k] !== undefined; });
    const keysB = Object.keys(b).filter(function (k) { return b[k] !== undefined; });
    if (keysA.length !== keysB.length) { return false; }
    for (const key of keysA) {
      if (!has.call(b, key) || !equals(a[key], b[key])) { return false; }
    }
    return true;
  }

  // ── expect ─────────────────────────────────────────────────────────────
  class AssertionError extends Error {
    constructor(message) { super(message); this.name = 'AssertionError'; }
  }

  function textOf(value) {
    return value instanceof View ? value.text : value;
  }

  function expect(actual) {
    function matchers(negated) {
      const not = negated ? 'not ' : '';
      function check(pass, message) {
        if (Boolean(pass) === negated) { throw new AssertionError(message); }
      }
      function number(name, value) {
        if (typeof value !== 'number' && typeof value !== 'bigint') {
          throw new AssertionError(name + '() needs a number, got ' + fmt(value));
        }
      }
      return {
        toBe(expected) {
          check(Object.is(actual, expected),
            'expected ' + fmt(actual) + ' ' + not + 'to be ' + fmt(expected) +
            (!negated && equals(actual, expected) ? ' — they are equal but not the same value; use toEqual()' : ''));
        },
        toEqual(expected) {
          check(equals(actual, expected), 'expected ' + fmt(actual) + ' ' + not + 'to equal ' + fmt(expected));
        },
        toBeTruthy() { check(actual, 'expected ' + fmt(actual) + ' ' + not + 'to be truthy'); },
        toBeFalsy() { check(!actual, 'expected ' + fmt(actual) + ' ' + not + 'to be falsy'); },
        toBeNull() { check(actual === null, 'expected ' + fmt(actual) + ' ' + not + 'to be null'); },
        toBeUndefined() { check(actual === undefined, 'expected ' + fmt(actual) + ' ' + not + 'to be undefined'); },
        toBeDefined() { check(actual !== undefined, 'expected ' + fmt(actual) + ' ' + not + 'to be defined'); },
        toContain(item) {
          // A rendered view or page is searched by its visible text — the words
          // a reader sees, not the markup around them.
          const target = textOf(actual);
          let pass;
          if (typeof target === 'string') {
            pass = target.indexOf(String(item)) !== -1;
          } else if (Array.isArray(target)) {
            pass = target.some(function (entry) { return equals(entry, item); });
          } else {
            throw new AssertionError('toContain() needs a string, an array, or what render()/visit() returned — got ' + fmt(actual));
          }
          check(pass, 'expected ' + fmt(actual) + ' ' + not + 'to contain ' + fmt(item));
        },
        toMatch(pattern) {
          const target = textOf(actual);
          if (typeof target !== 'string') {
            throw new AssertionError('toMatch() needs a string — got ' + fmt(actual));
          }
          const pass = pattern instanceof RegExp ? pattern.test(target) : target.indexOf(String(pattern)) !== -1;
          check(pass, 'expected ' + fmt(actual) + ' ' + not + 'to match ' + fmt(pattern));
        },
        toHaveLength(length) {
          const size = actual === null || actual === undefined ? undefined : actual.length;
          check(size === length, 'expected ' + fmt(actual) + ' ' + not + 'to have length ' + length + (negated ? '' : ', it has ' + fmt(size)));
        },
        toHaveProperty(path, value) {
          const keys = Array.isArray(path) ? path : String(path).split('.');
          let cursor = actual;
          let found = true;
          for (const key of keys) {
            if (cursor === null || cursor === undefined || !(key in Object(cursor))) { found = false; break; }
            cursor = cursor[key];
          }
          const pass = found && (arguments.length < 2 || equals(cursor, value));
          check(pass, 'expected ' + fmt(actual) + ' ' + not + 'to have property ' + fmt(keys.join('.')) +
            (arguments.length < 2 ? '' : ' equal to ' + fmt(value)));
        },
        toBeGreaterThan(n) { number('toBeGreaterThan', actual); check(actual > n, 'expected ' + fmt(actual) + ' ' + not + 'to be greater than ' + fmt(n)); },
        toBeGreaterThanOrEqual(n) { number('toBeGreaterThanOrEqual', actual); check(actual >= n, 'expected ' + fmt(actual) + ' ' + not + 'to be at least ' + fmt(n)); },
        toBeLessThan(n) { number('toBeLessThan', actual); check(actual < n, 'expected ' + fmt(actual) + ' ' + not + 'to be less than ' + fmt(n)); },
        toBeLessThanOrEqual(n) { number('toBeLessThanOrEqual', actual); check(actual <= n, 'expected ' + fmt(actual) + ' ' + not + 'to be at most ' + fmt(n)); },
        toThrow(expected) {
          if (typeof actual !== 'function') {
            throw new AssertionError('toThrow() needs a function to call — got ' + fmt(actual));
          }
          let thrown = null;
          let threw = false;
          try { actual(); } catch (error) { threw = true; thrown = error; }
          const message = thrown && typeof thrown.message === 'string' ? thrown.message : String(thrown);
          let pass = threw;
          if (threw && expected !== undefined) {
            pass = expected instanceof RegExp ? expected.test(message) : message.indexOf(String(expected)) !== -1;
          }
          check(pass, 'expected the function ' + not + 'to throw' +
            (expected === undefined ? '' : ' ' + fmt(expected)) +
            (threw ? ' — it threw ' + fmt(message) : ' — it returned'));
        },
      };
    }
    const positive = matchers(false);
    positive.not = matchers(true);
    return positive;
  }

  // ── markup ─────────────────────────────────────────────────────────────
  // A small, forgiving HTML reader — enough to find elements and read their
  // text the way a test asks for them. It is not a browser: scripts do not run
  // and styles do not apply. What it reads is the markup the server sent.
  const VOID = new Set(['area', 'base', 'br', 'col', 'embed', 'hr', 'img', 'input', 'link', 'meta', 'param', 'source', 'track', 'wbr']);
  const RAW = new Set(['script', 'style', 'textarea', 'title']);
  const HIDDEN_TEXT = new Set(['script', 'style', 'template', 'head']);
  const ENTITIES = {
    amp: '&', lt: '<', gt: '>', quot: '"', apos: "'", nbsp: ' ', mdash: '—', ndash: '–',
    rsquo: '’', lsquo: '‘', rdquo: '”', ldquo: '“', hellip: '…', middot: '·',
    copy: '©', reg: '®', trade: '™', times: '×', larr: '←', rarr: '→', bull: '•',
  };

  function decode(text) {
    return text.replace(/&(#x[0-9a-fA-F]+|#[0-9]+|[a-zA-Z]+);/g, function (whole, code) {
      if (code[0] === '#') {
        const point = code[1] === 'x' || code[1] === 'X' ? parseInt(code.slice(2), 16) : parseInt(code.slice(1), 10);
        try { return String.fromCodePoint(point); } catch (error) { return whole; }
      }
      return has.call(ENTITIES, code) ? ENTITIES[code] : whole;
    });
  }

  function parseAttrs(source) {
    const attrs = {};
    const pattern = /([^\s"'>\/=]+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s"'=<>`]+)))?/g;
    let match;
    while ((match = pattern.exec(source)) !== null) {
      const name = match[1].toLowerCase();
      const value = match[2] !== undefined ? match[2] : match[3] !== undefined ? match[3] : match[4] !== undefined ? match[4] : '';
      if (!has.call(attrs, name)) { attrs[name] = decode(value); }
    }
    return attrs;
  }

  function parse(html) {
    const root = { tag: '#root', attrs: {}, children: [], parent: null, start: 0, end: html.length };
    const stack = [root];
    const token = /<!--[\s\S]*?-->|<![^>]*>|<\/([a-zA-Z][\w:-]*)\s*>|<([a-zA-Z][\w:-]*)((?:[^>"']|"[^"]*"|'[^']*')*)>/g;
    let last = 0;
    let match;
    const top = function () { return stack[stack.length - 1]; };
    const pushText = function (from, to) {
      if (to > from) { top().children.push({ text: html.slice(from, to) }); }
    };
    while ((match = token.exec(html)) !== null) {
      pushText(last, match.index);
      last = token.lastIndex;
      if (match[1]) {
        const name = match[1].toLowerCase();
        for (let i = stack.length - 1; i > 0; i--) {
          if (stack[i].tag === name) {
            for (let j = stack.length - 1; j >= i; j--) { stack[j].end = match.index; stack[j].outerEnd = token.lastIndex; }
            stack.length = i;
            break;
          }
        }
        continue;
      }
      if (!match[2]) { continue; }
      const name = match[2].toLowerCase();
      let rawAttrs = match[3] || '';
      const selfClosing = /\/\s*$/.test(rawAttrs);
      if (selfClosing) { rawAttrs = rawAttrs.replace(/\/\s*$/, ''); }
      const node = { tag: name, attrs: parseAttrs(rawAttrs), children: [], parent: top(), outerStart: match.index, start: token.lastIndex, end: token.lastIndex, outerEnd: token.lastIndex };
      top().children.push(node);
      if (VOID.has(name) || selfClosing) { continue; }
      if (RAW.has(name)) {
        const close = html.toLowerCase().indexOf('</' + name, token.lastIndex);
        const stop = close === -1 ? html.length : close;
        if (stop > token.lastIndex) { node.children.push({ text: html.slice(token.lastIndex, stop) }); }
        node.end = stop;
        const after = close === -1 ? html.length : html.indexOf('>', close) + 1;
        node.outerEnd = after;
        token.lastIndex = after;
        last = after;
        continue;
      }
      stack.push(node);
    }
    pushText(last, html.length);
    for (let i = stack.length - 1; i > 0; i--) { stack[i].end = html.length; stack[i].outerEnd = html.length; }
    return root;
  }

  function textContent(node, out) {
    for (const child of node.children) {
      if (has.call(child, 'text')) {
        out.push(node.tag === 'textarea' || node.tag === 'title' ? child.text : decode(child.text));
      } else if (!HIDDEN_TEXT.has(child.tag)) {
        if (child.tag === 'br') { out.push('\n'); }
        textContent(child, out);
        if (/^(p|div|li|h[1-6]|tr|section|article|header|footer|main|nav|ul|ol|form|label|button|td|th|pre|blockquote)$/.test(child.tag)) { out.push(' '); }
      }
    }
    return out;
  }

  function collapse(text) {
    return text.replace(/[\s ]+/g, ' ').trim();
  }

  // Selectors: tag, #id, .class, [attr], [attr=v] (also ^= $= *= ~=), joined by
  // descendant (space) and child (>) combinators, in comma-separated lists.
  function parseSelector(selector) {
    const list = [];
    for (const part of String(selector).split(',')) {
      const steps = [];
      const re = /\s*(>)?\s*((?:[a-zA-Z][\w-]*|\*)?(?:#[\w-]+|\.[\w-]+|\[[^\]]+\])*)/g;
      let match;
      let combinator = ' ';
      const text = part.trim();
      if (!text) { throw new Error('empty selector in ' + fmt(selector)); }
      let consumed = 0;
      while (consumed < text.length && (match = re.exec(text)) !== null) {
        if (match[0].length === 0) { break; }
        consumed = re.lastIndex;
        if (match[1]) { combinator = '>'; }
        if (!match[2]) { continue; }
        steps.push({ combinator: combinator, compound: parseCompound(match[2], selector) });
        combinator = ' ';
      }
      if (consumed < text.length) {
        throw new Error('find() reads tag, #id, .class and [attr] selectors joined by spaces or >; it cannot read ' + fmt(selector));
      }
      list.push(steps);
    }
    return list;
  }

  function parseCompound(source, whole) {
    const compound = { tag: null, id: null, classes: [], attrs: [] };
    const re = /^([a-zA-Z][\w-]*|\*)|#([\w-]+)|\.([\w-]+)|\[\s*([^\]\s~^$*|=]+)\s*(?:([~^$*]?=)\s*(?:"([^"]*)"|'([^']*)'|([^\]\s]+)))?\s*\]/g;
    let match;
    let consumed = 0;
    while ((match = re.exec(source)) !== null) {
      if (match[0].length === 0) { break; }
      consumed = re.lastIndex;
      if (match[1]) { compound.tag = match[1] === '*' ? null : match[1].toLowerCase(); }
      else if (match[2]) { compound.id = match[2]; }
      else if (match[3]) { compound.classes.push(match[3]); }
      else if (match[4]) {
        const value = match[6] !== undefined ? match[6] : match[7] !== undefined ? match[7] : match[8];
        compound.attrs.push({ name: match[4].toLowerCase(), op: match[5] || null, value: value });
      }
    }
    if (consumed !== source.length) {
      throw new Error('find() cannot read the selector ' + fmt(whole));
    }
    return compound;
  }

  function matchesCompound(node, compound) {
    if (!node || has.call(node, 'text') || node.tag === '#root') { return false; }
    if (compound.tag && node.tag !== compound.tag) { return false; }
    if (compound.id && node.attrs.id !== compound.id) { return false; }
    if (compound.classes.length) {
      const classes = (node.attrs['class'] || '').split(/\s+/);
      for (const name of compound.classes) { if (classes.indexOf(name) === -1) { return false; } }
    }
    for (const attr of compound.attrs) {
      if (!has.call(node.attrs, attr.name)) { return false; }
      const value = node.attrs[attr.name];
      if (attr.op === '=' && value !== attr.value) { return false; }
      if (attr.op === '^=' && !value.startsWith(attr.value)) { return false; }
      if (attr.op === '$=' && !value.endsWith(attr.value)) { return false; }
      if (attr.op === '*=' && value.indexOf(attr.value) === -1) { return false; }
      if (attr.op === '~=' && value.split(/\s+/).indexOf(attr.value) === -1) { return false; }
    }
    return true;
  }

  function matchesSteps(node, steps, index, scope) {
    if (!matchesCompound(node, steps[index].compound)) { return false; }
    if (index === 0) { return true; }
    const combinator = steps[index].combinator;
    let parent = node.parent;
    if (combinator === '>') {
      return parent !== scope && matchesSteps(parent, steps, index - 1, scope);
    }
    while (parent && parent !== scope) {
      if (matchesSteps(parent, steps, index - 1, scope)) { return true; }
      parent = parent.parent;
    }
    return false;
  }

  function queryAll(scope, selector) {
    const list = parseSelector(selector);
    const found = [];
    const walk = function (node) {
      for (const child of node.children) {
        if (has.call(child, 'text')) { continue; }
        if (list.some(function (steps) { return matchesSteps(child, steps, steps.length - 1, scope); })) {
          found.push(child);
        }
        walk(child);
      }
    };
    walk(scope);
    return found;
  }

  class Element {
    constructor(node, source) { this._node = node; this._source = source; }
    get tag() { return this._node.tag; }
    get attrs() { return Object.assign({}, this._node.attrs); }
    attr(name) {
      const key = String(name).toLowerCase();
      return has.call(this._node.attrs, key) ? this._node.attrs[key] : null;
    }
    get text() { return collapse(textContent(this._node, []).join('')); }
    get html() { return this._source.slice(this._node.outerStart, this._node.outerEnd); }
    find(selector) { const all = this.findAll(selector); return all.length ? all[0] : null; }
    findAll(selector) {
      const source = this._source;
      return queryAll(this._node, selector).map(function (node) { return new Element(node, source); });
    }
    toString() { return this.html; }
  }

  class View {
    constructor(html) {
      Object.defineProperty(this, 'html', { value: String(html), enumerable: true });
      Object.defineProperty(this, '_tree', { value: null, writable: true });
    }
    get tree() {
      if (!this._tree) { this._tree = parse(this.html); }
      return this._tree;
    }
    // The words on the page: `<body>`'s text when there is one, scripts and
    // styles left out, whitespace collapsed the way a browser lays it out.
    get text() {
      const body = queryAll(this.tree, 'body')[0];
      return collapse(textContent(body || this.tree, []).join(''));
    }
    find(selector) { const all = this.findAll(selector); return all.length ? all[0] : null; }
    findAll(selector) {
      const html = this.html;
      return queryAll(this.tree, selector).map(function (node) { return new Element(node, html); });
    }
    contains(text) { return this.text.indexOf(String(text)) !== -1; }
    describe() {
      const text = this.text;
      return '<' + this.constructor.name + ' ' + JSON.stringify(text.length > 120 ? text.slice(0, 120) + '…' : text) + '>';
    }
    toString() { return this.html; }
  }

  class Rendered extends View {
    constructor(result) {
      super(result.html);
      this.tier = result.tier;
      this.component = result.component;
    }
  }

  class Page extends View {
    constructor(response, jar) {
      super(response.body);
      this.status = response.status;
      this.url = response.url;
      this.headers = response.headers;
      this.redirects = response.redirects;
      Object.defineProperty(this, '_jar', { value: jar });
    }
    header(name) {
      const key = String(name).toLowerCase();
      return has.call(this.headers, key) ? this.headers[key] : null;
    }
    describe() {
      const text = this.text;
      return '<Page ' + this.status + ' ' + this.url + ' ' + JSON.stringify(text.length > 100 ? text.slice(0, 100) + '…' : text) + '>';
    }

    // Submit a form on this page the way a browser does: its hidden fields and
    // current values, with `fields` filled in, posted with this visitor's
    // cookies, redirects followed. `target` names the action
    // (`<form action="action:sign_guestbook">` → "sign_guestbook") or the
    // form's own action URL.
    submit(target, fields) {
      fields = fields || {};
      const forms = this.findAll('form');
      const form = forms.find(function (candidate) {
        const action = candidate.attr('action') || '';
        return candidate.attr('data-albedo-action') === target ||
          action === target || action === 'action:' + target;
      });
      if (!form) {
        const names = forms.map(function (candidate) {
          return candidate.attr('data-albedo-action') || candidate.attr('action') || '(no action)';
        });
        throw new Error('no form for ' + fmt(target) + ' on ' + this.url +
          (names.length ? ' — the forms here are ' + names.map(function (n) { return fmt(n); }).join(', ') : ' — this page has no forms'));
      }
      const values = {};
      const known = new Set();
      for (const control of form.findAll('input, textarea, select')) {
        const name = control.attr('name');
        if (!name || control.attr('disabled') !== null) { continue; }
        known.add(name);
        const type = (control.attr('type') || '').toLowerCase();
        if (control.tag === 'input') {
          if (type === 'submit' || type === 'button' || type === 'image' || type === 'reset' || type === 'file') { continue; }
          if ((type === 'checkbox' || type === 'radio') && control.attr('checked') === null) { continue; }
          values[name] = control.attr('value') !== null ? control.attr('value') : (type === 'checkbox' || type === 'radio' ? 'on' : '');
        } else if (control.tag === 'textarea') {
          values[name] = control._node.children.map(function (child) { return child.text || ''; }).join('');
        } else {
          const options = control.findAll('option');
          const chosen = options.find(function (option) { return option.attr('selected') !== null; }) || options[0];
          if (chosen) { values[name] = chosen.attr('value') !== null ? chosen.attr('value') : chosen.text; }
        }
      }
      for (const key of Object.keys(fields)) {
        if (!known.has(key)) {
          throw new Error('the ' + fmt(target) + ' form has no field ' + fmt(key) +
            ' — its fields are ' + Array.from(known).filter(function (n) { return n[0] !== '_'; }).map(function (n) { return fmt(n); }).join(', '));
        }
        const value = fields[key];
        values[key] = value === null || value === undefined ? '' : String(value);
      }
      const method = (form.attr('method') || 'GET').toUpperCase();
      const action = form.attr('action') || this.url;
      if (!action.startsWith('/')) {
        throw new Error('the ' + fmt(target) + ' form posts to ' + fmt(action) + ', which is not a path on this app');
      }
      const response = call('request', { jar: this._jar, method: method, path: action, form: values, follow: true });
      return new Page(response, this._jar);
    }
  }

  // ── visitors ───────────────────────────────────────────────────────────
  // Each test starts as a fresh anonymous visitor: new cookies, new address.
  let anonymousJar = null;

  function jarFor(as) {
    if (as === undefined || as === null) {
      if (!anonymousJar) { anonymousJar = call('jar'); }
      return anonymousJar;
    }
    if (as instanceof User) { return as.jar; }
    throw new TypeError('{ as } takes a user from signUp() or signIn() — got ' + fmt(as));
  }

  class User {
    constructor(email, jar) {
      Object.defineProperty(this, 'email', { value: email, enumerable: true });
      Object.defineProperty(this, 'jar', { value: jar });
    }
    toString() { return 'User(' + this.email + ')'; }
  }

  function credentials(name, email, password) {
    if (email && typeof email === 'object') {
      password = email.password;
      email = email.email;
    }
    if (typeof email !== 'string' || typeof password !== 'string') {
      throw new TypeError(name + '(email, password) takes two strings');
    }
    return { email: email, password: password };
  }

  function signUp(email, password) {
    const given = credentials('signUp', email, password);
    const jar = call('jar');
    call('auth', { jar: jar, kind: 'register', email: given.email, password: given.password });
    return new User(given.email, jar);
  }

  function signIn(email, password) {
    const given = credentials('signIn', email, password);
    const jar = call('jar');
    call('auth', { jar: jar, kind: 'login', email: given.email, password: given.password });
    return new User(given.email, jar);
  }

  function signOut(user) {
    if (!(user instanceof User)) { throw new TypeError('signOut(user) takes a user from signUp() or signIn()'); }
    call('auth', { jar: user.jar, kind: 'logout' });
  }

  function visit(path, options) {
    options = options || {};
    if (typeof path !== 'string' || path[0] !== '/') {
      throw new TypeError('visit() takes a path on this app, starting with "/" — got ' + fmt(path));
    }
    const jar = jarFor(options.as);
    const response = call('request', { jar: jar, method: 'GET', path: path, follow: options.follow !== false });
    return new Page(response, jar);
  }

  // ── components ─────────────────────────────────────────────────────────
  // JSX for a component imported from the app does not run it here. It names
  // it — file, export, props — and `render()` hands that to the renderer the
  // app uses for the component's tier: the build's Tier-A renderer, or the
  // server's renderer for Tier B and C. A component test therefore asserts
  // against the markup a visitor would get, not a look-alike.
  const ELEMENT = Symbol('albedo.element');

  function tag(value, module, name) {
    if (typeof value === 'function' && !has.call(value, '__albedo_ref')) {
      try { Object.defineProperty(value, '__albedo_ref', { value: { module: module, export: name } }); } catch (error) { /* frozen */ }
    }
    return value;
  }

  function isProjectModule(specifier) {
    return has.call(globalThis.__ALBEDO_MODULES, specifier) && !globalThis.__albedo_is_npm_module(specifier);
  }

  const importDefault = globalThis.__albedo_import_default;
  globalThis.__albedo_import_default = function (specifier) {
    const value = importDefault(specifier);
    return isProjectModule(specifier) ? tag(value, specifier, 'default') : value;
  };
  const importNamed = globalThis.__albedo_import_named;
  globalThis.__albedo_import_named = function (specifier) {
    if (!isProjectModule(specifier)) { return importNamed(specifier); }
    const record = globalThis.__ALBEDO_MODULES[specifier];
    const out = {};
    for (const key of Object.keys(record)) {
      out[key] = key === 'default' ? record[key] : tag(record[key], specifier, key);
    }
    return out;
  };

  // A project module is loaded the first time something imports it, from the
  // file on disk, through the compiler the server uses.
  const resolveProject = globalThis.__albedo_resolve_project;
  globalThis.__albedo_resolve_project = function (base) {
    const known = resolveProject(base);
    if (has.call(globalThis.__ALBEDO_MODULES, known)) { return known; }
    const loaded = globalThis.__albedo_test_load(String(base));
    return loaded || known;
  };

  const realH = globalThis.h;
  const testH = function (type, props) {
    const children = Array.prototype.slice.call(arguments, 2);
    if (typeof type === 'function' && type.__albedo_ref) {
      return { [ELEMENT]: true, ref: type.__albedo_ref, props: props || {}, children: children };
    }
    return realH.apply(this, arguments);
  };
  for (const key of Object.getOwnPropertyNames(realH)) {
    if (!has.call(testH, key)) {
      try { testH[key] = realH[key]; } catch (error) { /* read-only */ }
    }
  }
  globalThis.h = testH;

  function asData(value, path, name) {
    if (value === null || typeof value === 'string' || typeof value === 'boolean') { return value; }
    if (typeof value === 'number') {
      if (!isFinite(value)) { throw new TypeError(name + ': prop ' + path + ' is ' + value + ', which has no JSON form'); }
      return value;
    }
    if (value === undefined) { return undefined; }
    if (typeof value === 'function') {
      throw new TypeError(name + ': prop ' + path + ' is a function. render() passes props as data, ' +
        'the way a page passes them to a component it bakes; an event handler runs in the browser, ' +
        'so test what it does through the page with visit().');
    }
    if (value && value[ELEMENT]) {
      throw new TypeError(name + ': prop ' + path + ' is a component (<' + componentName(value.ref) + '>). ' +
        'render() takes one component with data props; test a composition through the page that renders it, with visit().');
    }
    if (value instanceof Date) { return value.toISOString(); }
    if (Array.isArray(value)) {
      return value.map(function (item, index) { return asData(item, path + '[' + index + ']', name); });
    }
    if (globalThis.__albedo_is_element && globalThis.__albedo_is_element(value)) {
      throw new TypeError(name + ': prop ' + path + ' is markup. render() passes props as data — ' +
        'text children work (<Card>hello</Card>), markup children do not yet; test that composition with visit().');
    }
    const out = {};
    for (const key of Object.keys(value)) {
      const item = asData(value[key], path + '.' + key, name);
      if (item !== undefined) { out[key] = item; }
    }
    return out;
  }

  function render(element, options) {
    options = options || {};
    if (typeof element === 'function' && element.__albedo_ref) {
      element = { [ELEMENT]: true, ref: element.__albedo_ref, props: {}, children: [] };
    }
    if (typeof element === 'string' || (element && globalThis.__albedo_is_element && globalThis.__albedo_is_element(element))) {
      throw new TypeError('render() was handed markup, not a component. It takes a component imported from ' +
        'your app — render(<Card title="x" />) — and renders it with the renderer the app uses for it.');
    }
    if (!element || !element[ELEMENT]) {
      throw new TypeError('render() takes a component imported from your app, as JSX: render(<Card title="x" />) — got ' + fmt(element));
    }
    const name = 'render(<' + componentName(element.ref) + '>)';
    const props = asData(Object.assign({}, element.props), 'props', name) || {};
    delete props.key;
    delete props.ref;
    if (element.children.length) {
      const forced = element.children.map(function (child) {
        const value = globalThis.__albedo_force_thunk ? globalThis.__albedo_force_thunk(child) : child;
        return typeof value === 'number' ? String(value) : value;
      });
      for (const child of forced) {
        if (typeof child !== 'string') { asData(child, 'children', name); }
      }
      props.children = forced.length === 1 ? forced[0] : forced;
    }
    const host = {};
    if (options.shared !== undefined) { host.shared = asData(options.shared, 'shared', name); }
    if (options.state !== undefined) { host.state = asData(options.state, 'state', name); }
    const result = call('render', {
      module: element.ref.module,
      export: element.ref.export,
      props: props,
      host: Object.keys(host).length ? host : null,
    });
    return new Rendered(result);
  }

  // ── registration ───────────────────────────────────────────────────────
  let tests = [];
  let suites = [{ name: '', skip: false, only: false, beforeEach: [], afterEach: [] }];

  function currentSuite() { return suites[suites.length - 1]; }

  function register(name, fn, mode) {
    if (typeof name !== 'string' || !name) {
      throw new TypeError('test(name, fn) takes a name first — got ' + fmt(name));
    }
    if (mode !== 'todo' && typeof fn !== 'function') {
      throw new TypeError('test(' + fmt(name) + ', fn) takes a function second — got ' + fmt(fn));
    }
    const path = suites.slice(1).map(function (suite) { return suite.name; });
    const inherited = suites.some(function (suite) { return suite.skip; }) ? 'skip'
      : (mode === 'run' && suites.some(function (suite) { return suite.only; })) ? 'only' : mode;
    tests.push({ name: path.concat([name]).join(' › '), fn: fn, mode: inherited, suites: suites.slice() });
  }

  function test(name, fn) { register(name, fn, 'run'); }
  test.skip = function (name, fn) { register(name, fn, 'skip'); };
  test.only = function (name, fn) { register(name, fn, 'only'); };
  test.todo = function (name) { register(name, undefined, 'todo'); };

  function describeWith(flags) {
    return function (name, fn) {
      if (typeof fn !== 'function') { throw new TypeError('describe(' + fmt(name) + ', fn) takes a function second'); }
      suites.push({ name: String(name), skip: Boolean(flags.skip), only: Boolean(flags.only), beforeEach: [], afterEach: [] });
      try {
        const result = fn();
        if (result && typeof result.then === 'function') {
          throw new Error('describe(' + fmt(name) + ') returned a promise. A describe body only registers tests and runs at once; put the awaits inside test().');
        }
      } finally {
        suites.pop();
      }
    };
  }
  const describe = describeWith({});
  describe.skip = describeWith({ skip: true });
  describe.only = describeWith({ only: true });

  function beforeEach(fn) { currentSuite().beforeEach.push(fn); }
  function afterEach(fn) { currentSuite().afterEach.push(fn); }

  globalThis.__albedo_test_reset = function () {
    tests = [];
    suites = [{ name: '', skip: false, only: false, beforeEach: [], afterEach: [] }];
  };

  globalThis.__albedo_test_list = function () {
    const focused = tests.some(function (entry) { return entry.mode === 'only'; });
    return JSON.stringify(tests.map(function (entry) {
      return {
        name: entry.name,
        todo: entry.mode === 'todo',
        skip: entry.mode === 'skip' || entry.mode === 'todo' || (focused && entry.mode !== 'only'),
      };
    }));
  };

  function describeError(error) {
    if (error instanceof AssertionError) { return error.message; }
    if (error && typeof error.message === 'string') {
      const name = error.name && error.name !== 'Error' ? error.name + ': ' : '';
      return name + error.message;
    }
    return 'threw ' + fmt(error);
  }

  globalThis.__albedo_test_run = async function (index) {
    const entry = tests[index];
    anonymousJar = null;
    let failure = null;
    try {
      for (const suite of entry.suites) {
        for (const hook of suite.beforeEach) { await hook(); }
      }
      await entry.fn();
    } catch (error) {
      failure = describeError(error);
    }
    for (let i = entry.suites.length - 1; i >= 0; i--) {
      for (const hook of entry.suites[i].afterEach) {
        try { await hook(); } catch (error) {
          if (failure === null) { failure = 'afterEach: ' + describeError(error); }
        }
      }
    }
    return JSON.stringify(failure === null ? { ok: true } : { ok: false, error: failure });
  };

  globalThis.__albedo_test = Object.freeze({
    test: test,
    it: test,
    describe: describe,
    beforeEach: beforeEach,
    afterEach: afterEach,
    expect: expect,
    render: render,
    visit: visit,
    signUp: signUp,
    signIn: signIn,
    signOut: signOut,
  });
})();
