// The Jobs view's data (ADR-0056 § 5): where each job class's runs spent their time and
// resources, and one run's own phases for Activity's row detail. Read from the daemon's
// `/api/jobs` and `/api/runs/:runner` (src/jobs_view.rs), never from the polled state.
// A quantity the daemon did not measure is `null`, and every formatter here says
// "not measured" for it: never zero.
import type { PhaseSegment, PhaseTone } from '../kit/PhaseBar';
import { fmtUptime } from '../popover/format';
import { call, headers } from '../session';
import { ActionError } from './api';

/** A run's phases, in order (ADR-0056 § 2). */
export type PhaseId = 'pickup' | 'prepare' | 'parked' | 'sandbox' | 'register' | 'job' | 'teardown';

/** The median and p90 of a quantity over the runs that measured it. */
export interface Spread {
  median: number | null;
  p90: number | null;
  /** How many runs measured it. */
  runs: number;
}

export interface PhaseSpread extends Spread {
  phase: PhaseId;
  /** Runs whose figure crossed clocks and came out negative, so it read as zero. */
  skewed_runs: number;
}

export interface Split<T> {
  setup: T;
  steps: T;
  teardown: T;
}

export interface JobClass {
  key: string;
  label: string;
  repo: string;
  /** The workflow and job the label names; `workflow` is null when the label has another shape, and then `job` is the whole label. */
  workflow: string | null;
  job: string;
  runs: number;
  /** Under the profiling threshold: shown, left out of the ranking. */
  profiling: boolean;
  /** Runs on a backend without cgroups (a macOS VM): phases and steps only. */
  unmeasured_resource_runs: number;
  wall: Spread;
  total_ms: number;
  phases: PhaseSpread[];
  split: Split<Spread>;
  /** Parked time over wall time across the runs that measured both. */
  parked_share: number | null;
  top_gate: { gate: string; runs: number } | null;
  cpu_used_mcpu: Spread;
  cpu_reserved_mcpu: Spread;
  throttled_ms: Spread;
  io_bytes: Spread;
}

export interface JobsView {
  window_days: number;
  from_ms: number;
  to_ms: number;
  profiling_runs: number;
  classes: JobClass[];
}

export interface PhaseSpan {
  phase: PhaseId;
  ms: number | null;
  skewed: boolean;
}

export interface RunStep {
  name: string;
  bucket: 'setup' | 'steps' | 'teardown';
  started_ms?: number;
  completed_ms?: number;
  conclusion?: string;
}

export interface RunFootprint {
  runner: string;
  job_id: number;
  label: string | null;
  result: string | null;
  finished_ms: number;
  phases: PhaseSpan[];
  split: Split<number> | null;
  steps: RunStep[] | null;
  wait_gate: string | null;
  cpu_used_mcpu: number | null;
  cpu_reserved_mcpu: number | null;
  throttled_ms: number | null;
  io_bytes: number | null;
}

/** The windows the view offers, in days; the daemon allows 1 to 90. */
export const WINDOWS = [7, 30, 90] as const;
export type WindowDays = (typeof WINDOWS)[number];

/** GET /api/jobs?days=N. */
export async function getJobs(days: number): Promise<JobsView> {
  const res = await call(`/api/jobs?days=${days}`, { headers: headers() });
  if (!res.ok) throw new ActionError(res.status, (await res.text()).trim() || `faber answered ${res.status}`);
  return (await res.json()) as JobsView;
}

/** GET /api/runs/:runner, or null when no recorded run has that runner. */
export async function getRun(runner: string): Promise<RunFootprint | null> {
  const res = await call(`/api/runs/${encodeURIComponent(runner)}`, { headers: headers() });
  if (res.status === 404) return null;
  if (!res.ok) throw new ActionError(res.status, (await res.text()).trim() || `faber answered ${res.status}`);
  return (await res.json()) as RunFootprint;
}

// --- words and numbers ---------------------------------------------------------------------------

export const NOT_MEASURED = 'not measured';

/** A length in the house duration form (`fmtUptime`: 12s, 3m, 1h 02m); null is "not measured". */
export function fmtMs(ms: number | null | undefined): string {
  if (ms === null || ms === undefined) return NOT_MEASURED;
  return fmtUptime(ms / 1000);
}

/** 812 B, 4.2 MB, 1.3 GB, in Docker's decimal units; null is "not measured". */
export function fmtBytes(bytes: number | null | undefined): string {
  if (bytes === null || bytes === undefined) return NOT_MEASURED;
  const units = ['B', 'kB', 'MB', 'GB', 'TB'];
  let v = bytes;
  let i = 0;
  while (v >= 1000 && i < units.length - 1) {
    v /= 1000;
    i += 1;
  }
  return i === 0 ? `${v} B` : `${v < 10 ? v.toFixed(1) : Math.round(v)} ${units[i]}`;
}

/** 1.5 cores; null is "not measured". */
export function fmtCores(mcpu: number | null | undefined): string {
  if (mcpu === null || mcpu === undefined) return NOT_MEASURED;
  return `${(mcpu / 1000).toFixed(mcpu < 10_000 ? 1 : 0)} cores`;
}

/** 42%; null is "not measured". */
export function fmtShare(share: number | null | undefined): string {
  if (share === null || share === undefined) return NOT_MEASURED;
  return `${Math.round(share * 100)}%`;
}

/** A spread as `median, p90 …`, or "not measured". */
export function fmtSpread(s: Spread, fmt: (v: number | null) => string): string {
  if (s.median === null) return NOT_MEASURED;
  return s.p90 !== null && s.p90 !== s.median ? `${fmt(s.median)}, p90 ${fmt(s.p90)}` : fmt(s.median);
}

// --- the phase bar -------------------------------------------------------------------------------

/** Each phase's name on screen. */
export const PHASE_LABEL: Record<PhaseId, string> = {
  pickup: 'Pickup',
  prepare: 'Prepare',
  parked: 'Parked',
  sandbox: 'Sandbox',
  register: 'Register',
  job: 'Job',
  teardown: 'Teardown',
};

/** What each phase is, for the legend's note. */
export const PHASE_NOTE: Record<PhaseId, string> = {
  pickup: 'GitHub queued it until faber’s poll saw it',
  prepare: 'resolving the runner image, building a variant if needed',
  parked: 'waiting for admission',
  sandbox: 'network, services, dind and the runner container',
  register: 'the runner starting until GitHub dispatched the job',
  job: 'GitHub’s steps',
  teardown: 'GitHub finished until faber cleaned up',
};

// The tone says who the time belongs to: amber is waiting for capacity, the muted greys are
// faber's and GitHub's overhead, the accent is the job's own setup and the strong ink its steps.
const PHASE_TONE: Record<PhaseId, PhaseTone> = {
  pickup: 'overhead',
  prepare: 'faber',
  parked: 'wait',
  sandbox: 'faber',
  register: 'overhead',
  job: 'work',
  teardown: 'faber',
};

const SPLIT_SEGMENTS = [
  { key: 'setup', label: 'Setup', tone: 'setup', note: 'set up job, checkout, setup and cache actions, listed installs' },
  { key: 'steps', label: 'Job steps', tone: 'work', note: 'every other step, installs inside them included' },
  { key: 'post', label: 'Post steps', tone: 'post', note: 'post steps and GitHub’s cleanup' },
] as const;

/**
 * The bar's segments for one set of phase figures: the `job` phase split by GitHub's steps when
 * they were recorded, whole otherwise. A phase with no figure stays in the list, unmeasured.
 */
export function phaseSegments(phases: { phase: PhaseId; ms: number | null; skewed?: boolean }[], split: Split<number | null> | null): PhaseSegment[] {
  return phases.flatMap((p): PhaseSegment[] => {
    const base = { key: p.phase, label: PHASE_LABEL[p.phase], ms: p.ms, tone: PHASE_TONE[p.phase], note: PHASE_NOTE[p.phase], skewed: p.skewed ?? false };
    if (p.phase !== 'job' || split === null || split.setup === null) return [base];
    const parts: Record<(typeof SPLIT_SEGMENTS)[number]['key'], number | null> = { setup: split.setup, steps: split.steps, post: split.teardown };
    return SPLIT_SEGMENTS.map((s) => ({ key: `job-${s.key}`, label: s.label, ms: parts[s.key], tone: s.tone, note: s.note, skewed: false }));
  });
}

/** One run's segments. */
export const runSegments = (run: RunFootprint): PhaseSegment[] => phaseSegments(run.phases, run.split);

/** A class's segments from each phase's median. */
export function classSegments(c: JobClass): PhaseSegment[] {
  const split = c.split.setup.runs > 0 ? { setup: c.split.setup.median, steps: c.split.steps.median, teardown: c.split.teardown.median } : null;
  return phaseSegments(
    c.phases.map((p) => ({ phase: p.phase, ms: p.median, skewed: p.skewed_runs > 0 })),
    split,
  );
}
