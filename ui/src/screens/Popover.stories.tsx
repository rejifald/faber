import type { Meta, StoryObj } from '@storybook/react-vite';
import { waitFor } from 'storybook/test';
import * as fx from '../popover/fixtures';
import type { Invoke } from '../popover/types';
import { CARD_MAX } from '../popover/wiring';
import { Popover, PopoverFallback } from './Popover';

// The popover's @states, one story each. The reference artboards in docs/design/popover/reference/
// hold for the rows and marks; since 2026-10-26 these stories are the layout's reference (two
// columns while jobs show, gauge bars). Every fixture uses owner/demo names (popover/fixtures.ts). Commands
// go to a stub: the menu's login item reads as on, and every other command settles at once.

const invoke: Invoke = async (cmd, args) => {
  if (cmd === 'autostart_enabled') return true;
  if (cmd === 'set_autostart') return args?.enabled;
  return undefined;
};

const meta = {
  title: 'Screens/Popover',
  component: Popover,
  args: { snap: fx.idle, now: fx.NOW, invoke },
} satisfies Meta<typeof Popover>;
export default meta;
type Story = StoryObj<typeof meta>;

/** Before the first snapshot: empty gauge tracks in the 308 px column. */
export const Loading: Story = { args: { snap: null } };

/** Healthy, nothing in flight: the 308 px column, header, the gauge bars stacked, Pause. */
export const Idle: Story = {};

/** Idle with the Linux VM up: the Virtual machines section shows with no job running (vms/anatomy.md § 2). */
export const IdleVm: Story = { args: { snap: fx.idleVm } };

/** The VM supervisor cannot be reached: the last list stays, greyed, and the label says how old it is. */
export const VmsStale: Story = { args: { snap: fx.vmsStale } };

/** Idle, with the one peer row an idle popover shows: a stale one, its count in amber. */
export const IdlePeerStale: Story = { args: { snap: fx.idlePeerStale } };

/** Jobs running: the 620 px card, gauges left and the groups right; memory binds. */
export const Running: Story = { args: { snap: fx.running } };

/** Claims parked at memory: the memory bar binds in amber, the all-waiting group says why. */
export const Waiting: Story = { args: { snap: fx.waiting } };

/** A staged update across both columns, one group open, a peer under "Other hosts" in the gauge column. */
export const Busy: Story = { args: { snap: fx.busy, defaultOpenGroup: 'demo/PR-2741' } };

/**
 * Finished runs (ADR-0044, amended 2026-11-01): done pips whether CI passed or failed, and a fault
 * ring, and the finished-only groups after the live one, newest first. Held still, so the done
 * pips stay as the app shows them between snapshots (Blocks/PipStrip › DoneFading shows the fade).
 */
export const Finished: Story = { parameters: { still: true }, args: { snap: fx.finished } };

/** reference/busy-overflow for the rows (pip-glyphs.png is the pips): one group over six pips shows its job count and a bar. */
export const BusyOverflow: Story = { args: { snap: fx.busyOverflow } };

/**
 * Far more groups than the gauge column holds: the groups that fit, compact lines while they fit,
 * then "Show N more" with a tally of what it hides. The jobs never make the card taller than the gauge column
 * (owner, 2026-10-26); header, attention and Resume always show, and the card never scrolls.
 */
export const BusyBeyondCap: Story = {
  args: { snap: fx.busyBeyondCap },
  play: async ({ canvasElement }) => {
    await waitFor(() => {
      const card = canvasElement.querySelector('section');
      if (!card || card.getBoundingClientRect().height > CARD_MAX) throw new Error('card over the cap');
      const gauges = canvasElement.querySelector('[data-gauges]');
      const columns = canvasElement.querySelector('[data-columns]');
      if (!gauges || !columns || columns.getBoundingClientRect().height > gauges.getBoundingClientRect().height + 1)
        throw new Error('the jobs made the card taller than the gauges');
      if (!canvasElement.querySelector('[data-more]')) throw new Error('no Show N more row');
      if (![...canvasElement.querySelectorAll('button')].some((b) => b.textContent === 'Resume')) throw new Error('no Resume');
    });
  },
};

/**
 * Groups of every worst state past the gauge column: the first in full, then compact lines with
 * the full row's glyph (frozen, waiting), then "Show N more". Blocks/CompactGroup
 * shows each state's line. Held still, as between snapshots.
 */
export const Folded: Story = { parameters: { still: true }, args: { snap: fx.folded } };

/** A group opened in full: the column grows to hold it, up to the window's cap, rather than folding it away. */
export const FoldedGroupOpen: Story = { args: { snap: fx.busyBeyondCap, defaultOpenGroup: 'demo/PR-2741' } };

/**
 * "Show N more" pressed in the jobs column (fold.md): every group, the folded ones as compact
 * lines worst first, scrolling at the height the column folded at with fades at an edge that has
 * more, then "Show fewer" and "Open panel". The card does not grow.
 */
export const FoldedListOpen: Story = { args: { snap: fx.busyBeyondCap, defaultJobsOpen: true } };

/** The attention list past three rows, opened: every row and "Show fewer"; the card grows, up to the window's cap. */
export const AttentionListOpen: Story = { args: { snap: fx.attention, defaultAttentionOpen: true } };

/** reference/paused-frozen: Resume is the filled primary; frozen pips are amber pause bars, still. */
export const PausedFrozen: Story = { args: { snap: fx.pausedFrozen } };

/** More issues than fit, and no jobs: three rows, worst first, then "Show N more" with a severity tally, over the 308 px column. */
export const AttentionRows: Story = { args: { snap: fx.attention } };

/** The mirror down: a red row with how many jobs it affected (the dashboard names them) and its last error in mono. */
export const MirrorDown: Story = {
  args: { snap: fx.mirrorDown },
  play: async ({ canvasElement }) => {
    await waitFor(() => {
      const text = canvasElement.textContent || '';
      if (!text.includes('Mirror down, 3 jobs affected')) throw new Error('no affected count');
      if (text.includes('faber-1001')) throw new Error('runner names in the popover');
    });
  },
};

/** The mirror answering but not caching: an amber row with the reason it gives. */
export const MirrorPassingThrough: Story = { args: { snap: fx.mirrorPassing } };

/** The mirror's image not built: a red row with the image and the build command in mono, like a missing profile image. */
export const MirrorImageMissing: Story = { args: { snap: fx.mirrorImageMissing } };

/** Rows with inline actions: Release, Clear, Install now and Skip. */
export const AttentionActions: Story = { args: { snap: fx.attentionActions } };

/** macOS jobs parked behind a missing VM session and a failed golden image: two rows, each counting what waits. */
export const MacosHeld: Story = { args: { snap: fx.macosHeld } };

/** A class OOM-killed at the most faber grants unasked (ADR-0053): one red row naming it, the other counted. */
export const OomHeld: Story = { args: { snap: fx.oomHeld } };

/** Pause pressed and refused: the reason shows in a status note beside it, until the next interaction. */
export const ActionFailed: Story = {
  args: {
    invoke: async (cmd) => {
      if (cmd === 'set_paused') throw new Error('the daemon refused: draining');
      return undefined;
    },
  },
  play: async ({ canvasElement }) => {
    const pause = [...canvasElement.querySelectorAll('button')].find((b) => b.textContent === 'Pause');
    pause?.click();
    await waitFor(() => {
      if (!canvasElement.querySelector('[role="status"]')?.textContent) throw new Error('no failure note');
    });
  },
};

/**
 * Two places busy at once: Install now in its row and Pause in the actions row. Each shows loading
 * and holds back only its own siblings (Skip, Reconcile); neither settles the other.
 */
export const TwoActionsInFlight: Story = {
  args: { snap: fx.attentionActions, invoke: () => new Promise(() => {}) },
  play: async ({ canvasElement }) => {
    const press = (label: string) =>
      [...canvasElement.querySelectorAll('button')].find((b) => b.textContent === label)?.click();
    press('Install now');
    press('Pause');
    await waitFor(() => {
      if (canvasElement.querySelectorAll('[aria-busy="true"]').length < 2) throw new Error('not both busy');
    });
  },
};

/**
 * ADR-0064: Paused carries no Dismiss (it stops jobs); the refused row does. Two dismissed rows
 * fold into "2 dismissed" under the list, out of its tally.
 */
export const DismissedRows: Story = { args: { snap: fx.dismissed } };

/** The dismissed rows opened: each muted, with its mark and Restore, then Restore all. */
export const DismissedRowsOpen: Story = { args: { snap: fx.dismissed, defaultDismissedOpen: true } };

/** Everything dismissed: no row, only the quiet "2 dismissed" line; the popover reads as idle. */
export const AllDismissed: Story = { args: { snap: fx.allDismissed } };

/** Two trust changes acknowledged fold into one quiet row; the one made since stays amber, with Acknowledge. */
export const TrustAcknowledged: Story = { args: { snap: fx.trustAcked } };

/** The fallback a render error shows until the next snapshot: the panel and Quit stay reachable. */
export const Fallback: Story = { render: () => <PopoverFallback invoke={invoke} /> };

/** The ⋯ menu open over a short popover: version, panel, dashboard, Start at login, update wait, Quit. */
export const MenuOpen: Story = { args: { snap: fx.idle, menuOpen: true, onMenuOpenChange: () => {} } };

/** The Limits view in the 308 px card (settings/anatomy.md § 5): an open gate's Adjust limits or Limits… opens it. */
export const LimitsOpen: Story = { args: { snap: fx.limits(), defaultView: 'limits' } };

/** The ⋯ menu open with Limits… at the top, above Open panel. */
export const MenuOpenLimits: Story = { args: { snap: fx.limits(), menuOpen: true, onMenuOpenChange: () => {} } };

/** A gate row under the pointer: the job group's neutral lift; the bar's track and fill still show. */
export const GateHover: Story = { args: { snap: fx.busy }, parameters: { force: { hover: 'button[aria-label^="Memory"]' } } };

/** A gate row under the keyboard: the focus ring inside the row. */
export const GateFocus: Story = { args: { snap: fx.busy }, parameters: { force: { 'focus-visible': 'button[aria-label^="Memory"]' } } };

/** Memory open in the two-column card: what the bar is made of, then Adjust limits. The column grows, so the jobs fold later. */
export const GateOpen: Story = { args: { snap: fx.running, defaultOpenGate: 'mem' } };

/** The Linux VM open: what it was given and its free disk. */
export const VmOpen: Story = { args: { snap: fx.running, defaultOpenVm: 'linux-0' } };

/** Memory open while claims wait at it: "Room for" is left out, and an amber line says how many wait. */
export const GateOpenWaiting: Story = { args: { snap: fx.waiting, defaultOpenGate: 'mem' } };

/** Slots open: running of the ceiling, free slots, and the ceiling setting. */
export const GateOpenSlots: Story = { args: { snap: fx.ceilingAuto, defaultOpenGate: 'slots' } };

/** CPU open: reserved, free, and what the next job reserves. */
export const GateOpenCpu: Story = { args: { snap: fx.busy, defaultOpenGate: 'cpu' } };

/** Disk open: free on the host, held by running jobs, the floor and the spare above it. */
export const GateOpenDisk: Story = { args: { snap: fx.busy, defaultOpenGate: 'disk' } };

/** A gate open on the idle 308 px card. */
export const IdleGateOpen: Story = { args: { snap: fx.idle, defaultOpenGate: 'mem' } };

/** A gate open beside many groups: the taller gauge column lets more groups show before the fold. */
export const GateOpenFolded: Story = { args: { snap: fx.busyBeyondCap, defaultOpenGate: 'disk' } };

/** reference/offline: collapsed to the dashboard link; the header and ⋯ menu stay. */
export const Offline: Story = { args: { snap: fx.offline } };

/** A gate and a job group open side by side: two lifted blocks, one per column. */
export const GateAndGroupOpen: Story = { args: { snap: fx.busy, defaultOpenGate: 'mem', defaultOpenGroup: 'demo/PR-2741' } };
