// The Memory view's data shaping (docs/design/dashboard/screens.md § 4 "Memory", § 5 ClassDetail):
// the daemon's classes as rows, the filters and their counts, the limits and parked-gate panels,
// and the pin request. Pure, so stories and tests shape data exactly as the view does.
import type { Budget, ClassCap, DaemonState } from '../popover/types';
import { fmtAgo, fmtUptime } from '../popover/format';
import type { PinRequest } from './api';

const MIB_PER_GIB = 1024;

/** Fewer samples than this and a class is still finding its footing. */
export const LEARN_SAMPLES = 15;
/** How many of the latest runner peaks the run strip shows. */
export const RECENT_RUNS = 14;
/** A bimodal split needs at least this many peaks in each cluster, so one outlier is not a mode. */
const MIN_CLUSTER = 2;
/** ...and at least this wide an empty gap, so two close small classes are not a mode. */
const MIN_GAP_MIB = MIB_PER_GIB;

/** GiB for a table cell or a sentence: two decimals with one trailing zero dropped (6.0, 0.77, 1.15). */
export function gibText(mib: number): string {
  const s = (mib / MIB_PER_GIB).toFixed(2);
  return s.endsWith('0') ? s.slice(0, -1) : s;
}

/** MiB rounded up to the next 0.1 GiB (1e-9 absorbs float noise: 3.1 x 1 stays 3.1). */
export function ceilTenthGib(mib: number): number {
  return Math.round((Math.ceil((mib / MIB_PER_GIB) * 10 - 1e-9) / 10) * MIB_PER_GIB);
}

const max = (values: number[]): number => values.reduce((a, b) => (b > a ? b : a), 0);

/**
 * The bimodal rule. Sort the runner peaks and take the widest empty gap between two neighbours;
 * that gap splits them into a low and a high cluster. The class is bimodal when both clusters
 * hold at least two peaks (one outlier is not a mode), the gap is at least 1 GiB, and the high
 * cluster starts at twice the low cluster's maximum or more: no run landed between X and 2X,
 * which is what a warm run against a cold one (a cache miss) looks like. Null when it is not.
 */
export function splitPeaks(peaks: number[]): { lowMax: number; highMin: number; highMax: number; lowCount: number; highCount: number } | null {
  if (peaks.length < MIN_CLUSTER * 2) return null;
  const sorted = [...peaks].sort((a, b) => a - b);
  let at = 0;
  let widest = 0;
  for (let i = 1; i < sorted.length; i += 1) {
    const gap = sorted[i] - sorted[i - 1];
    if (gap > widest) {
      widest = gap;
      at = i;
    }
  }
  const lowCount = at;
  const highCount = sorted.length - at;
  if (lowCount < MIN_CLUSTER || highCount < MIN_CLUSTER) return null;
  const lowMax = sorted[at - 1];
  const highMin = sorted[at];
  if (widest < MIN_GAP_MIB || highMin < 2 * lowMax) return null;
  return { lowMax, highMin, highMax: sorted[sorted.length - 1], lowCount, highCount };
}

/** One class as the Memory view draws it. */
export interface ClassView {
  key: string;
  /** Mono display name: `acme/web:e2e`, the repo and the job. */
  name: string;
  repo: string;
  job: string;
  /** The caps in force, MiB. */
  runnerMib: number;
  dindMib: number;
  /** Runner plus dind. */
  capMib: number;
  runnerPeaks: number[];
  dindPeaks: number[];
  /** Parallel to `runnerPeaks`: where the sample was an OOM stand-in. */
  runnerCensored: boolean[];
  /** The class's measured peak: highest runner peak plus highest dind peak (MiB); null with no samples. */
  peakMib: number | null;
  /** Bimodal only: the warm cluster's peak, so the cell reads "low to high". */
  warmPeakMib: number | null;
  /** Bimodal only: where the cold cluster of runner peaks starts, MiB. */
  coldFromMib: number | null;
  /** Runs in the high (cold) cluster, bimodal only. */
  coldRuns: number;
  samples: number;
  status: 'learned' | 'pinned' | 'warned' | 'profiling' | string;
  /** The badge's words, sentence case: Pinned, Warned or Learning; empty for a plain learned class, which shows no badge. */
  statusLabel: string;
  statusTone: 'neutral' | 'amber' | 'accent';
  pinned: boolean;
  warned: boolean;
  bimodal: boolean;
  learning: boolean;
  /** The measured peak is above the cap in force. */
  overCap: boolean;
  /** Max peak x margin rounded up to 0.1 GiB, per container; null with no peaks. */
  coldSafe: { runnerMib: number; dindMib: number } | null;
  /** A job of this class is admitted right now. */
  inFlight: boolean;
  /** Seconds since a run of this class last finished; null when the daemon's recent outcomes do not hold one. */
  lastRanAgeS: number | null;
  /** The latest runner peaks (oldest first) for the run strip. */
  recentPeaks: number[];
  recentCensored: boolean[];
  /** One line under the name for a class that needs a look: warned, then bimodal, then learning. */
  note: string | null;
  /** The explanation shown in the class panel, one paragraph each. */
  explanation: string[];
}

/** A class key is `repo::workflow_id::job_name` (src/limiter.rs `class_key`); `|` is read too. */
function keyParts(key: string): { repo: string; job: string } {
  const parts = key.includes('::') ? key.split('::') : key.split('|');
  if (parts.length >= 3) return { repo: parts[0], job: parts.slice(2).join(' ') };
  return { repo: parts[0], job: '' };
}

/**
 * The class's display name, `repo:job`, from its key; the daemon's own label when the key has no
 * job part. One name for the Memory table, its panel and the Overview's Learned caps.
 */
export function classDisplayName(c: Pick<ClassCap, 'key' | 'label'>): string {
  const { repo, job } = keyParts(c.key);
  return job ? `${repo}:${job}` : c.label;
}

/**
 * The status badge: Pinned, Warned, or Learning while the class has fewer than 15 samples. A
 * normal learned class carries none (no bare default states), so its label is empty.
 */
function tidy(c: ClassCap): Pick<ClassView, 'status' | 'statusLabel' | 'statusTone'> {
  const status = c.pinned ? 'pinned' : c.status;
  if (status === 'pinned') return { status, statusLabel: 'Pinned', statusTone: 'accent' };
  if (status === 'warned') return { status, statusLabel: 'Warned', statusTone: 'amber' };
  return { status, statusLabel: c.samples < LEARN_SAMPLES ? 'Learning' : '', statusTone: 'neutral' };
}

/** The classes as rows, in the daemon's order. `now` is accepted for symmetry with the view; ages come from the outcomes' own `ts_ms`. */
export function classViews(state: DaemonState, now: number): ClassView[] {
  const budget = state.budget ?? {};
  const outcomes = state.outcomes ?? [];
  const inflight = state.inflight ?? [];
  const margin = budget.margin && budget.margin > 0 ? budget.margin : 1;
  return (budget.classes ?? []).map((c) => {
    const { repo, job } = keyParts(c.key);
    const runnerPeaks = c.peaks_runner_mib ?? [];
    const dindPeaks = c.peaks_dind_mib ?? [];
    const runnerCensored = c.censored_runner ?? [];
    const dindMax = max(dindPeaks);
    const split = splitPeaks(runnerPeaks);
    const hasPeaks = runnerPeaks.length > 0 || dindPeaks.length > 0;
    const peakMib = hasPeaks ? max(runnerPeaks) + dindMax : null;
    const capMib = c.runner_mib + c.dind_mib;
    const status = tidy(c);
    const warned = status.status === 'warned';
    const bimodal = split !== null;
    const learning = c.samples < LEARN_SAMPLES;
    const matches = (r?: string, j?: string | null) => r === repo && (j ?? '') === job;
    const lastOutcome = outcomes.filter((o) => matches(o.repo, o.job_name)).reduce<number | null>((t, o) => (t === null || o.ts_ms > t ? o.ts_ms : t), null);
    const coldSafe = hasPeaks ? { runnerMib: ceilTenthGib(max(runnerPeaks) * margin), dindMib: ceilTenthGib(dindMax * margin) } : null;
    const high = peakMib === null ? '' : gibText(peakMib);
    const low = split ? gibText(split.lowMax + dindMax) : '';
    const explanation: string[] = [];
    if (warned) {
      explanation.push('Warned means faber measured this class over half the budget and stopped adjusting its cap on its own. Pin a cap to move it on.');
    }
    if (bimodal) {
      explanation.push(
        `This class is bimodal: warm runs peak near ${low} GiB and cold runs (a dependency cache miss, say) spike to ${high} GiB. The cap is learned mostly from warm samples, so a cold run can be killed for memory. One cap cannot cover both: pin one that does.`,
      );
    }
    if (learning) {
      explanation.push(`Only ${c.samples} of ${LEARN_SAMPLES} samples so far: the cap is a first guess and will move as runs finish.`);
    }
    if (status.status === 'pinned') {
      explanation.push('This cap came from a person. faber keeps measuring peaks but does not change it; return to learned to let it adjust again.');
    }
    return {
      key: c.key,
      name: classDisplayName(c),
      repo,
      job,
      runnerMib: c.runner_mib,
      dindMib: c.dind_mib,
      capMib,
      runnerPeaks,
      dindPeaks,
      runnerCensored,
      peakMib,
      warmPeakMib: split ? split.lowMax + dindMax : null,
      coldFromMib: split ? split.highMin : null,
      coldRuns: split ? split.highCount : 0,
      samples: c.samples,
      ...status,
      pinned: c.pinned,
      warned,
      bimodal,
      learning,
      overCap: peakMib !== null && peakMib > capMib,
      coldSafe,
      inFlight: inflight.some((j) => j.admitted && matches(j.repo, j.job_name)),
      lastRanAgeS: lastOutcome === null ? null : Math.max(0, Math.floor((now - lastOutcome) / 1000)),
      recentPeaks: runnerPeaks.slice(-RECENT_RUNS),
      recentCensored: runnerCensored.slice(-RECENT_RUNS),
      note: warned ? 'Auto-learn paused, needs a pin' : bimodal ? 'Bimodal, warm and cold peaks' : learning ? 'New class, still learning' : null,
      explanation,
    };
  });
}

/** The measured peak as the table sets it: "6.1" or, for a bimodal class, "3.1-8.2" with an en dash. */
export function peakText(c: ClassView): string {
  if (c.peakMib === null) return '—';
  return c.warmPeakMib === null ? gibText(c.peakMib) : `${gibText(c.warmPeakMib)}–${gibText(c.peakMib)}`;
}

/** The idle line under a class's title. */
export function idleLine(c: ClassView): string {
  if (c.inFlight) return 'running now';
  return c.lastRanAgeS === null ? 'idle' : `idle, last ran ${fmtAgo(c.lastRanAgeS)} ago`;
}

export type ClassFilter = 'all' | 'pinned' | 'warned' | 'bimodal';

const FILTERS: Record<ClassFilter, (c: ClassView) => boolean> = {
  all: () => true,
  pinned: (c) => c.pinned,
  warned: (c) => c.warned,
  bimodal: (c) => c.bimodal,
};

export function filterClasses(classes: ClassView[], filter: ClassFilter): ClassView[] {
  return classes.filter(FILTERS[filter]);
}

/** How many classes each filter holds. */
export function filterCounts(classes: ClassView[]): Record<ClassFilter, number> {
  return {
    all: classes.length,
    pinned: classes.filter(FILTERS.pinned).length,
    warned: classes.filter(FILTERS.warned).length,
    bimodal: classes.filter(FILTERS.bimodal).length,
  };
}

/** Parses the pin form's GiB text into the wire request (MiB), or null when the runner is not a positive number or the dind is negative or not a number. */
export function pinRequest(key: string, runnerGib: string, dindGib: string): PinRequest | null {
  const runner = Number(runnerGib.trim());
  const dind = dindGib.trim() === '' ? 0 : Number(dindGib.trim());
  if (!Number.isFinite(runner) || !Number.isFinite(dind) || runner <= 0 || dind < 0) return null;
  return { class: key, pin: true, runner_mib: Math.round(runner * MIB_PER_GIB), dind_mib: Math.round(dind * MIB_PER_GIB) };
}

/** Returns a pinned class to learned. */
export const unpinRequest = (key: string): PinRequest => ({ class: key, pin: false });

/** One fact of the Limits panel. */
export interface LimitRow {
  key: string;
  label: string;
  value: string;
  tone?: 'amber' | 'red';
  note?: string;
}

const cores = (mcpu: number) => String(Math.round(mcpu / 100) / 10);

/** The Limits panel's rows, each hidden when its field is absent. */
export function limitRows(budget: Budget): LimitRow[] {
  const rows: LimitRow[] = [];
  const reserve = budget.reserve;
  if (budget.reserve_mib !== undefined || reserve) {
    const kept = budget.reserve_mib !== undefined ? `${gibText(budget.reserve_mib)} GiB kept for the VM` : '';
    const debit = reserve?.in_force ?? reserve?.mode;
    const basis = debit ? `admitting on ${debit} peaks` : '';
    rows.push({
      key: 'reserve',
      label: 'Reserve',
      value: [kept, basis].filter(Boolean).join(', '),
      ...(reserve?.cooling ? { tone: 'amber' as const, note: 'Cooling off after a fallback to max peaks' } : {}),
    });
  }
  const cpu = budget.cpu;
  if (cpu) {
    if (cpu.budget_mcpu !== undefined && cpu.mode !== 'off') {
      rows.push({
        key: 'cpu',
        label: 'CPU',
        value: `${cores(cpu.reserved_mcpu ?? 0)} of ${cores(cpu.budget_mcpu)} cores reserved`,
        ...(cpu.frozen ? { tone: 'amber' as const, note: 'Admission is frozen under CPU pressure' } : {}),
      });
    } else {
      rows.push({ key: 'cpu', label: 'CPU', value: cpu.mode === 'off' ? 'Not limiting admission' : (cpu.mode ?? 'Not stated') });
    }
  }
  const disk = budget.disk;
  if (disk) {
    if (disk.mode === 'off') {
      rows.push({ key: 'disk', label: 'Disk', value: 'Not limiting admission' });
    } else {
      const free = typeof disk.free_mib === 'number' ? `${gibText(disk.free_mib)} GiB free` : 'free space unread';
      const floor = typeof disk.floor_gib === 'number' ? `, floor ${disk.floor_gib} GiB` : '';
      rows.push({
        key: 'disk',
        label: 'Disk',
        value: `${free}${floor}`,
        ...(disk.closed
          ? { tone: 'red' as const, note: 'No room for any job above the floor' }
          : disk.fresh === false
            ? { tone: 'amber' as const, note: 'The free-space reading is stale' }
            : {}),
      });
    }
  }
  const machine = budget.machine;
  if (machine) {
    const declared = machine.share !== undefined && machine.share !== 'unstated';
    const keeps = [
      typeof machine.keep_memory_gib === 'number' ? `${machine.keep_memory_gib} GiB` : '',
      typeof machine.keep_cpus === 'number' ? `${machine.keep_cpus} CPUs` : '',
    ].filter(Boolean);
    const hold = machine.hold ? `Holding admission: ${machine.hold}` : machine.encroaches?.length ? machine.encroaches.join('; ') : undefined;
    rows.push({
      key: 'machine',
      label: 'Machine share',
      value: declared && keeps.length ? `keeps ${keeps.join(' and ')} for the machine` : 'Not declared',
      ...(hold ? { tone: 'amber' as const, note: hold } : {}),
    });
  }
  const co = budget.cotenant;
  if (co) {
    const now = typeof co.latest_mib === 'number' ? `${gibText(co.latest_mib)} GiB now` : '';
    const peak = typeof co.peak_mib === 'number' ? `${gibText(co.peak_mib)} GiB peak` : '';
    const floor = typeof co.floor_mib === 'number' && !now && !peak ? `${gibText(co.floor_mib)} GiB floor` : '';
    rows.push({
      key: 'cotenant',
      label: 'Other tenants of the VM',
      value: [now, peak, floor].filter(Boolean).join(', ') || 'Not measured',
      ...(co.peak_binds ? { tone: 'amber' as const, note: 'Their peak sets the reserve' } : {}),
    });
  }
  return rows;
}

/** The gate keys the daemon sends (src/limiter.rs, reconcile.rs, vm/admit.rs, machine.rs), in words. */
const GATE_WORDS: Record<string, string> = {
  held: 'Docker held',
  paused: 'Paused',
  'machine-pressure': 'Machine memory pressure',
  swap: 'Swap growth',
  docker: 'Docker unreachable',
  disk: 'Disk space',
  adoption: 'Adopting running jobs',
  ceiling: 'Job ceiling',
  memory: 'Memory',
  cpu: 'CPU',
  'cpu-pressure': 'CPU pressure',
  'vm-memory': 'VM memory',
  image: 'Runner image',
  'image-pull': 'Runner image pull',
  'image-missing': 'Runner image missing',
  pool: 'Runner pool',
  'repo-held': 'Repo held',
  'refused-job': 'Refused job',
  github: 'GitHub',
  credential: 'GitHub credential',
  permission: 'GitHub permission',
  wall: 'Network wall',
  vm_session: 'macOS VM session',
  golden_image: 'macOS golden image',
  macos_cap: 'macOS VM cap',
  share: 'Machine share',
  vm_memory: 'macOS VM memory',
};

/** A gate key in sentence-case words; a key the daemon adds later reads as itself, underscores and hyphens as spaces. */
export function gateName(key: string): string {
  const known = GATE_WORDS[key];
  if (known) return known;
  const t = key.replace(/[_-]+/g, ' ').trim();
  return t.charAt(0).toUpperCase() + t.slice(1);
}

/** One gate of the "What parked jobs" panel. */
export interface GateRow {
  gate: string;
  /** The gate in words. */
  label: string;
  parkedNow: number;
  arrivals: number;
  parkedMs: number;
  /** "12m", "5h 20m": how long jobs have waited at this gate in all. */
  parkedFor: string;
  /** How often this gate was the one binding. */
  binding: number;
}

/** The gates that hold counts, longest-parked first; empty when the daemon sends none. */
export function parkedGates(budget: Budget): GateRow[] {
  return Object.entries(budget.gates ?? {})
    .filter((e): e is [string, NonNullable<(typeof e)[1]>] => e[1] != null)
    .map(([gate, g]) => ({
      gate,
      label: gateName(gate),
      parkedNow: g.parked_now ?? 0,
      arrivals: g.parks ?? 0,
      parkedMs: g.parked_ms ?? 0,
      parkedFor: fmtUptime((g.parked_ms ?? 0) / 1000),
      binding: g.binding ?? 0,
    }))
    .sort((a, b) => b.parkedMs - a.parkedMs || a.gate.localeCompare(b.gate));
}
