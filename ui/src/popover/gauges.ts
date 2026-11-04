// The gauges (popover.md, Hero): one bar per gate in force, the binding one marked.
import { gib1 } from './format';
import type { Budget, DaemonState } from './types';

export type GaugeKey = 'slots' | 'mem' | 'cpu' | 'disk';

export interface GaugeModel {
  key: GaugeKey;
  /** The gate's name: "Slots", "Memory", "Memory p75", "CPU", "Disk". */
  caption: string;
  /** The gate's real figures, beside the caption: "3 / 6", "5.5 / 8.0 GiB", "120 GiB spare". */
  value: string;
  /** Slots only: one segment per slot, `filled` of them running. */
  segs?: number;
  filled?: number;
  /** Every other gate: how full, 0 to 1. */
  fill?: number;
  /** Where the next typical job would no longer fit, 0 to 1 from the bar's start; null for none. */
  mark?: number | null;
  /** CPU not in `auto`: drawn faint, as a gate that holds nothing. */
  off?: boolean;
  /** The binding gate: `ok` takes the accent, `warn` (something waits) amber; its caption gains "binds". */
  bind?: 'ok' | 'warn';
  /** The facts in words, for a screen reader and the hover title. */
  aria: string;
}

/**
 * The disk gate's charge for a light (non-dind) Docker job, in MiB: the daemon's DISK_CHARGE_MIB
 * in src/limiter.rs. The disk gauge's fit mark sits where one more such job would cross the floor.
 */
const DISK_CHARGE_MIB = 3 * 1024;

/** Gate key (WaitView, HeadroomView) to the gauge that shows it. Any other gate highlights none. */
const GAUGE_OF_GATE: Record<string, GaugeKey> = {
  ceiling: 'slots',
  memory: 'mem',
  'vm-memory': 'mem',
  cpu: 'cpu',
  'cpu-pressure': 'cpu',
  disk: 'disk',
};

const clamp01 = (x: number) => Math.max(0, Math.min(1, Number.isFinite(x) ? x : 0));
/** A fit mark only where it means something: strictly inside the bar. */
const fitMark = (f: number) => (Number.isFinite(f) && f > 0 && f < 1 ? f : null);

/**
 * The gate the gauges mark as binding, and whether something is parked at it. With claims parked,
 * the gate most are parked at (the daemon's tally, else the parked rows' own gates); otherwise
 * `budget.headroom.binds`. An older daemon without headroom marks none. The JS never estimates.
 */
export function bindingGate(s: DaemonState): { gate: string | null; parked: boolean } {
  const b = s.budget || {};
  const parked = (s.inflight || []).filter((j) => !j.admitted);
  if (parked.length) {
    let gate: string | null = null;
    let best = 0;
    for (const [g, c] of Object.entries(b.gates || {})) {
      if ((c?.parked_now || 0) > best) [gate, best] = [g, c?.parked_now || 0];
    }
    if (!gate) {
      const tally = new Map<string, number>();
      for (const j of parked) if (j.waiting?.gate) tally.set(j.waiting.gate, (tally.get(j.waiting.gate) || 0) + 1);
      for (const [g, n] of tally) if (n > best) [gate, best] = [g, n];
    }
    return { gate, parked: true };
  }
  const hr = b.headroom;
  return { gate: hr && typeof hr.jobs === 'number' ? (hr.binds ?? null) : null, parked: false };
}

/** The gauges for this budget, in order: Slots, Memory, CPU, Disk, each only when in force. */
export function gaugeModels(b: Budget, gate: string | null, parked: boolean): GaugeModel[] {
  const hr = b.headroom && typeof b.headroom.jobs === 'number' ? b.headroom : null;
  const gauges: GaugeModel[] = [];
  if (typeof b.hard_ceiling === 'number' && b.hard_ceiling > 0) {
    const n = b.hard_ceiling;
    const used = b.docker_inflight || 0;
    gauges.push({
      key: 'slots',
      caption: 'Slots',
      value: `${used} / ${n}`,
      segs: n,
      filled: Math.min(n, used),
      aria: `Slots: ${used} of ${n} in use, ${Math.max(0, n - used)} free`,
    });
  }
  if (b.budget_mib && b.budget_mib > 0) {
    const f = (b.reserved_mib || 0) / b.budget_mib;
    gauges.push({
      key: 'mem',
      caption: b.reserve?.in_force === 'p75' ? 'Memory p75' : 'Memory',
      value: `${gib1(b.reserved_mib)} / ${gib1(b.budget_mib)} GiB`,
      fill: clamp01(f),
      mark: hr?.job_mib ? fitMark(1 - hr.job_mib / b.budget_mib) : null,
      aria: `Memory: ${gib1(b.reserved_mib)} of ${gib1(b.budget_mib)} GiB reserved`,
    });
  }
  const c = b.cpu;
  if (c && c.budget_mcpu && c.budget_mcpu > 0) {
    const gated = c.mode === 'auto';
    const f = (c.reserved_mcpu || 0) / c.budget_mcpu;
    const cores = (m: number | undefined) => ((m || 0) / 1000).toFixed(1);
    gauges.push({
      key: 'cpu',
      caption: 'CPU',
      value: `${cores(c.reserved_mcpu)} / ${cores(c.budget_mcpu)} cores${gated ? '' : ', not gated'}`,
      fill: clamp01(f),
      off: !gated,
      mark: gated && hr?.job_mcpu ? fitMark(1 - hr.job_mcpu / c.budget_mcpu) : null,
      aria: `CPU: ${cores(c.reserved_mcpu)} of ${cores(c.budget_mcpu)} cores reserved${gated ? '' : ', not gated'}`,
    });
  }
  const d = b.disk;
  if (d && typeof d.free_mib === 'number') {
    const free = d.free_mib;
    const held = d.held_mib || 0;
    if (d.mode === 'on' && typeof d.floor_gib === 'number') {
      const floorMib = d.floor_gib * 1024;
      const spare = Math.max(0, Math.round((free - held - floorMib) / 1024));
      gauges.push({
        key: 'disk',
        caption: 'Disk',
        value: `${spare} GiB spare`,
        fill: free > 0 ? clamp01((floorMib + held) / free) : 1,
        mark: free > 0 ? fitMark(1 - DISK_CHARGE_MIB / free) : null,
        aria: `Disk: ${spare} GiB spare above the ${d.floor_gib} GiB floor`,
      });
    } else {
      const g = Math.round(free / 1024);
      gauges.push({ key: 'disk', caption: 'Disk', value: `${g} GiB free`, fill: 0, aria: `Disk: ${g} GiB free, no floor` });
    }
  }
  const bind = gate ? GAUGE_OF_GATE[gate] : undefined;
  for (const r of gauges) {
    if (r.key !== bind) continue;
    r.bind = parked ? 'warn' : 'ok';
    r.aria += ', binds';
  }
  return gauges;
}

/** One line of an open gate (popover.md § Hero): what it is and its figure; a wait line has no figure. */
export interface GateFact {
  label: string;
  value?: string;
  /** Claims are parked at this gate: the line is amber. */
  waits?: boolean;
}

const coresOf = (mcpu: number | undefined) => `${((mcpu || 0) / 1000).toFixed(1)} cores`;
const gibOf = (mib: number | undefined) => `${gib1(mib)} GiB`;

/**
 * What an open gate lists under its bar, from the same budget the bar reads. Only fields the
 * daemon sent: a line whose field is absent is left out, so an older daemon shows fewer lines.
 * A gate with claims parked at it ends on an amber "N jobs wait for …" line.
 */
export function gateFacts(g: GaugeModel, b: Budget): GateFact[] {
  const hr = b.headroom && typeof b.headroom.jobs === 'number' ? b.headroom : null;
  const parkedAt = (...gates: string[]) => gates.reduce((n, k) => n + (b.gates?.[k]?.parked_now || 0), 0);
  const facts: GateFact[] = [];
  const waitLine = (n: number, what: string) => {
    if (n > 0) facts.push({ label: `${n} ${n === 1 ? 'job waits' : 'jobs wait'} for ${what}`, waits: true });
  };
  if (g.key === 'slots') {
    const n = b.hard_ceiling || 0;
    const used = b.docker_inflight || 0;
    facts.push({ label: 'Running', value: `${used} of ${n}` });
    facts.push({ label: 'Free slots', value: String(Math.max(0, n - used)) });
    const set = b.hard_ceiling_setting;
    if (set !== undefined) facts.push({ label: 'Ceiling', value: set === 'auto' ? `auto, ${n}` : String(set) });
    waitLine(parkedAt('ceiling'), 'a slot');
  } else if (g.key === 'mem') {
    if (typeof b.reserved_mib === 'number') facts.push({ label: 'Reserved by jobs', value: gibOf(b.reserved_mib) });
    facts.push({ label: 'Free', value: gibOf(Math.max(0, (b.budget_mib || 0) - (b.reserved_mib || 0))) });
    if (typeof hr?.job_mib === 'number') facts.push({ label: 'Next job reserves', value: gibOf(hr.job_mib) });
    const waiting = parkedAt('memory', 'vm-memory');
    // Headroom counts typical jobs; with claims parked here it would contradict the wait line.
    if (hr && (hr.binds === 'memory' || hr.binds === 'vm-memory') && !waiting) {
      const jobs = hr.jobs ?? 0;
      facts.push({ label: 'Room for', value: `${jobs} more ${jobs === 1 ? 'job' : 'jobs'}` });
    }
    // Only the unusual sizing: the largest job is the default, and labelling it says nothing.
    if (b.reserve?.in_force === 'p75') facts.push({ label: 'Reserve sized by', value: '75th percentile' });
    waitLine(waiting, 'memory');
  } else if (g.key === 'cpu') {
    const c = b.cpu || {};
    if (typeof c.reserved_mcpu === 'number') facts.push({ label: 'Reserved by jobs', value: coresOf(c.reserved_mcpu) });
    facts.push({ label: 'Free', value: coresOf(Math.max(0, (c.budget_mcpu || 0) - (c.reserved_mcpu || 0))) });
    if (!g.off && typeof hr?.job_mcpu === 'number') facts.push({ label: 'Next job reserves', value: coresOf(hr.job_mcpu) });
    waitLine(parkedAt('cpu', 'cpu-pressure'), 'CPU');
  } else {
    const d = b.disk || {};
    // Whole GiB, as the disk bar's figure and the floor are.
    const wholeGib = (mib: number) => `${Math.round(mib / 1024)} GiB`;
    if (typeof d.free_mib === 'number') facts.push({ label: 'Free on the host', value: wholeGib(d.free_mib) });
    if (typeof d.held_mib === 'number') facts.push({ label: 'Held by running jobs', value: wholeGib(d.held_mib) });
    if (d.mode === 'on' && typeof d.floor_gib === 'number' && typeof d.free_mib === 'number') {
      const spare = Math.max(0, Math.round((d.free_mib - (d.held_mib || 0) - d.floor_gib * 1024) / 1024));
      facts.push({ label: 'Floor kept free', value: `${d.floor_gib} GiB` });
      facts.push({ label: 'Spare above the floor', value: `${spare} GiB` });
    } else if (d.mode === 'off') {
      facts.push({ label: 'Floor', value: 'off' });
    }
    waitLine(parkedAt('disk'), 'disk');
  }
  return facts;
}
