// The Limits strip's pure helpers (docs/design/settings/anatomy.md § 3, § 5): the engine-memory
// bar's arithmetic and the Stepper's step-and-clamp rule.

/** The daemon's budget fields the memory bar reads; an older daemon omits some. */
export interface MemoryFields {
  budget_mib?: number;
  reserved_mib?: number;
  reserve_mib?: number;
  headroom_mib?: number;
}

export interface MemorySegments {
  /** Engine memory: budget plus reserve. */
  totalMib: number;
  reserveMib: number;
  jobsMib: number;
  /** What is left of the budget after jobs and headroom, never below 0. */
  freeMib: number;
  headroomMib: number;
  /** GiB with one decimal, no unit. */
  display: { total: string; reserve: string; jobs: string; free: string; headroom: string };
}

/** GiB with one decimal, no unit ("2.0"). */
export const gib = (mib: number): string => (mib / 1024).toFixed(1);

/** The four segments of the engine-memory bar, or null when the daemon did not send what they need. */
export function memorySegments(budget: MemoryFields | null | undefined): MemorySegments | null {
  if (!budget) return null;
  const { budget_mib: b, reserved_mib: jobs, reserve_mib: reserve, headroom_mib: headroom } = budget;
  if (![b, jobs, reserve, headroom].every((n) => typeof n === 'number' && Number.isFinite(n))) return null;
  const free = Math.max(0, (b as number) - (jobs as number) - (headroom as number));
  const total = (b as number) + (reserve as number);
  return {
    totalMib: total,
    reserveMib: reserve as number,
    jobsMib: jobs as number,
    freeMib: free,
    headroomMib: headroom as number,
    display: {
      total: gib(total),
      reserve: gib(reserve as number),
      jobs: gib(jobs as number),
      free: gib(free),
      headroom: gib(headroom as number),
    },
  };
}

// The Limits view's knobs: what each row steps and shows, the ceiling's auto and off modes, the
// CPU warning, and the per-row change machine (settings/anatomy.md § 2, § 3, § 5).

export type KnobKey = 'ceiling' | 'reserve' | 'headroom' | 'margin' | 'native';

/** The budget fields the five rows read; an older daemon omits some. */
export interface KnobFields extends MemoryFields {
  margin?: number;
  native_concurrency?: number;
  hard_ceiling?: number | null;
  hard_ceiling_setting?: 'auto' | 'off' | number;
  docker_inflight?: number;
  headroom?: { jobs?: number; binds?: string } | null;
}

/** The `set_config` argument a change is sent as. */
export type ConfigArg = number | 'auto';

export interface KnobSpec {
  key: KnobKey;
  label: string;
  hint: string;
  step: number;
  min: number;
  /** The Tauri argument name (camelCase) of `set_config`. */
  arg: 'hardCeiling' | 'reserveMib' | 'headroomMib' | 'margin' | 'nativeConcurrency';
}

/** The five rows, in order. Minimums are the schema's. */
export const KNOBS: readonly KnobSpec[] = [
  { key: 'ceiling', label: 'Hard ceiling', hint: 'Most jobs at once', step: 1, min: 1, arg: 'hardCeiling' },
  { key: 'reserve', label: 'Reserve', hint: 'Kept for the VM itself', step: 512, min: 0, arg: 'reserveMib' },
  { key: 'headroom', label: 'Headroom', hint: 'Left free between jobs', step: 512, min: 1, arg: 'headroomMib' },
  { key: 'margin', label: 'Margin', hint: 'On learned job sizes', step: 0.05, min: 1, arg: 'margin' },
  { key: 'native', label: 'Native jobs', hint: 'Without Docker', step: 1, min: 1, arg: 'nativeConcurrency' },
];

/** How a knob's number is shown: GiB for the two memory knobs, `1.25×` for the margin. */
export function showKnob(key: KnobKey, value: number): string {
  if (key === 'reserve' || key === 'headroom') return `${gib(value)} GiB`;
  if (key === 'margin') return `${Number(value.toFixed(2))}×`;
  return String(value);
}

/** Whether the daemon sent every knob: if not, the view says Limits are unavailable. */
export function limitsAvailable(budget: KnobFields | null | undefined): budget is KnobFields {
  if (!budget) return false;
  return [budget.reserve_mib, budget.headroom_mib, budget.margin, budget.native_concurrency].every(
    (n) => typeof n === 'number' && Number.isFinite(n),
  );
}

/** What the daemon holds for a knob (the ceiling is its own case: see `ceilingView`). */
export function knobValue(budget: KnobFields, key: Exclude<KnobKey, 'ceiling'>): number {
  return { reserve: budget.reserve_mib, headroom: budget.headroom_mib, margin: budget.margin, native: budget.native_concurrency }[key] as number;
}

export interface CeilingView {
  /** `auto` and `off` are the daemon's modes; `explicit` is a number the operator set, or one just pressed. */
  mode: 'auto' | 'off' | 'explicit';
  value: number;
  display: string;
  /** The hint line; the row's own hint unless the mode says otherwise. */
  hint: string;
  /** *Use auto* shows once the ceiling is an explicit number. */
  canUseAuto: boolean;
  /** The CPU warning, when the explicit ceiling is above what CPU allows. */
  warning?: string;
  /** What `+` sets when the ceiling is off: the resolved number, or 1 when there is none. */
  fromOff: number;
}

/** The CPU warning for an explicit ceiling, or undefined (anatomy § 3). */
export function cpuWarning(budget: KnobFields, ceiling: number): string | undefined {
  const h = budget.headroom;
  if (h?.binds !== 'cpu' || typeof h.jobs !== 'number') return undefined;
  const binds = (budget.docker_inflight ?? 0) + h.jobs;
  return ceiling > binds ? `Above what CPU allows. CPU binds at ${binds}.` : undefined;
}

/** The Hard ceiling row as the daemon holds it (anatomy § 2), with `shown` the number just pressed, if any. */
export function ceilingView(budget: KnobFields, shown: number | null): CeilingView {
  const resolved = typeof budget.hard_ceiling === 'number' ? budget.hard_ceiling : null;
  const setting = budget.hard_ceiling_setting ?? (resolved === null ? 'auto' : resolved);
  const fromOff = resolved ?? 1;
  const base = { fromOff, hint: KNOBS[0].hint };
  if (shown === null && setting === 'off') {
    return { ...base, mode: 'off', value: 0, display: 'Off', hint: 'Budget only', canUseAuto: false };
  }
  if (shown === null && setting === 'auto') {
    return {
      ...base,
      mode: 'auto',
      value: resolved ?? 1,
      display: resolved === null ? 'Auto' : String(resolved),
      hint: 'Auto, faber’s pick',
      canUseAuto: false,
    };
  }
  const value = shown ?? (setting as number);
  return { ...base, mode: 'explicit', value, display: String(value), canUseAuto: true, warning: cpuWarning(budget, value) };
}

/** The change in flight or last settled on one row. */
export interface RowState {
  /** The number just pressed, shown until the snapshot catches up; null when none (or after Use auto). */
  shown: number | null;
  /** What the pending or last request sends, kept for Retry. */
  arg: ConfigArg | null;
  status: 'idle' | 'applying' | 'applied' | 'error';
  /** The daemon's reason, for `error`. */
  message?: string;
  /** Counts presses: a response for an older one is ignored. */
  seq: number;
}

export const IDLE_ROW: RowState = { shown: null, arg: null, status: 'idle', seq: 0 };

export type RowAction =
  | { type: 'press'; seq: number; value: number | null; arg: ConfigArg }
  | { type: 'ok'; seq: number }
  | { type: 'fail'; seq: number; message: string }
  | { type: 'clear'; seq: number };

/** The row's status machine: applying, then applied for 2 s, then idle; an error keeps its message until the next press. */
export function rowReducer(state: RowState, a: RowAction): RowState {
  switch (a.type) {
    case 'press':
      return { shown: a.value, arg: a.arg, status: 'applying', seq: a.seq };
    case 'ok':
      return a.seq === state.seq ? { ...state, status: 'applied', message: undefined } : state;
    case 'fail':
      return a.seq === state.seq ? { ...state, status: 'error', message: a.message } : state;
    case 'clear':
      return a.seq === state.seq && state.status === 'applied' ? { ...IDLE_ROW, seq: state.seq } : state;
  }
}

/** The daemon's reason cut to 80 characters with an ellipsis (the full text goes in the title). */
export const cut = (text: string, max = 80): string => (text.length > max ? `${text.slice(0, max)}…` : text);

/** How long a change waits for a further press on its row before one request is sent. */
export const DEBOUNCE_MS = 400;
/** How long "Applied" shows. */
export const APPLIED_MS = 2000;

/** What the debouncer schedules with; the tests pass a fake. */
export interface Scheduler {
  set: (fn: () => void, ms: number) => unknown;
  clear: (id: unknown) => void;
}

/**
 * Settles each key on its own: `push` restarts that key's wait, so ten quick presses send one
 * request, and a press on another key does not touch it. `flush` sends now (Retry, Use auto).
 */
export function debouncer<T>(run: (key: string, value: T) => void, ms = DEBOUNCE_MS, sched: Scheduler = { set: setTimeout, clear: (id) => clearTimeout(id as number) }) {
  const waiting = new Map<string, unknown>();
  const held = new Map<string, T>();
  const stop = (key: string) => {
    held.delete(key);
    if (waiting.has(key)) sched.clear(waiting.get(key));
    waiting.delete(key);
  };
  return {
    push(key: string, value: T) {
      stop(key);
      held.set(key, value);
      waiting.set(
        key,
        sched.set(() => {
          waiting.delete(key);
          held.delete(key);
          run(key, value);
        }, ms),
      );
    },
    flush(key: string, value: T) {
      stop(key);
      run(key, value);
    },
    /** Sends every change still waiting now, as when the view closes mid-wait. */
    flushAll() {
      for (const [key, value] of [...held]) {
        stop(key);
        run(key, value);
      }
    },
  };
}
