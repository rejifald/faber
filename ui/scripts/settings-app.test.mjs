// Settings (ui/src/screens/Settings.tsx) mounted with react-dom in jsdom against a stubbed `fetch`:
// the daemon's /api/settings and /api/settings/raw answer from `replies`, every request is
// recorded, and the session key comes from sessionStorage as the redeem page leaves it. It runs
// what a story cannot: load, edit, save, the 400/403/409 answers, narrowing a trusted actor in the
// form, and the raw editor's text, TOML error and one-draft-at-a-time rule.
import assert from 'node:assert/strict';
import { join } from 'node:path';
import { after, before, beforeEach, test } from 'node:test';
import { JSDOM } from 'jsdom';
import { createServer } from 'vite';

const ui = join(import.meta.dirname, '..');
const dom = new JSDOM('<!doctype html><html><body></body></html>', { pretendToBeVisual: true, url: 'http://localhost/' });
const { window } = dom;
const { document } = window;

// The globals react-dom and Base UI read, set before either loads.
for (const key of [
  'window',
  'document',
  'navigator',
  'Node',
  'Element',
  'HTMLElement',
  'SVGElement',
  'Event',
  'KeyboardEvent',
  'MouseEvent',
  'FocusEvent',
  'MutationObserver',
  'getComputedStyle',
  'requestAnimationFrame',
  'cancelAnimationFrame',
  'DocumentFragment',
  'ShadowRoot',
]) {
  Object.defineProperty(globalThis, key, { value: window[key], configurable: true, writable: true });
}
globalThis.IS_REACT_ACT_ENVIRONMENT = true;
class Observer {
  observe() {}
  unobserve() {}
  disconnect() {}
}
window.ResizeObserver = globalThis.ResizeObserver = Observer;
window.IntersectionObserver = globalThis.IntersectionObserver = Observer;
window.matchMedia = () => ({ matches: false, addEventListener() {}, removeEventListener() {}, addListener() {}, removeListener() {} });
window.PointerEvent ??= window.MouseEvent;
globalThis.PointerEvent = window.PointerEvent;
window.HTMLElement.prototype.scrollIntoView ??= () => {};


for (const key of ['sessionStorage', 'location']) {
  Object.defineProperty(globalThis, key, { value: window[key], configurable: true, writable: true });
}
window.sessionStorage.setItem('faber-key', 'k-123');
window.navigator.clipboard ??= { writeText: async () => {} };

let server;
let React;
let act;
let createRoot;
let Settings;
let view;

/** Requests the stub saw: {method, url, headers, body}. */
let requests = [];
/** Answers by `METHOD path` (query ignored); a function gets the request. Default: 404. */
let replies = {};
globalThis.fetch = async (url, init = {}) => {
  const req = { method: init.method ?? 'GET', url: String(url), headers: init.headers ?? {}, body: init.body ? JSON.parse(init.body) : undefined };
  requests.push(req);
  const r = replies[`${req.method} ${req.url.split('?')[0]}`];
  const out = typeof r === 'function' ? r(req) : r;
  if (!out) return new Response('not found', { status: 404 });
  return new Response(JSON.stringify(out.body), { status: out.status ?? 200, headers: { 'Content-Type': 'application/json' } });
};

before(async () => {
  React = await import('react');
  ({ act } = React);
  ({ createRoot } = await import('react-dom/client'));
  server = await createServer({ root: ui, configFile: false, logLevel: 'silent', appType: 'custom', server: { middlewareMode: true, hmr: false } });
  ({ Settings } = await server.ssrLoadModule('/src/screens/Settings.tsx'));
  const fx = await server.ssrLoadModule('/src/settings/fixtures.ts');
  view = { ...fx.settingsView, base: 'b1' };
});
after(() => server?.close());

const RAW_TEXT = ['# faber: watched repos carry the topic below.', 'topic = "faber"', 'poll_interval_secs = 30', '', '[autolimit]', 'headroom_mib = 1536', ''].join('\n');

let root;
let host;
beforeEach(async () => {
  if (root) await act(async () => root.unmount());
  host?.remove();
  window.location.hash = '';
  requests = [];
  replies = {
    'GET /api/settings': { body: view },
    'GET /api/settings/raw': { body: { path: view.path, text: RAW_TEXT, base: 'r1' } },
  };
});

const flush = () => act(async () => {});
const text = () => document.body.textContent;
const byText = (selector, t) => [...document.querySelectorAll(selector)].find((n) => n.textContent.trim() === t);
const posts = () => requests.filter((r) => r.method === 'POST');

async function mount() {
  host = document.createElement('div');
  document.body.append(host);
  root = createRoot(host);
  await act(async () => root.render(React.createElement(Settings)));
  await flush();
}

async function click(el) {
  assert.ok(el, 'the element to click is drawn');
  await act(async () => {
    el.dispatchEvent(new window.PointerEvent('pointerdown', { bubbles: true, button: 0 }));
    el.dispatchEvent(new window.MouseEvent('mousedown', { bubbles: true, button: 0 }));
    el.dispatchEvent(new window.PointerEvent('pointerup', { bubbles: true, button: 0 }));
    el.dispatchEvent(new window.MouseEvent('mouseup', { bubbles: true, button: 0 }));
    el.dispatchEvent(new window.MouseEvent('click', { bubbles: true, button: 0 }));
  });
  await flush();
}

/** Types into an input or textarea the way a user does: the native setter, then an input event. */
async function type(el, value) {
  assert.ok(el, 'the field to type into is drawn');
  const proto = el.tagName === 'TEXTAREA' ? window.HTMLTextAreaElement.prototype : window.HTMLInputElement.prototype;
  await act(async () => {
    Object.getOwnPropertyDescriptor(proto, 'value').set.call(el, value);
    el.dispatchEvent(new window.Event('input', { bubbles: true }));
  });
}

/** The control group of a field row, found by its label. */
const group = (label) => [...document.querySelectorAll('[role="group"]')].find((g) => document.getElementById(g.getAttribute('aria-labelledby'))?.textContent === label);
const navLink = (title) => [...document.querySelectorAll('nav a')].find((a) => a.firstChild?.textContent === title);
const wait = (ms) => act(async () => new Promise((r) => setTimeout(r, ms)));

/** A saved answer: the view with `edit` applied to its config. */
const saved = (edit, base = 'b2') => {
  const next = structuredClone(view);
  edit(next.config);
  return { body: { saved: true, errors: [], base, settings: { ...next, base } } };
};

test('load renders the nav and the first section from the schema', async () => {
  await mount();
  assert.deepEqual(requests.map((r) => `${r.method} ${r.url}`), ['GET /api/settings']);
  assert.equal(requests[0].headers['X-Faber-Session'], 'k-123');
  for (const t of ['Limits', 'Admission tuning', 'Docker host', 'Trust', 'Profiles', 'Daemon', 'Updates', 'config.toml']) {
    assert.ok(navLink(t), `${t} is in the nav`);
  }
  assert.equal(navLink('Limits').getAttribute('aria-current'), 'page');
  assert.equal(navLink('config.toml'), [...document.querySelectorAll('nav a')].at(-1), 'config.toml is last');
  assert.ok(group('Headroom'), 'a Limits field is drawn');
  assert.equal(group('CPU overcommit'), undefined, 'a tuning field is not on Limits');
  await click(navLink('Admission tuning'));
  assert.ok(group('CPU overcommit'), 'it is on Admission tuning');
});

test('editing a field and saving posts the whole draft with the session key and base', async () => {
  replies['POST /api/settings'] = saved((c) => (c.autolimit.headroom_mib = 1024));
  await mount();
  await type(group('Headroom').querySelector('input'), '1024');
  assert.match(text(), /1 unsaved change\. Applies now\./);
  await click(byText('button', 'Save'));
  assert.equal(posts().length, 1);
  const [req] = posts();
  assert.equal(view.base, 'b1', 'GET /api/settings sent the base');
  assert.equal(req.url, '/api/settings?base=b1', 'the save sends GET\'s base');
  assert.equal(req.headers['X-Faber-Session'], 'k-123');
  assert.equal(req.body.autolimit.headroom_mib, 1024);
  assert.equal(req.body.topic, 'faber', 'the whole config goes, not the change');
  assert.doesNotMatch(text(), /unsaved change/, 'the bar closes after the save');
  assert.equal(group('Headroom').querySelector('input').value, '1024');
});

test('a 400 marks the field its error names and keeps the draft', async () => {
  replies['POST /api/settings'] = { status: 400, body: { saved: false, errors: ['Headroom must be a whole number of MiB.'], settings: view } };
  await mount();
  await type(group('Headroom').querySelector('input'), '1024');
  await click(byText('button', 'Save'));
  assert.match(group('Headroom').textContent, /Headroom must be a whole number of MiB\./);
  assert.equal(group('Reserve').querySelector('[role="alert"]'), null, 'other fields are not marked');
  assert.match(text(), /1 unsaved change/);
});

test('a 403 shows the refused notice with its sentence and command', async () => {
  const refused = [{ key: 'gate.allow_public_repos', sentence: 'allow_public_repos turns on, so anyone can queue jobs.', needs: 'root', command: 'sudo faber config apply config.toml' }];
  replies['POST /api/settings'] = { status: 403, body: { saved: false, errors: [], refused, settings: view } };
  await mount();
  await type(group('Headroom').querySelector('input'), '1024');
  await click(byText('button', 'Save'));
  assert.match(text(), /Not saved: this widens trust/);
  assert.match(text(), /allow_public_repos turns on/);
  assert.ok([...document.querySelectorAll('code')].some((c) => c.textContent === 'sudo faber config apply config.toml'));
});

test('a 409 reloads the file and says it changed', async () => {
  replies['POST /api/settings'] = { status: 409, body: { saved: false, errors: ['config changed on disk'], settings: view } };
  await mount();
  await type(group('Headroom').querySelector('input'), '1024');
  await click(byText('button', 'Save'));
  await flush();
  assert.match(text(), /Changed on disk meanwhile/);
  assert.equal(group('Headroom').querySelector('input').value, '1536', 'the draft is dropped');
  assert.equal(requests.filter((r) => r.method === 'GET').length, 2, 'the view was read again');
});

test('removing a trusted actor sends the narrowed list; removing the last one is refused in the form', async () => {
  replies['POST /api/settings'] = saved((c) => (c.trusted_actors = ['owner']));
  await mount();
  await click(navLink('Trust'));
  await click(document.querySelector('button[aria-label="Remove release-bot"]'));
  assert.match(text(), /1 unsaved change\. Applies on restart\./);

  await click(document.querySelector('button[aria-label="Remove owner"]'));
  assert.match(text(), /Removing the last one trusts everyone/);
  assert.match(text(), /1 unsaved change/, 'the second removal changed nothing');

  await click(byText('button', 'Save'));
  assert.deepEqual(posts()[0].body.trusted_actors, ['owner']);
});

test('a profile is added last, named, and refused before posting when the name is taken', async () => {
  await mount();
  await click(navLink('Profiles'));
  await click(byText('button', 'Add profile'));
  assert.ok(group('Name'), 'the new profile opens');
  await type(group('Name').querySelector('input'), 'linux');
  await click(byText('button', 'Save'));
  assert.deepEqual(posts(), [], 'nothing is sent');
  assert.match(group('Name').textContent, /A profile called linux already exists\./);
  await type(group('Name').querySelector('input'), 'fresh');
  replies['POST /api/settings'] = saved((c) => c.profile.push({ name: 'fresh', labels: ['faber'], backend: 'docker', dind: false }));
  await click(byText('button', 'Save'));
  const sent = posts()[0].body.profile;
  assert.equal(sent.length, 4);
  assert.deepEqual(sent[3], { name: 'fresh', labels: ['faber'], backend: 'docker', dind: false });
});

test('a blank optional field with unstated_saves is written out at save time', async () => {
  replies['POST /api/settings'] = saved((c) => (c.autolimit.headroom_mib = 1024));
  await mount();
  await click(navLink('Docker host'));
  assert.ok(group('Pull unqualified images'));
  await click(navLink('Limits'));
  await type(group('Headroom').querySelector('input'), '1024');
  await click(byText('button', 'Save'));
  assert.equal(posts()[0].body.host.pull_unqualified_images, true, 'unstated_saves 1 saves true');
});

test('raw: a comment survives in the text sent, after a dry run shows what applies', async () => {
  replies['POST /api/settings/raw'] = (req) => {
    const next = structuredClone(view);
    next.config.poll_interval_secs = 40;
    const dry = req.url.includes('dry_run');
    // As the daemon sends it: `settings` is the current config, `preview` what the text would save.
    return { body: { saved: !dry, errors: [], base: 'r2', ...(dry ? { preview: next.config, settings: view } : { settings: { ...next, base: 'r2' } }) } };
  };
  await mount();
  await click(navLink('config.toml'));
  const area = document.querySelector('textarea[aria-label="config.toml"]');
  assert.equal(area.value, RAW_TEXT);
  const edited = RAW_TEXT.replace('poll_interval_secs = 30', 'poll_interval_secs = 40');
  await type(area, edited);
  assert.match(text(), /1 line changed\./);
  await wait(700);
  const dry = posts()[0];
  assert.match(dry.url, /^\/api\/settings\/raw\?base=r1&dry_run=1$/);
  assert.match(text(), /1 line changed\. Poll interval on restart\./, 'the effect text comes from preview, not settings.config');
  await click(byText('button', 'Save'));
  const real = posts().at(-1);
  assert.equal(real.url, '/api/settings/raw?base=r1');
  assert.equal(real.headers['X-Faber-Session'], 'k-123');
  assert.equal(real.body.text, edited);
  assert.ok(real.body.text.startsWith('# faber: watched repos carry the topic below.'), 'the comment is kept');
  assert.equal(document.querySelector('textarea[aria-label="config.toml"]').value, edited);
  assert.doesNotMatch(text(), /lines? changed/);
});

test('raw: a TOML error marks its line', async () => {
  replies['POST /api/settings/raw'] = { status: 400, body: { line: 3, column: 21, message: 'expected a number after poll_interval_secs =' } };
  await mount();
  await click(navLink('config.toml'));
  await type(document.querySelector('textarea[aria-label="config.toml"]'), RAW_TEXT.replace('= 30', '= 30 s'));
  await wait(700);
  assert.match(text(), /Line 3: expected a number after poll_interval_secs =/);
  const tinted = [...document.querySelectorAll('.bg-red-soft')].filter((n) => n.getAttribute('aria-hidden') !== null || n.parentElement?.getAttribute('aria-hidden') !== null);
  assert.ok(tinted.length >= 1, 'a line is tinted red');
  assert.equal(document.querySelector('textarea[aria-label="config.toml"]').getAttribute('aria-invalid'), 'true');
});

test('switching to the text with a dirty form shows the blocking notice, and Discard clears it', async () => {
  await mount();
  await type(group('Headroom').querySelector('input'), '1024');
  await click(navLink('config.toml'));
  assert.match(text(), /Form changes not saved/);
  assert.equal(document.querySelector('textarea[aria-label="config.toml"]'), null, 'no editor while the form is dirty');
  await click([...document.querySelectorAll('section button')].find((b) => b.textContent === 'Discard'));
  assert.doesNotMatch(text(), /Form changes not saved/);
  assert.ok(document.querySelector('textarea[aria-label="config.toml"]'), 'the editor shows once the form is clean');
});

test('the form is blocked while the text has unsaved changes', async () => {
  await mount();
  await click(navLink('config.toml'));
  await type(document.querySelector('textarea[aria-label="config.toml"]'), `${RAW_TEXT}# note\n`);
  await click(navLink('Limits'));
  assert.match(text(), /Text changes not saved/);
  assert.equal(group('Headroom'), undefined);
});

test('a daemon that cannot load shows the pane with Retry, and Retry loads', async () => {
  replies['GET /api/settings'] = { status: 500, body: {} };
  await mount();
  assert.match(text(), /Can’t load settings/);
  replies['GET /api/settings'] = { body: view };
  await click(byText('button', 'Retry'));
  assert.ok(group('Headroom'));
});

test('a 401 shows the session-ended notice once, with why, and does not reload', async () => {
  // jsdom's own reload cannot be replaced, so the page's global `location` is swapped for one that counts.
  let reloads = 0;
  const real = globalThis.location;
  const counting = {
    get hash() {
      return real.hash;
    },
    set hash(v) {
      real.hash = v;
    },
    reload: () => reloads++,
  };
  Object.defineProperty(globalThis, 'location', { value: counting, configurable: true, writable: true });
  window.sessionStorage.setItem('faber-since', String(Date.now() - 60_000));
  try {
    replies['GET /api/settings'] = { status: 401, body: {} };
    await mount();
    await wait(50);
    assert.match(text(), /Session ended/);
    assert.match(text(), /The daemon restarted \(an update or a restart\) — run faber dashboard or open it from the menu-bar app\./);
    assert.equal(document.querySelectorAll('[role="alert"]').length, 1);
    assert.equal(reloads, 0);
    assert.equal(requests.length, 1, 'no retry loop');
    assert.ok(!byText('button', 'Retry'));
  } finally {
    window.sessionStorage.removeItem('faber-since');
    Object.defineProperty(globalThis, 'location', { value: real, configurable: true, writable: true });
  }
});

test('a session older than 12 h says so', async () => {
  window.sessionStorage.setItem('faber-since', String(Date.now() - 13 * 3600 * 1000));
  try {
    replies['GET /api/settings'] = { status: 401, body: {} };
    await mount();
    assert.match(text(), /Session older than 12 h/);
  } finally {
    window.sessionStorage.removeItem('faber-since');
  }
});
