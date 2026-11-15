// The popover's wiring (ui/src/popover/wiring.ts) against a stubbed invoke that records each
// (cmd, args), and a Node EventTarget standing in for the window: what each control sends, the
// shortcuts, the window height, per-place pending, and that every listener is removed on cleanup.
import assert from 'node:assert/strict';
import { join } from 'node:path';
import { after, before, test } from 'node:test';
import { createServer } from 'vite';

const ui = join(import.meta.dirname, '..');
let server;
let w;

before(async () => {
  server = await createServer({
    root: ui,
    configFile: false,
    logLevel: 'silent',
    appType: 'custom',
    server: { middlewareMode: true, hmr: false },
  });
  w = await server.ssrLoadModule('/src/popover/wiring.ts');
});
after(() => server?.close());

/** An invoke that records every call and answers from `replies`. */
function recorder(replies = {}) {
  const calls = [];
  const invoke = async (cmd, args) => {
    calls.push([cmd, args]);
    const r = replies[cmd];
    if (r instanceof Error) throw r;
    return typeof r === 'function' ? r(args) : r;
  };
  return { calls, invoke };
}

const tick = () => new Promise((r) => setTimeout(r, 0));

test('each control sends its command with the arguments the app reads', async () => {
  const { calls, invoke } = recorder();
  const row = { label: 'Clear', cmd: 'clear_repo_hold', args: { repo: 'owner/demo' } };
  for (const c of [
    w.commands.pause(false),
    w.commands.pause(true),
    w.commands.reconcile(),
    w.commands.row(row),
    w.commands.setAutostart(true),
    w.commands.setFloor(3600),
    w.commands.setFloor(72 * 3600),
    w.commands.setFloor(null),
    w.commands.resize(692, 255),
    w.commands.showPanel(),
    w.commands.openDashboard(),
    w.commands.openSettings(),
    w.commands.quit(),
    w.commands.setConfig({ reserveMib: 2560 }),
    w.commands.setConfig({ hardCeiling: 'auto' }),
  ]) {
    await w.send(invoke, c);
  }
  assert.deepEqual(calls, [
    ['set_paused', { paused: true }],
    ['set_paused', { paused: false }],
    ['reconcile', undefined],
    ['clear_repo_hold', { repo: 'owner/demo' }],
    ['set_autostart', { enabled: true }],
    ['set_update_floor', { secs: 6 * 3600 }],
    ['set_update_floor', { secs: 3600 }],
    ['set_update_floor', { secs: 3600 }],
    ['resize_popover', { width: 692, height: 255 }],
    ['show_panel', undefined],
    ['open_dashboard', undefined],
    ['open_settings', undefined],
    ['quit_app', undefined],
    ['set_config', { reserveMib: 2560 }],
    ['set_config', { hardCeiling: 'auto' }],
  ]);
});

test('⌘⇧F opens the panel, ⌘, opens Settings, ⌘Q quits, and nothing else is a shortcut', () => {
  assert.deepEqual(w.shortcut({ key: ',', metaKey: true }), { cmd: 'open_settings' });
  assert.equal(w.shortcut({ key: ',', metaKey: true, shiftKey: true }), null);
  assert.deepEqual(w.shortcut({ key: 'F', metaKey: true, shiftKey: true }), { cmd: 'show_panel' });
  assert.deepEqual(w.shortcut({ key: 'q', metaKey: true }), { cmd: 'quit_app' });
  assert.equal(w.shortcut({ key: 'q', metaKey: true, shiftKey: true }), null);
  assert.equal(w.shortcut({ key: 'f', shiftKey: true }), null);
  assert.equal(w.shortcut({ key: 'r', metaKey: true }), null);
});

test('the window is as tall as the card plus its shadow, or the open menu plus its own', () => {
  assert.equal(w.windowHeight(203, null), 203 + w.SHADOW_ROOM);
  assert.equal(w.windowHeight(120.2, 300), 300 + w.MENU_ROOM);
  assert.equal(w.windowHeight(203, 100), 203 + w.SHADOW_ROOM);
});

test('two places in flight never settle each other', () => {
  let p = {};
  p = w.startPending(p, 'row:upd-staged', 'Install now');
  p = w.startPending(p, 'actions', 'pause');
  p = w.settlePending(p, 'actions', 'pause');
  assert.deepEqual(p, { 'row:upd-staged': 'Install now' });
  // A late settle of an older action in the same place leaves the newer one.
  p = w.startPending(p, 'row:upd-staged', 'Skip');
  p = w.settlePending(p, 'row:upd-staged', 'Install now');
  assert.deepEqual(p, { 'row:upd-staged': 'Skip' });
});

test('the window wiring: snapshot now, on focus and on push; menu shut on blur; shortcuts', async () => {
  const snap = { online: false, base_url: 'http://127.0.0.1:8787' };
  const { calls, invoke } = recorder({ get_snapshot: snap });
  let push;
  let unsubscribed = 0;
  const listen = async (name, cb) => {
    assert.equal(name, 'snapshot');
    push = cb;
    return () => unsubscribed++;
  };
  const taken = [];
  let closed = 0;
  const win = new EventTarget();
  const cleanup = w.wireWindow(win, { invoke, listen }, (s) => taken.push(s), () => closed++);
  await tick();
  assert.deepEqual(taken, [snap]);

  win.dispatchEvent(new Event('focus'));
  await tick();
  push({ payload: { online: true, state: {} } });
  assert.equal(taken.length, 3);

  win.dispatchEvent(new Event('blur'));
  assert.equal(closed, 1);

  const key = Object.assign(new Event('keydown', { cancelable: true }), { key: 'q', metaKey: true });
  win.dispatchEvent(key);
  assert.ok(key.defaultPrevented);
  await tick();
  assert.deepEqual(calls.at(-1), ['quit_app', undefined]);

  cleanup();
  assert.equal(unsubscribed, 1);
  const before = calls.length;
  win.dispatchEvent(new Event('focus'));
  win.dispatchEvent(new Event('blur'));
  win.dispatchEvent(Object.assign(new Event('keydown'), { key: 'q', metaKey: true }));
  await tick();
  assert.equal(calls.length, before, 'no command after cleanup');
  assert.equal(closed, 1, 'no blur handler after cleanup');
});

test('a subscription that resolves after cleanup is undone at once', async () => {
  const { invoke } = recorder({ get_snapshot: { online: false } });
  let resolve;
  let unsubscribed = 0;
  const listen = () => new Promise((r) => (resolve = r));
  const cleanup = w.wireWindow(new EventTarget(), { invoke, listen }, () => {}, () => {});
  cleanup();
  resolve(() => unsubscribed++);
  await tick();
  assert.equal(unsubscribed, 1);
});

test('the window is the card plus its shadow room either side: 380 or 692 px (lib.rs POPOVER_W, POPOVER_W_WIDE)', () => {
  assert.equal(w.windowWidth(308), 380);
  assert.equal(w.windowWidth(620), 692);
});

test('the jobs column folds at the gauge column: full groups, then compact lines, then the more row', () => {
  const base = { cap: 700, openIndex: -1, compact: 21, more: 23, divider: 6 };
  const height = ({ full, compact }, heights) =>
    heights.slice(0, full).reduce((a, b) => a + b, 0) + base.divider + compact * base.compact + base.more;
  // Everything fits: nothing folds.
  assert.deepEqual(w.fitJobs({ ...base, heights: [40, 40], room: 200 }), { full: Infinity, compact: Infinity });
  // Six 40 px groups in 200 px: three in full and three compact lines show them all, with no more row.
  assert.deepEqual(w.fitJobs({ ...base, heights: Array(6).fill(40), room: 200 }), { full: 3, compact: Infinity });
  // Twenty: full rows while two compact lines and the more row still fit, then compact lines.
  const many = Array(20).fill(40);
  const fold = w.fitJobs({ ...base, heights: many, room: 200 });
  assert.deepEqual(fold, { full: 3, compact: 2 });
  assert.ok(height(fold, many) <= 200, 'never taller than the gauge column');
  // Unknown heights count as the average of the known ones.
  assert.deepEqual(w.fitJobs({ ...base, heights: [40, undefined, 40], room: 110 }), { full: 2, compact: Infinity });
});

test('the cap bounds the fold when the gauge column is taller than the window allows', () => {
  const heights = Array.from({ length: 24 }, (_, i) => (i % 4 === 0 ? 52 : 33));
  const fold = w.fitJobs({ heights, room: 2000, cap: 500, openIndex: -1, compact: 21, more: 23, divider: 6 });
  const shown = heights.slice(0, fold.full).reduce((a, b) => a + b, 0) + 6 + fold.compact * 21 + 23;
  assert.ok(shown <= 500, `${shown} px over the 500 px cap: Pause would be cut off`);
  assert.equal(w.WINDOW_MAX, 800, 'resize_popover clamps the window to 800 (desktop/src-tauri/src/lib.rs)');
});

test('an open group stays in full, and the first group always shows', () => {
  const base = { compact: 21, more: 23, divider: 6, heights: [40, 40, 300, 40] };
  assert.deepEqual(w.fitJobs({ ...base, room: 200, cap: 700, openIndex: -1 }), { full: 2, compact: Infinity });
  assert.deepEqual(w.fitJobs({ ...base, room: 200, cap: 700, openIndex: 2 }), { full: Infinity, compact: Infinity });
  assert.equal(w.fitJobs({ ...base, room: 200, cap: 300, openIndex: 2 }).full, 3, 'the open group shows even past the cap');
  assert.equal(w.fitJobs({ ...base, heights: [500, 40], room: 200, cap: 700, openIndex: -1 }).full, 1);
});

test('every command the popover can send is registered with the app', async () => {
  // A command missing from `generate_handler!` fails only at click time, as "command … not found"
  // (set_pin did: the OOM-held row's Pin). Every literal `cmd:` the popover sends must be registered.
  const { readFile } = await import('node:fs/promises');
  const lib = await readFile(join(ui, '../desktop/src-tauri/src/lib.rs'), 'utf8');
  const handler = lib.match(/generate_handler!\[([^\]]*)\]/);
  assert.ok(handler, 'generate_handler! not found in desktop/src-tauri/src/lib.rs');
  const registered = new Set(handler[1].split(',').map((s) => s.trim()).filter(Boolean));
  const sent = new Set();
  for (const f of ['src/popover/wiring.ts', 'src/popover/attention.ts']) {
    for (const m of (await readFile(join(ui, f), 'utf8')).matchAll(/cmd: '([a-z_]+)'/g)) sent.add(m[1]);
  }
  assert.ok(sent.has('set_pin') && sent.has('set_paused'), 'the scan found the popover commands');
  assert.deepEqual([...sent].filter((c) => !registered.has(c)), []);
});
