// PopoverApp (ui/src/entries/PopoverApp.tsx) mounted with react-dom in jsdom, against a stubbed
// `window.__TAURI__`: its invoke records every (cmd, args) and answers from `replies`, and its
// `listen` hands the test the app's `snapshot` callback, so the test pushes snapshots as the app
// does. It runs what a story cannot: the error boundary's recovery on the next snapshot, the
// update wait's override and its clearing, the ⋯ menu's error note, and the "Show N more"
// folds, which need heights jsdom does not compute (stubbed below).
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

// Heights for the fold. A group is 100 px, a compact line 20 and the more row 21; the gauge
// column is 300 px, and the columns are as tall as it (the jobs fold under it). The card is 200 px
// of fixed parts plus the columns. Everything else is 0, as jsdom reports.
const GROUP_H = 100;
const COMPACT_H = 20;
const MORE_H = 21;
const GAUGES_H = 300;
const FIXED_H = 200;
/** What an open gate's lines add to the gauge column. */
const GATE_OPEN_H = 202;
// A test may change these: a taller compact line, a shorter gauge column, one group taller.
const layout = { compact: COMPACT_H, gauges: GAUGES_H, tall: { group: '', height: 0 } };
Object.defineProperty(window.HTMLElement.prototype, 'offsetHeight', {
  configurable: true,
  get() {
    if (this.dataset.group !== undefined) return this.dataset.group === layout.tall.group ? layout.tall.height : GROUP_H;
    if (this.dataset.compact !== undefined) return layout.compact;
    if (this.dataset.more !== undefined) return MORE_H;
    if (this.dataset.gauges !== undefined || this.dataset.columns !== undefined) {
      const open = this.ownerDocument.querySelector('[data-gauges] [aria-expanded="true"]');
      return layout.gauges + (open ? GATE_OPEN_H : 0);
    }
    if (this.tagName === 'SECTION') {
      const columns = this.querySelector('[data-columns]');
      return FIXED_H + (columns ? columns.offsetHeight : 0);
    }
    return 0;
  },
});

let server;
let React;
let act;
let createRoot;
let PopoverApp;
let fx;

/** The stub app: every invoke recorded, answered from `replies`; `push` sends a snapshot. */
const app = {
  calls: [],
  replies: {},
  onSnapshot: null,
  async invoke(cmd, args) {
    app.calls.push([cmd, args]);
    const r = app.replies[cmd];
    if (r instanceof Error) throw r;
    return typeof r === 'function' ? r(args) : r;
  },
};
window.__TAURI__ = {
  core: { invoke: (cmd, args) => app.invoke(cmd, args) },
  event: {
    listen: async (name, cb) => {
      if (name === 'snapshot') app.onSnapshot = cb;
      return () => {};
    },
  },
};

before(async () => {
  React = await import('react');
  ({ act } = React);
  ({ createRoot } = await import('react-dom/client'));
  server = await createServer({
    root: ui,
    configFile: false,
    logLevel: 'silent',
    appType: 'custom',
    server: { middlewareMode: true, hmr: false },
  });
  ({ PopoverApp } = await server.ssrLoadModule('/src/entries/PopoverApp.tsx'));
  fx = await server.ssrLoadModule('/src/popover/fixtures.ts');
});
after(() => server?.close());

let root;
let host;
beforeEach(async () => {
  if (root) await act(async () => root.unmount());
  host?.remove();
  app.calls = [];
  app.replies = { get_snapshot: fx.idle, autostart_enabled: true };
  app.onSnapshot = null;
});

// Settles React, then one animation frame: the window-size report (useWindowSize) is sent from a
// frame, and a frame still pending when a test clears `app.calls` lands in its next assertion (#420).
const flush = async () => {
  await act(async () => {});
  await act(() => new Promise((resolve) => window.requestAnimationFrame(() => resolve())));
};

/** Mounts the popover window, as ui/popover/main.tsx does, and waits for the first snapshot. */
async function mount() {
  host = document.createElement('div');
  document.body.append(host);
  root = createRoot(host);
  await act(async () => root.render(React.createElement(PopoverApp)));
  await flush();
  assert.ok(app.onSnapshot, 'PopoverApp subscribes to the app’s snapshot event');
}

const push = (payload) => act(async () => app.onSnapshot({ payload }));
const text = () => document.body.textContent;
const byText = (selector, t) => [...document.querySelectorAll(selector)].find((n) => n.textContent.trim() === t);

/**
 * The commands a step sent, minus `resize_popover`: the window follows its content
 * asynchronously, so a resize may land after the click that caused it, on a slow runner
 * (seen on CI for #288). Asserting on `calls.at(-1)` raced it.
 */
function sentSince(mark) {
  return app.calls.slice(mark).filter(([cmd]) => cmd !== 'resize_popover');
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

const openMenu = () => click(document.querySelector('button[aria-label="More"]'));
const menuItem = (start) => [...document.querySelectorAll('[role^="menuitem"]')].find((n) => n.textContent.startsWith(start));

/** Runs `fn` with console.error silenced, returning what it would have printed. */
async function quietly(fn) {
  const errors = [];
  const original = console.error;
  console.error = (...a) => errors.push(a);
  try {
    await fn();
  } finally {
    console.error = original;
  }
  return errors;
}

test('a snapshot the screen cannot draw shows the fallback, and the next one draws again', async () => {
  await mount();
  assert.match(text(), /Slots/);
  // A null job: the jobs are read in a try, but the screen's grouping throws on it.
  const bad = { ...fx.running, state: { ...fx.running.state, inflight: [null] } };
  const errors = await quietly(() => push(bad));
  assert.match(text(), /could not draw the daemon’s latest state/);
  assert.ok(errors.some((a) => a[0] === 'popover render failed'), 'the boundary logs what failed');
  const beforePanel = app.calls.length;
  await click(byText('button', 'Open panel'));
  assert.deepEqual(sentSince(beforePanel), [['show_panel', undefined]], 'the fallback still reaches the panel');

  await push(fx.running);
  assert.doesNotMatch(text(), /could not draw/);
  assert.match(text(), /demo\/PR-2741/);
});

test('the update wait shows what the app saved until the snapshot moves, then the snapshot', async () => {
  const at = (floor) => ({ ...fx.idle, app_update: { floor_secs: floor } });
  app.replies.get_snapshot = at(3600);
  app.replies.set_update_floor = (args) => args.secs;
  await mount();
  await openMenu();
  assert.ok(menuItem('App update wait: 1 h'), 'the snapshot’s wait');

  const beforeSave = app.calls.length;
  await click(menuItem('App update wait'));
  assert.deepEqual(sentSince(beforeSave), [['set_update_floor', { secs: 6 * 3600 }]]);
  assert.ok(menuItem('App update wait: 6 h'), 'what the app saved, before the snapshot catches up');

  await push(at(3600));
  assert.ok(menuItem('App update wait: 6 h'), 'a snapshot that has not moved does not undo it');

  await push(at(24 * 3600));
  assert.ok(menuItem('App update wait: 24 h'), 'once the snapshot moves, it wins');

  await push(at(3600));
  assert.ok(menuItem('App update wait: 1 h'), 'and the old override is gone, not waiting to return');
});

test('a failed update-wait save clears the override and leaves an error note in the menu', async () => {
  app.replies.get_snapshot = { ...fx.idle, app_update: { floor_secs: 3600 } };
  app.replies.set_update_floor = new Error('the app refused the wait');
  await mount();
  await openMenu();
  await click(menuItem('App update wait'));
  assert.ok(menuItem('App update wait: 1 h'), 'the label stays on the snapshot’s value');
  assert.match(text(), /the app refused the wait/);
});

test('a login item that will not register says so in the menu, and the note clears on the next press', async () => {
  app.replies.autostart_enabled = false;
  app.replies.set_autostart = new Error('login item refused');
  await mount();
  await openMenu();
  await click(menuItem('Start at login'));
  assert.ok(app.calls.some(([c, a]) => c === 'set_autostart' && a.enabled === true));
  assert.match(text(), /login item refused/);

  await act(async () => {
    document.querySelector('section').dispatchEvent(new window.PointerEvent('pointerdown', { bubbles: true }));
  });
  assert.doesNotMatch(text(), /login item refused/);
});

test('job groups fold at the gauge column into compact lines and "Show N more", which opens in place', async () => {
  const groups = Array.from({ length: 20 }, (_, i) => fx.job({ pr: 3000 + i, elapsed_s: 600 - i }));
  await mount();
  await push({ ...fx.running, state: { ...fx.running.state, inflight: groups } });
  await flush();

  // 288 px of room under the 300 px gauge column (less the jobs column's 12 px padding): two
  // 101 px groups, the 6 px hairline, two 21 px compact lines and the 22 px more row.
  assert.equal(document.querySelectorAll('[data-group]').length, 2);
  assert.equal(document.querySelectorAll('[data-compact]').length, 2);
  const more = document.querySelector('[data-more]');
  assert.match(more.textContent, /Show 16 more/);
  assert.equal(
    more.querySelector('button').getAttribute('aria-label'),
    'Show 16 more: 16 running',
    'the button names its tally in words',
  );
  assert.equal(document.querySelector('[data-group]').dataset.group, 'demo/PR-3000', 'the oldest group shows first');
  assert.equal(
    document.querySelector('[data-compact] [role="group"]').getAttribute('aria-label'),
    'demo/PR-3002, 1 running, 9 minutes',
    'a compact line is named like the full row',
  );
  assert.ok(document.querySelector('section').className.includes('w-popover-wide'), 'jobs show in two columns');

  // Pressing it opens the list in place: every group is on screen, and the panel is not.
  app.calls = [];
  await click(more.querySelector('button'));
  assert.deepEqual(sentSince(0), [], 'opening does not open the panel');
  assert.equal(document.querySelectorAll('[data-group]').length, 2, 'the groups shown in full keep their place');
  assert.equal(document.querySelectorAll('[data-compact]').length, 18, 'every other group is a compact line');
  assert.equal(document.querySelector('[data-more]'), null);
  const less = byText('button', 'Show fewer');
  assert.equal(less.getAttribute('aria-expanded'), 'true');
  const mark = sentSince(0).length;
  await click(byText('button', 'Open panel'));
  assert.deepEqual(sentSince(mark), [['show_panel', undefined]], 'Open panel opens the panel');
  assert.ok(byText('button', 'Show fewer'), 'and does not close the list');
  await click(byText('button', 'Show fewer'));
  assert.equal(document.querySelectorAll('[data-compact]').length, 2, 'Show fewer folds it again');

  // Two groups fit in full: no fold, still two columns.
  await push({ ...fx.running, state: { ...fx.running.state, inflight: groups.slice(0, 2) } });
  await flush();
  assert.equal(document.querySelectorAll('[data-group]').length, 2);
  assert.equal(document.querySelector('[data-folded]'), null);

  // No jobs: the 308 px column.
  await push(fx.idle);
  await flush();
  assert.ok(!document.querySelector('section').className.includes('w-popover-wide'), 'idle is one column');
});

test('a folded runner failure takes the first compact line, and the tally says what is folded', async () => {
  const groups = Array.from({ length: 20 }, (_, i) => fx.job({ pr: 3300 + i, elapsed_s: 600 - i }));
  // The newest group (the last, so the one that would fold last) has a runner failure beside its job.
  const outcomes = [fx.fault({ pr: 3319, ts_ms: Date.now() - 30_000 })];
  await mount();
  await push({ ...fx.running, state: { ...fx.running.state, inflight: groups, outcomes } });
  await flush();
  const first = document.querySelector('[data-compact] [role="group"]');
  assert.match(first.getAttribute('aria-label'), /^demo\/PR-3319, .*runner failed/, 'worst first');
  assert.equal(
    document.querySelector('[data-more] button').getAttribute('aria-label'),
    'Show 16 more: 16 running',
    'the runner failure is a compact line, so the tally holds the rest',
  );
  assert.equal(document.querySelector('[data-group]').dataset.group, 'demo/PR-3300', 'groups shown in full keep their order');
});

test('mounting with defaultJobsOpen opens the folded list once the fold is measured', async () => {
  const groups = Array.from({ length: 20 }, (_, i) => fx.job({ pr: 3500 + i, elapsed_s: 600 - i }));
  const snap = { ...fx.running, state: { ...fx.running.state, inflight: groups } };
  host = document.createElement('div');
  document.body.append(host);
  root = createRoot(host);
  const { Popover } = await server.ssrLoadModule('/src/screens/Popover.tsx');
  await act(async () =>
    root.render(React.createElement(Popover, { snap, now: Date.now(), invoke: app.invoke, defaultJobsOpen: true })),
  );
  await flush();
  await flush();
  assert.ok(byText('button', 'Show fewer'), 'the list is open');
  assert.equal(document.querySelectorAll('[data-group]').length + document.querySelectorAll('[data-compact]').length, 20, 'every group is present');
});

test('focus stays on the fold toggle when it opens and closes, in the jobs column and the attention list', async () => {
  const groups = Array.from({ length: 20 }, (_, i) => fx.job({ pr: 3600 + i, elapsed_s: 600 - i }));
  await mount();
  await push({ ...fx.running, state: { ...fx.running.state, inflight: groups } });
  await flush();
  const jobs = document.querySelector('[data-more] button');
  jobs.focus();
  await click(jobs);
  assert.equal(document.activeElement, jobs, 'the same button is still focused once the list is open');
  assert.ok(jobs.isConnected && jobs.getAttribute('aria-expanded') === 'true');
  await click(jobs);
  assert.equal(document.activeElement, jobs, 'and once it is shut again');
  assert.equal(jobs.getAttribute('aria-expanded'), 'false');

  await push(fx.attention);
  await flush();
  const rows = [...document.querySelectorAll('button')].find((b) => /^Show \d+ more/.test(b.getAttribute('aria-label') ?? ''));
  rows.focus();
  await click(rows);
  assert.equal(document.activeElement, rows);
  await click(rows);
  assert.equal(document.activeElement, rows);
});

test('an open fold closes when the window loses focus, and when nothing is folded', async () => {
  const groups = Array.from({ length: 20 }, (_, i) => fx.job({ pr: 3400 + i, elapsed_s: 600 - i }));
  await mount();
  await push({ ...fx.running, state: { ...fx.running.state, inflight: groups } });
  await flush();
  await click(document.querySelector('[data-more] button'));
  assert.equal(document.querySelectorAll('[data-compact]').length, 18);
  await act(async () => window.dispatchEvent(new window.Event('blur')));
  await flush();
  assert.equal(document.querySelectorAll('[data-compact]').length, 2, 'blur folds it');
  assert.ok(document.querySelector('[data-more]'));

  await click(document.querySelector('[data-more] button'));
  await push({ ...fx.running, state: { ...fx.running.state, inflight: groups.slice(0, 2) } });
  await flush();
  await push({ ...fx.running, state: { ...fx.running.state, inflight: groups } });
  await flush();
  assert.ok(document.querySelector('[data-more]'), 'once nothing was folded, it comes back shut');
});

test('the attention list folds past three rows into "Show N more" with a severity tally, and opens in place', async () => {
  await mount();
  await push(fx.attention);
  await flush();
  const toggle = [...document.querySelectorAll('button')].find((b) => /^Show \d+ more/.test(b.getAttribute('aria-label') ?? ''));
  assert.ok(toggle, 'the attention list is folded');
  assert.match(toggle.getAttribute('aria-label'), /^Show 2 more: \d+ (problem|warning|notice)/);
  const list = document.getElementById(toggle.getAttribute('aria-controls'));
  const rowsOf = () => list.firstElementChild.children.length;
  const before = rowsOf();
  assert.equal(before, 3, 'three rows show');
  app.calls = [];
  await click(toggle);
  assert.deepEqual(sentSince(0), [], 'no panel');
  const open = byText('button', 'Show fewer');
  assert.equal(open.getAttribute('aria-expanded'), 'true');
  assert.equal(rowsOf(), before + 2, 'the folded rows are drawn');
  assert.equal(byText('button', 'Open panel'), undefined, 'the attention fold has no panel link');
  await click(open);
  assert.ok(document.querySelector('button[aria-expanded="false"][aria-label^="Show 2 more"]'));
});

test('an open gate makes the gauge column taller, so the jobs fold later', async () => {
  const groups = Array.from({ length: 20 }, (_, i) => fx.job({ pr: 3200 + i, elapsed_s: 600 - i }));
  await mount();
  await push({ ...fx.running, state: { ...fx.running.state, inflight: groups } });
  await flush();
  assert.equal(document.querySelectorAll('[data-group]').length, 2, 'shut: two groups in full');

  // 202 px more room: two more 101 px groups in full, and the same two compact lines.
  await click(document.querySelector('[data-gauges] button[aria-label^="Memory"]'));
  await flush();
  await flush();
  assert.equal(document.querySelectorAll('[data-group]').length, 4);
  assert.equal(document.querySelectorAll('[data-compact]').length, 2);
  assert.match(document.querySelector('[data-more]').textContent, /Show 14 more/);

  await click(document.querySelector('[data-gauges] button[aria-label^="Memory"]'));
  await flush();
  await flush();
  assert.equal(document.querySelectorAll('[data-group]').length, 2, 'shut again: back to two');
});

test('the fold settles when a compact line is taller than the guess', async () => {
  // A 200 px first group in 258 px of room: after it, the hairline and the more row leave 29 px,
  // one compact line at the 23 px guess but none at the real 31. Without the remembered height
  // the fold flips 1, 0, 1 … until React gives up and the error boundary shows the fallback.
  Object.assign(layout, { compact: 30, gauges: 270, tall: { group: 'demo/PR-3100', height: 200 } });
  try {
    const groups = Array.from({ length: 20 }, (_, i) => fx.job({ pr: 3100 + i, elapsed_s: 600 - i }));
    await mount();
    await push({ ...fx.running, state: { ...fx.running.state, inflight: groups } });
    await flush();
    await flush();
    assert.ok(document.querySelector('[data-columns]'), 'the screen drew, not the fallback');
    assert.equal(document.querySelectorAll('[data-group]').length, 1);
    assert.equal(document.querySelectorAll('[data-compact]').length, 0, 'no compact line fits at its real height');
    assert.match(document.querySelector('[data-more]').textContent, /Show 19 more/);
    await flush();
    assert.equal(document.querySelectorAll('[data-compact]').length, 0, 'and it stays settled');
  } finally {
    Object.assign(layout, { compact: COMPACT_H, gauges: GAUGES_H, tall: { group: '', height: 0 } });
  }
});

test('an open gate shuts when its gauge leaves the snapshot, and stays shut when it returns', async () => {
  await mount();
  const snap = fx.running;
  await push(snap);
  await click(document.querySelector('[data-gauges] button[aria-label^="CPU"]'));
  assert.equal(document.querySelector('[data-gauges] button[aria-label^="CPU"]').getAttribute('aria-expanded'), 'true');

  await push({ ...snap, state: { ...snap.state, budget: { ...snap.state.budget, cpu: null } } });
  assert.equal(document.querySelector('[data-gauges] button[aria-label^="CPU"]'), null, 'no CPU budget, no CPU gate');
  assert.equal(document.querySelector('[aria-expanded="true"]'), null, 'no gate is left open');

  await push(snap);
  const cpu = document.querySelector('[data-gauges] button[aria-label^="CPU"]');
  assert.equal(cpu.getAttribute('aria-expanded'), 'false', 'the gate comes back shut');
});

test('the left column keeps one row open: a VM closes an open gate, and a gate an open VM', async () => {
  await mount();
  await push(fx.running);
  const gate = () => document.querySelector('[data-gauges] button[aria-label^="Memory"]');
  const vm = () => document.querySelector('[data-gauges] button[aria-label^="Linux VM"]');
  await click(gate());
  assert.equal(gate().getAttribute('aria-expanded'), 'true');
  await click(vm());
  assert.equal(vm().getAttribute('aria-expanded'), 'true', 'the VM opens');
  assert.equal(gate().getAttribute('aria-expanded'), 'false', 'and the gate shuts');
  assert.match(text(), /Disk free/);
  await click(gate());
  assert.equal(gate().getAttribute('aria-expanded'), 'true', 'the gate opens');
  assert.equal(vm().getAttribute('aria-expanded'), 'false', 'and the VM shuts');
});

test('an open gate opens Limits, a press applies once after the wait, and Escape returns', async () => {
  app.replies.get_snapshot = fx.limits();
  await mount();
  await push(fx.limits());
  const gate = document.querySelector('button[aria-label^="Memory"]');
  assert.equal(gate.getAttribute('aria-expanded'), 'false');
  assert.ok(!/Adjust limits/.test(text()), 'a shut gate has no Adjust limits');
  await click(gate);
  assert.equal(gate.getAttribute('aria-expanded'), 'true');
  assert.match(text(), /Reserved by jobs/);
  // One gate open at a time.
  await click(document.querySelector('button[aria-label^="CPU"]'));
  assert.equal(gate.getAttribute('aria-expanded'), 'false');
  const adjust = [...document.querySelectorAll('button')].find((b) => b.textContent === 'Adjust limits');
  await click(adjust);
  assert.match(text(), /Engine memory/);
  assert.equal(document.querySelector('button[aria-label="More"][aria-haspopup]'), null, 'the Limits view replaces the header');

  // Two quick presses on Reserve send one set_config with the final value, after the 400 ms wait.
  const reserve = document.querySelector('[role="spinbutton"][aria-label="Reserve"]');
  const before = Number(reserve.getAttribute('aria-valuenow'));
  const mark = app.calls.length;
  await click(reserve.nextElementSibling);
  await click(reserve.nextElementSibling);
  assert.deepEqual(sentSince(mark), [], 'nothing is sent while presses keep coming');
  await act(async () => new Promise((r) => setTimeout(r, 450)));
  assert.deepEqual(sentSince(mark), [['set_config', { reserveMib: before + 1024 }]]);

  await act(async () => {
    reserve.dispatchEvent(new window.KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
  });
  await flush();
  assert.doesNotMatch(text(), /Engine memory/);
  assert.match(text(), /Slots|Memory/);
});

test('the ⋯ menu lists Settings… with ⌘, and it sends open_settings', async () => {
  await mount();
  await openMenu();
  const item = menuItem('Settings…');
  assert.ok(item, 'Settings… is in the ⋯ menu');
  assert.match(item.textContent, /⌘,/);
  assert.ok(menuItem('Limits…').compareDocumentPosition(item) & window.Node.DOCUMENT_POSITION_FOLLOWING, 'below Limits…');
  const mark = app.calls.length;
  await click(item);
  assert.deepEqual(sentSince(mark), [['open_settings', undefined]]);
});
