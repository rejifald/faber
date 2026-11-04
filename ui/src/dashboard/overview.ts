// What the Overview draws, shaped from the daemon's state (docs/design/dashboard/screens.md § 4).
// Pure: the screen renders these view models, and ui/scripts/overview.test.mjs pins them.
import { fmtAgo, fmtUptime, plural } from '../popover/format';
import { gib } from '../popover/limits';
import type { Part } from '../popover/attention';
import type { ClassCap, DaemonState, Inflight } from '../popover/types';
import type { Alarm } from './alarms';
import { verdictReason } from './activity';
import { classDisplayName, gateName } from './memory';

type Tone = 'green' | 'amber' | 'red' | 'neutral' | 'accent';

// --- durations -----------------------------------------------------------------------------------

/** A duration to the second where it helps: 18s, 1m 40s, 6m 41s, 3h 2m, 2d 4h. */
/** A sentence-case name set mid-sentence: "Memory" reads "memory", "VM session" stays as it is. */
const midSentence = (name: string) => (/^[A-Z][a-z]/.test(name) ? name[0].toLowerCase() + name.slice(1) : name);

export function fmtDur(seconds: number | null | undefined): string {
  if (seconds == null || !Number.isFinite(seconds)) return '—';
  const s = Math.max(0, Math.round(seconds));
  if (s < 60) return `${s}s`;
  if (s < 3600) {
    const r = s % 60;
    return r ? `${Math.floor(s / 60)}m ${r}s` : `${Math.floor(s / 60)}m`;
  }
  if (s < 86400) return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
  return `${Math.floor(s / 86400)}d ${Math.floor((s % 86400) / 3600)}h`;
}

/** The nearest-rank 95th percentile, or null with nothing to rank. */
export function p95(values: number[]): number | null {
  const v = values.filter((x) => Number.isFinite(x)).sort((a, b) => a - b);
  if (!v.length) return null;
  return v[Math.max(0, Math.ceil(v.length * 0.95) - 1)];
}

// --- the four tiles (S1) -------------------------------------------------------------------------

export interface TileModel {
  key: string;
  label: string;
  value: string;
  /** A unit set small after the value. */
  unit?: string;
  note: string;
  tone?: 'amber' | 'red';
}

/** Since-start counts, and the p95 of the waits the daemon still holds. No trend: the daemon serves none (S1). */
export function statTiles(s: DaemonState): TileModel[] {
  const c = s.counts;
  const count = (n: number | undefined) => (c && n !== undefined ? String(n) : '—');
  const since = `since start, ${fmtUptime(s.uptime_s)}`;
  const waits = (s.outcomes || []).map((o) => o.wait_s).filter((w): w is number => typeof w === 'number');
  const p = p95(waits);
  const wait: TileModel =
    p === null
      ? { key: 'wait', label: 'Wait to admit', value: '—', note: 'no runs held yet' }
      : p < 60
        ? { key: 'wait', label: 'Wait to admit', value: String(Math.round(p)), unit: 's', note: `p95 of the last ${plural(waits.length, 'run')}` }
        : { key: 'wait', label: 'Wait to admit', value: fmtDur(p), note: `p95 of the last ${plural(waits.length, 'run')}` };
  return [
    { key: 'delivered', label: 'Delivered', value: count(c?.completed), note: since },
    { key: 'faults', label: "faber's own faults", value: count(c?.faults), note: since, ...((c?.faults ?? 0) > 0 ? { tone: 'amber' as const } : {}) },
    wait,
    { key: 'refused', label: 'Refused at the gate', value: count(c?.refused), note: since },
  ];
}

// --- budget and in flight ------------------------------------------------------------------------

export interface BudgetJob {
  key: string;
  label: string;
  mib: number;
}

const classKey = (j: Inflight) => `${j.repo}|${j.workflow}|${j.job_name}`;

/** What a job holds (or would): its class's cap in force, else the headroom's typical job. */
export function jobMib(j: Inflight, classes: ClassCap[] | undefined, fallback?: number): number | null {
  const c = (classes || []).find((x) => x.key === classKey(j));
  if (c) return c.runner_mib + c.dind_mib;
  return typeof fallback === 'number' && fallback > 0 ? fallback : null;
}

/** Admitted (running or frozen) and parked jobs, as the rail counts them. */
export function jobCounts(s: DaemonState): { admitted: number; parked: number } {
  const all = s.inflight || [];
  const parked = all.filter((j) => j.admitted === false).length;
  return { admitted: all.length - parked, parked };
}

/**
 * The rail's jobs: each admitted job with a known cap, largest first. What the daemon reserved
 * beyond them (jobs with no learned class, the escalation guard) shows as one "Other reservations"
 * slice, so the rail never claims more room than the budget has.
 */
export function budgetJobs(s: DaemonState): BudgetJob[] {
  const b = s.budget;
  const jobs: BudgetJob[] = [];
  for (const j of s.inflight || []) {
    if (j.admitted === false) continue;
    const mib = jobMib(j, b?.classes);
    if (mib) jobs.push({ key: String(j.job_id), label: `${j.repo ?? '?'} ${j.job_name ?? ''}`.trim(), mib });
  }
  jobs.sort((a, z) => z.mib - a.mib);
  const rest = (b?.reserved_mib ?? 0) - jobs.reduce((n, j) => n + j.mib, 0);
  if (rest >= 1) jobs.push({ key: 'other', label: 'Other reservations', mib: rest });
  return jobs;
}

export interface InflightRow {
  key: string;
  dot: Tone;
  pulse: boolean;
  repo: string;
  /** `#482`, else the branch. */
  ref: string;
  job: string;
  step: string;
  mib: number | null;
  elapsed: string;
  tone?: 'amber' | 'red';
}

function stepText(j: Inflight): string {
  if (j.frozen) {
    const why = j.frozen.reason ? ` for ${j.frozen.reason}` : '';
    const err = j.frozen.thaw_failure ? `, could not thaw: ${j.frozen.thaw_failure.error}` : '';
    return `Frozen${why}${err}`;
  }
  if (j.admitted === false || j.waiting) {
    const w = j.waiting;
    return `Parked${w?.gate ? ` — waiting for ${midSentence(gateName(w.gate))}` : ''}${w?.detail ? `: ${w.detail}` : ''}`;
  }
  const st = j.step;
  if (!st?.name) return 'Starting';
  return st.index && st.total ? `${st.name} (${st.index} of ${st.total})` : st.name;
}

/** One row per job in flight; a parked or frozen job is toned (red when a thaw is failing). */
export function inflightRows(s: DaemonState): InflightRow[] {
  const fallback = s.budget?.headroom?.job_mib;
  return (s.inflight || []).map((j) => {
    const parked = j.admitted === false || (!j.frozen && Boolean(j.waiting));
    const tone: InflightRow['tone'] = j.frozen ? (j.frozen.thaw_failure ? 'red' : 'amber') : parked ? 'amber' : undefined;
    return {
      key: String(j.job_id),
      dot: tone ?? 'accent',
      pulse: !tone,
      repo: j.repo ?? '?',
      ref: j.pr ? `#${j.pr}` : (j.branch ?? ''),
      job: j.job_name ?? '',
      step: stepText(j),
      mib: jobMib(j, s.budget?.classes, fallback),
      elapsed: fmtDur(j.elapsed_s),
      ...(tone ? { tone } : {}),
    };
  });
}

// --- latest activity -----------------------------------------------------------------------------

export type ActivityFilter = 'all' | 'delivered' | 'faults' | 'refused' | 'retried';
export const ACTIVITY_FILTERS: ActivityFilter[] = ['all', 'delivered', 'faults', 'refused', 'retried'];
/** How many rows the Overview shows; the rest is the Activity view. */
export const ACTIVITY_ROWS = 8;

export interface ActivityRow {
  key: string;
  ts_ms: number;
  kind: Exclude<ActivityFilter, 'all'>;
  result: string;
  tone: Tone;
  repo: string;
  job: string;
  /** The Detail column: what happened, in a few words. */
  detail: string;
  /** Faults colour their detail. */
  detailTone?: 'red';
  took: string;
  /** Free daemon text for the row's disclosure; set as text. */
  cause?: string;
  runner?: string;
  attempt?: number;
}

const RESULTS: Record<string, { label: string; tone: Tone }> = {
  success: { label: 'Delivered', tone: 'green' },
  timeout: { label: 'Timed out', tone: 'amber' },
  infra_error: { label: 'Infra error', tone: 'red' },
  failure: { label: 'Failed', tone: 'red' },
};


/**
 * Finished runs, refusals and retries the daemon still holds, newest first. Outcomes are runs
 * (`success` is delivered, anything else a fault); refusals are the jobs refused right now; a
 * retry is one auto-retry action (S4, S5).
 */
export function activityRows(s: DaemonState): ActivityRow[] {
  const rows: ActivityRow[] = [];
  for (const o of s.outcomes || []) {
    const r = RESULTS[o.result ?? ''] ?? { label: o.result ? o.result[0].toUpperCase() + o.result.slice(1).replace(/_/g, ' ') : 'Unknown', tone: 'neutral' as Tone };
    const delivered = o.result === 'success';
    const parked = o.wait_s && o.wait_s > 0 ? `${fmtDur(o.wait_s)} of it parked${o.wait_gate ? ` on ${o.wait_gate}` : ''}` : '';
    rows.push({
      key: `outcome:${o.job_id}:${o.ts_ms}`,
      ts_ms: o.ts_ms,
      kind: delivered ? 'delivered' : 'faults',
      result: r.label,
      tone: r.tone,
      repo: o.repo ?? '?',
      job: o.job_name ?? '—',
      detail: delivered ? parked : (o.cause ?? ''),
      ...(!delivered && o.cause ? { detailTone: 'red' as const } : {}),
      took: fmtDur(o.duration_s),
      ...(o.cause ? { cause: o.cause } : {}),
      ...(o.runner ? { runner: o.runner } : {}),
      ...(o.attempt ? { attempt: o.attempt } : {}),
    });
  }
  const refused = (key: string, ts: number, repo: string, job: string, detail: string) =>
    rows.push({ key, ts_ms: ts, kind: 'refused', result: 'Refused', tone: 'neutral', repo, job, detail, took: '—' });
  for (const r of s.refused_queued || []) refused(`refused:${r.job_id}`, r.since_ms ?? 0, r.repo ?? '?', r.job_name ?? '—', verdictReason(r.verdict ?? ''));
  for (const r of s.dind_refused || []) refused(`dind:${r.job_id}`, r.at_ms, r.repo, r.job_name, r.reason);
  for (const r of s.service_refused || []) refused(`service:${r.job_id}`, r.at_ms, r.repo, r.job_name, r.reason);
  for (const r of s.retries || []) {
    rows.push({
      key: `retry:${r.run_id}:${r.attempt}:${r.ts_ms}`,
      ts_ms: r.ts_ms,
      kind: 'retried',
      result: 'Retried',
      tone: 'neutral',
      repo: r.repo,
      job: '—',
      detail: `re-ran ${plural(r.jobs, 'job')} (${r.mode})`,
      took: '—',
    });
  }
  return rows.sort((a, z) => z.ts_ms - a.ts_ms);
}

export const activityWhen = (ts: number, now: number) => `${fmtAgo((now - ts) / 1000)} ago`;

/** Rows per filter, for the SegmentedControl's counts. */
export function activityCounts(rows: ActivityRow[]): Record<ActivityFilter, number> {
  const n: Record<ActivityFilter, number> = { all: rows.length, delivered: 0, faults: 0, refused: 0, retried: 0 };
  for (const r of rows) n[r.kind]++;
  return n;
}

/** The latest rows of one filter. */
export function filterActivity(rows: ActivityRow[], filter: ActivityFilter, limit = ACTIVITY_ROWS): ActivityRow[] {
  return (filter === 'all' ? rows : rows.filter((r) => r.kind === filter)).slice(0, limit);
}

// --- learned caps --------------------------------------------------------------------------------

export interface CapRow {
  key: string;
  label: string;
  /** `pinned`, `warned`, or null for a plain learned class. */
  badge: 'pinned' | 'warned' | null;
  samples: number;
  mib: number;
}

const RANK = { pinned: 0, warned: 1, none: 2 };

/** The classes, pinned first, then warned, then by cap, largest first. */
export function learnedCaps(s: DaemonState): CapRow[] {
  return (s.budget?.classes || [])
    .map((c): CapRow => ({
      key: c.key,
      label: classDisplayName(c),
      badge: c.pinned || c.status === 'pinned' ? 'pinned' : c.status === 'warned' ? 'warned' : null,
      samples: c.samples,
      mib: c.runner_mib + c.dind_mib,
    }))
    .sort((a, z) => RANK[a.badge ?? 'none'] - RANK[z.badge ?? 'none'] || z.mib - a.mib || a.label.localeCompare(z.label));
}

// --- the Host panel (S2) -------------------------------------------------------------------------

export interface HostFact {
  key: string;
  label: string;
  value: string;
  tone?: Tone;
  mono?: boolean;
  note?: string;
}

const PRESSURE: Record<string, { value: string; tone: Tone }> = {
  normal: { value: 'none', tone: 'green' },
  warn: { value: 'elevated', tone: 'amber' },
  critical: { value: 'critical', tone: 'red' },
};

/** Docker, Disk, Memory pressure, Uptime, Keep awake and the audit log; a row is absent when its data is. */
export function hostFacts(s: DaemonState): HostFact[] {
  const out: HostFact[] = [];
  const b = s.budget;
  if (b?.docker_reachable !== undefined) {
    out.push({ key: 'docker', label: 'Docker', value: b.docker_reachable ? 'reachable' : 'unreachable', tone: b.docker_reachable ? 'green' : 'red' });
  }
  const d = b?.disk;
  if (d && d.mode !== 'off' && typeof d.free_mib === 'number') {
    const low = typeof d.floor_gib === 'number' && d.free_mib / 1024 < d.floor_gib * 2;
    out.push({
      key: 'disk',
      label: 'Disk',
      value: `${gib(d.free_mib)} GiB free`,
      tone: d.closed ? 'red' : d.fresh === false || low ? 'amber' : 'green',
      ...(d.closed ? { note: `Below the ${d.floor_gib} GiB floor: nothing new starts.` } : d.fresh === false ? { note: `Reading is ${fmtAgo(d.reading_age_s ?? 0)} old.` } : {}),
    });
  }
  const p = b?.machine?.pressure;
  if (p) {
    const m = PRESSURE[p] ?? { value: p, tone: 'neutral' as Tone };
    out.push({ key: 'pressure', label: 'Memory pressure', value: m.value, tone: m.tone });
  }
  if (typeof s.uptime_s === 'number') out.push({ key: 'uptime', label: 'Uptime', value: fmtUptime(s.uptime_s), tone: 'green' });
  const a = s.awake;
  if (a) {
    const v: { value: string; tone: Tone } = !a.enabled
      ? { value: 'off', tone: 'neutral' }
      : !a.supported
        ? { value: 'not supported here', tone: 'neutral' }
        : a.held
          ? { value: 'holding', tone: 'green' }
          : a.error
            ? { value: 'failed', tone: 'amber' }
            : { value: 'on, idle', tone: 'neutral' };
    out.push({ key: 'awake', label: 'Keep awake', ...v });
  }
  if (s.audit_path) out.push({ key: 'audit', label: 'Audit log', value: s.audit_path, mono: true });
  return out;
}

// --- needs attention -----------------------------------------------------------------------------

export interface AlarmCardModel {
  key: string;
  tone: 'red' | 'amber' | 'neutral';
  title: string;
  /** What follows the title, as text parts (set as text, never markup). */
  body: Part[];
}

const FALLBACK_TITLE = { bad: 'Needs a fix', warn: 'Needs attention', info: 'For your information' };

/** A card's body starts a sentence: the first character of a leading text part is capitalised. A daemon value (mono or strong) is never altered. */
function sentenceCase(parts: Part[]): Part[] {
  const [first, ...rest] = parts;
  if (typeof first !== 'string') return parts;
  const at = first.search(/\S/);
  if (at < 0) return parts;
  return [first.slice(0, at) + first.charAt(at).toUpperCase() + first.slice(at + 1), ...rest];
}

/** An alarm as a card: its bold head is the title, the rest the body. An alarm with no head gets a generic title. */
export function alarmCard(a: Alarm): AlarmCardModel {
  const tone = a.sev === 'bad' ? 'red' : a.sev === 'warn' ? 'amber' : 'neutral';
  const [head, ...rest] = a.parts;
  if (head && typeof head === 'object' && 'strong' in head) {
    const [first, ...more] = rest;
    // The head is followed by " — " in the alarm's own wording; the card's layout supplies the break.
    const cut = typeof first === 'string' ? first.replace(/^\s*[—-]\s+/, '').replace(/^\.\s*/, '') : first;
    return { key: a.key, tone, title: head.strong, body: sentenceCase(first === undefined ? [] : cut === '' ? more : [cut, ...more]) };
  }
  return { key: a.key, tone, title: FALLBACK_TITLE[a.sev], body: sentenceCase(a.parts) };
}
