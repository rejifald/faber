// The Virtual machines section's data (popover/vms/anatomy.md § 5): one row per running VM and
// the stale state while the supervisor cannot be reached.
import type { Fact } from '../blocks/FactList';
import type { StatusTone } from '../kit/StatusDot';
import { fmtAgo, gib, plural, spokenAgo } from './format';
import type { Budget, VmEntry } from './types';

/** The VM list counts as stale once it is older than this (twice the daemon's 15 s poll). */
export const VM_STALE_S = 30;

/** What a VM is doing, as its icon draws it: serving a job, up with nothing to do, booting or
 * proving the wall, stopping, not faber's, or not known while the list is stale. */
export type VmStatus = 'serving' | 'idle' | 'settling' | 'stopping' | 'unmanaged' | 'unknown';

/** One line of an open VM row: a term and its value (`Memory`, `16 GiB`). */
export type VmFact = Fact;

export interface VmRowModel {
  key: string;
  kind: 'Linux' | 'macOS' | string;
  /** The supervisor's slot: for the key and the order only, never shown. */
  slot: number;
  /** What it serves, after the kind: `3 jobs`, or the job name. Empty otherwise: its state is the
   * icon's, spelled out under `State` in the open row (owner, 2026-10-31), and empty while stale. */
  doing: string;
  status: VmStatus;
  /** The icon's colour: green serving, amber settling or stopping, neutral otherwise and while stale. */
  tone: StatusTone;
  /** What the open row lists: its state in words when it is not serving or idle (the icon cannot
   * tell booting from proving the wall), what it was given, and the Linux VM's free disk. Only the
   * parts the daemon sent; empty, the row does not open. */
  facts: VmFact[];
  /** The coarse age since it started, `—` when unknown or stale. */
  age: string;
  /** The row spoken: "Linux VM, serving 3 jobs, 6 vCPU, 16 GiB, 40 GiB disk free, up 2 hours". */
  label: string;
}

export interface VmSectionModel {
  rows: VmRowModel[];
  /** `as of 3m ago`, set while the list is stale. */
  asOf: string | null;
  /** The list is stale: every icon neutral, every age `—`. */
  dim: boolean;
}

const SETTLING: Record<string, string> = {
  starting: 'booting',
  booting: 'booting',
  ready: 'booting',
  probe_sent: 'proving the wall',
  stopping: 'stopping',
  unmanaged: 'unmanaged',
};

const KIND_NAME: Record<string, string> = { linux: 'Linux', macos: 'macOS' };
const KIND_ORDER: Record<string, number> = { linux: 0, macos: 1 };

function shapeRow(v: VmEntry, budget: Budget, dim: boolean, nowS: number): VmRowModel {
  const linux = v.kind === 'linux';
  const settling = SETTLING[v.state];
  let doing: string;
  let serving = false;
  if (settling) {
    doing = settling;
  } else if (v.state === 'in_service' || v.state === 'job_finished') {
    if (linux) {
      const n = budget.docker_inflight || 0;
      serving = n > 0;
      doing = serving ? plural(n, 'job') : '';
    } else {
      // GitHub's text, a fork's job name withheld: the repo stands in. Set as a text node only.
      const text = v.job ? v.job.job_name || v.job.repo : '';
      serving = Boolean(text);
      doing = text;
    }
  } else {
    doing = v.state.replace(/_/g, ' ');
  }
  // Stale: the last state may be wrong, so no state is drawn (owner, 2026-10-31).
  const status: VmStatus = dim
    ? 'unknown'
    : v.state === 'stopping'
      ? 'stopping'
      : v.state === 'unmanaged'
        ? 'unmanaged'
        : settling !== undefined
          ? 'settling'
          : serving
            ? 'serving'
            : 'idle';
  const tone: StatusTone = dim ? 'neutral' : status === 'settling' || status === 'stopping' ? 'amber' : serving ? 'green' : 'neutral';

  const sizes = [
    v.cpus != null ? `${v.cpus} vCPU` : null,
    v.memory_mib != null ? gib(v.memory_mib) : null,
  ].filter((x): x is string => x !== null);
  const freeMib = linux ? budget.disk?.vm_free_mib : null;
  const free = freeMib != null ? gib(freeMib) : null;
  const word = !dim && (status === 'settling' || status === 'stopping' || status === 'unmanaged') ? doing : '';
  const facts: VmFact[] = [
    word ? { label: 'State', value: word, word: true } : null,
    v.cpus != null ? { label: 'CPU', value: `${v.cpus} vCPU` } : null,
    v.memory_mib != null ? { label: 'Memory', value: gib(v.memory_mib) } : null,
    free !== null ? { label: 'Disk free', value: free } : null,
  ].filter((f): f is VmFact => f !== null);

  const upS = v.started_unix != null && !dim ? Math.max(0, nowS - v.started_unix) : null;
  const kind = KIND_NAME[v.kind] || v.kind;
  const label = [
    // No slot: it is the supervisor's index (the Linux engine's is fixed), and tells a reader nothing.
    `${kind} VM`,
    dim ? null : serving ? `serving ${doing}` : doing || 'idle',
    ...sizes,
    free ? `${free} disk free` : null,
    upS !== null ? `up ${spokenAgo(upS)}` : null,
  ]
    .filter(Boolean)
    .join(', ');
  return {
    key: `${v.kind}-${v.slot}`,
    kind,
    slot: v.slot,
    // Stale: the last activity may be wrong, and the label's "as of" already says so.
    doing: dim || !serving ? '' : doing,
    status,
    tone,
    facts,
    age: upS !== null ? fmtAgo(upS) : '—',
    label,
  };
}

/**
 * The section's model from the budget: stopped VMs hidden, Linux first and then macOS by slot.
 * Sizes, ages and the job are optional (an older daemon omits them) and each degrades to nothing.
 */
export function vmSection(budget: Budget | undefined, nowMs: number = Date.now()): VmSectionModel {
  const b = budget || {};
  const age = b.vm?.listed_age_s;
  const dim = age != null && age > VM_STALE_S;
  const listed = (b.vm?.vms || [])
    .filter((v) => v.state !== 'stopped')
    .sort((a, c) => (KIND_ORDER[a.kind] ?? 2) - (KIND_ORDER[c.kind] ?? 2) || a.slot - c.slot);
  const rows = listed.map((v) => shapeRow(v, b, dim, nowMs / 1000));
  return { rows, asOf: dim ? `as of ${fmtAgo(age)} ago` : null, dim };
}
