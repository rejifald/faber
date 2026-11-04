// What each popover control and window event sends to the app, as plain functions: the screen and
// PopoverApp call these, and scripts/wiring.test.mjs runs them against a stubbed invoke and a
// Node EventTarget standing in for the window. No DOM is needed to pin the wiring.
import type { RowAction } from './attention';
import type { Invoke, Snapshot } from './types';

/** One Tauri command and its arguments, as `invoke` takes them. */
export interface Command {
  cmd: string;
  args?: Record<string, unknown>;
}

/** The app's own update wait cycles through these (the daemon refuses under an hour, and so does the app). */
export const FLOOR_PRESETS = [3600, 6 * 3600, 24 * 3600, 72 * 3600];

/** The next update wait after `current`, wrapping to the first. */
export const nextFloor = (current: number | null | undefined) => FLOOR_PRESETS.find((p) => p > (current ?? 0)) ?? FLOOR_PRESETS[0];

/** One live autolimiter knob for `set_config`: the Tauri argument names, `hardCeiling` being "auto", "off" or a number. */
export type ConfigChange = Partial<{
  reserveMib: number;
  headroomMib: number;
  margin: number;
  nativeConcurrency: number;
  hardCeiling: 'auto' | 'off' | number;
}>;

/** Every command a popover control sends. */
export const commands = {
  pause: (paused: boolean): Command => ({ cmd: 'set_paused', args: { paused: !paused } }),
  reconcile: (): Command => ({ cmd: 'reconcile' }),
  showPanel: (): Command => ({ cmd: 'show_panel' }),
  openDashboard: (): Command => ({ cmd: 'open_dashboard' }),
  openSettings: (): Command => ({ cmd: 'open_settings' }),
  quit: (): Command => ({ cmd: 'quit_app' }),
  readAutostart: (): Command => ({ cmd: 'autostart_enabled' }),
  setAutostart: (enabled: boolean): Command => ({ cmd: 'set_autostart', args: { enabled } }),
  setFloor: (current: number | null | undefined): Command => ({ cmd: 'set_update_floor', args: { secs: nextFloor(current) } }),
  row: (a: RowAction): Command => ({ cmd: a.cmd, args: a.args }),
  resize: (width: number, height: number): Command => ({ cmd: 'resize_popover', args: { width, height } }),
  setConfig: (change: ConfigChange): Command => ({ cmd: 'set_config', args: change }),
  snapshot: (): Command => ({ cmd: 'get_snapshot' }),
};

/** Runs a command through `invoke`. */
export const send = (invoke: Invoke, c: Command) => invoke(c.cmd, c.args);

/** The keys of a keydown the shortcuts read. */
export interface KeyPress {
  key: string;
  metaKey?: boolean;
  ctrlKey?: boolean;
  shiftKey?: boolean;
}

/** ⌘⇧F opens the panel, ⌘, opens Settings and ⌘Q quits, with the ⋯ menu shut or open; anything else is not ours. */
export function shortcut(e: KeyPress): Command | null {
  const mod = e.metaKey || e.ctrlKey;
  if (!mod) return null;
  const key = e.key.toLowerCase();
  if (e.shiftKey && key === 'f') return commands.showPanel();
  if (!e.shiftKey && key === 'q') return commands.quit();
  if (!e.shiftKey && key === ',') return commands.openSettings();
  return null;
}

/** Room under the card for its drop shadow, which renders into the transparent window. */
export const SHADOW_ROOM = 52;
/** Room either side of the card for its shadow: the window is the card plus twice this (lib.rs `POPOVER_SIDE`). */
export const SIDE_ROOM = 36;
/** Room under the open ⋯ menu, for its shadow. */
export const MENU_ROOM = 16;

/** The tallest the app lets the popover window grow (`resize_popover` in desktop/src-tauri/src/lib.rs). */
export const WINDOW_MAX = 800;
/** Above the card: room for its beak (`pt-lg`). */
export const ABOVE_CARD = 12;
/** The tallest the card may be and still fit the window with its shadow. */
export const CARD_MAX = WINDOW_MAX - ABOVE_CARD - SHADOW_ROOM;

/** The window's width for a card `cardWidth` wide: the card centred, with its shadow room either side. */
export const windowWidth = (cardWidth: number) => Math.ceil(cardWidth + 2 * SIDE_ROOM);

/** What the jobs column measures for its fold (anatomy § 6). Every height includes the 1 px gap after it. */
export interface JobsRoom {
  /** Each group's measured height in order, or undefined where it was never drawn. */
  heights: Array<number | undefined>;
  /** The gauge column's height, less the jobs column's own padding: where jobs fold. */
  room: number;
  /** The most the jobs column may take under the window's cap, which bounds an open group. */
  cap: number;
  /** The open group's index, or -1. */
  openIndex: number;
  /** A compact line's height. */
  compact: number;
  /** The "+N more groups" row's height. */
  more: number;
  /** The hairline and gap over the compact lines. */
  divider: number;
}

/** Compact lines a fold keeps room for before the more row, so folded groups still read as rows. */
export const COMPACT_MIN = 2;

/**
 * How the two-column jobs column folds (owner, 2026-10-26): `full` groups in full, then `compact`
 * one-line groups, then "Show N more" for the rest, all within the gauge column's
 * height. When compact lines let every group show, they do, with as many in full as that allows.
 * Otherwise groups show in full while they leave room for COMPACT_MIN compact lines and the more
 * row, and compact lines fill what is left. Infinity means all. An open group stays in full: the
 * room grows to hold it, up to the cap. The first group always shows in full.
 */
export function fitJobs({ heights, room, cap, openIndex, compact, more, divider }: JobsRoom): { full: number; compact: number } {
  const known = heights.filter((h): h is number => h !== undefined);
  const guess = known.length ? known.reduce((a, b) => a + b, 0) / known.length : 0;
  const h = heights.map((x) => x ?? guess);
  const n = h.length;
  const sum = (k: number) => h.slice(0, k).reduce((a, b) => a + b, 0);
  const tail = divider + COMPACT_MIN * compact + more;
  let r = room;
  if (openIndex >= 0) r = Math.max(r, sum(openIndex + 1) + (openIndex < n - 1 ? tail : 0));
  r = Math.min(r, cap);
  if (sum(n) <= r) return { full: Infinity, compact: Infinity };
  const least = Math.max(1, openIndex + 1);
  for (let k = n - 1; k >= least; k--) {
    if (sum(k) + divider + (n - k) * compact <= r) return { full: k, compact: Infinity };
  }
  let k = least;
  while (k < n - 1 && sum(k + 1) + tail <= r) k++;
  return { full: k, compact: Math.max(0, Math.floor((r - sum(k) - divider - more) / compact)) };
}

/** The window's height for a card ending at `cardBottom` and an open menu ending at `menuBottom`. */
export const windowHeight = (cardBottom: number, menuBottom: number | null) =>
  Math.ceil(Math.max(cardBottom + SHADOW_ROOM, menuBottom === null ? 0 : menuBottom + MENU_ROOM));

/** The action in flight in each place (a row, the actions row, the menu): its label. */
export type Pending = Readonly<Record<string, string>>;

/** `label` starts in `place`; other places keep theirs. */
export const startPending = (p: Pending, place: string, label: string): Pending => ({ ...p, [place]: label });

/** `label` settles in `place`, unless something newer took that place since. */
export function settlePending(p: Pending, place: string, label: string): Pending {
  if (p[place] !== label) return p;
  const rest = { ...p };
  delete rest[place];
  return rest;
}

/** The app's side of the window, as PopoverApp needs it. */
export interface Host {
  invoke: Invoke;
  /** Subscribes to the app's `snapshot` event; resolves to the unsubscribe. */
  listen?: (name: string, cb: (e: { payload: Snapshot }) => void) => Promise<() => void>;
}

/**
 * Wires the popover window: the cached snapshot now and on every re-show, the app's pushes, the
 * menu shut on blur, and the two shortcuts. Returns the cleanup, which removes every listener and
 * unsubscribes even when the subscription resolves after it.
 */
export function wireWindow(win: EventTarget, host: Host, take: (s: Snapshot) => void, closeMenu: () => void): () => void {
  const refresh = () =>
    send(host.invoke, commands.snapshot()).then(
      (s) => take(s as Snapshot),
      (e: unknown) => console.error('get_snapshot failed', e),
    );
  void refresh();
  let gone = false;
  let unlisten: (() => void) | undefined;
  void host.listen?.('snapshot', (e) => take(e.payload)).then((u) => (gone ? u() : (unlisten = u)));
  const keys = (e: Event) => {
    const c = shortcut(e as unknown as KeyPress);
    if (!c) return;
    e.preventDefault();
    send(host.invoke, c).catch(() => {});
  };
  win.addEventListener('focus', refresh);
  win.addEventListener('blur', closeMenu);
  win.addEventListener('keydown', keys);
  return () => {
    gone = true;
    unlisten?.();
    win.removeEventListener('focus', refresh);
    win.removeEventListener('blur', closeMenu);
    win.removeEventListener('keydown', keys);
  };
}
