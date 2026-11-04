// The Activity view's data shaping (docs/design/dashboard/screens.md S4, S5): every kind of event
// the daemon keeps in memory merged into one list, newest first, with the facets, filters and
// query string the view builds on. Pure, so the view's tests run it without a DOM.
import { fmtAgo } from '../popover/format';
import type { DaemonState } from '../popover/types';

export type EventKind = 'outcome' | 'decision' | 'fault' | 'retry' | 'drift' | 'bypass' | 'refused';
export type EventTone = 'green' | 'amber' | 'red' | 'neutral' | 'accent';

/** Kinds in the order the facet lists them. */
export const KINDS: EventKind[] = ['outcome', 'decision', 'fault', 'retry', 'drift', 'bypass', 'refused'];
export const KIND_LABEL: Record<EventKind, string> = {
  outcome: 'Outcome',
  decision: 'Decision',
  fault: 'Fault',
  retry: 'Retry',
  drift: 'Drift',
  bypass: 'Bypass',
  refused: 'Refused',
};

export interface EventField {
  key: string;
  label: string;
  value: string;
}

export interface ActivityEvent {
  key: string;
  /** When it happened; null for a current-state row with no time, which sorts first and reads "now". */
  ts_ms: number | null;
  kind: EventKind;
  /** The status label, in sentence case: "Delivered", "Infra error". Also the Result facet's value. */
  result: string;
  repo: string;
  pr?: number;
  /** The job's name, or '' when the event has none. */
  job: string;
  jobId?: number;
  runner?: string;
  /** The attempt number, when the daemon reports one; 2 or more is a retried run. */
  attempt?: number;
  /** Decisions only. */
  actor?: string;
  detail: string;
  /** How long it took, formatted; absent when the event has no duration. */
  took?: string;
  tone: EventTone;
  fields: EventField[];
  /** Long cause or error text, set in a CodeBlock. */
  message?: string;
}

/** 8m 12s, 3h, 34s. */
export function fmtDur(seconds: number): string {
  const s = Math.max(0, Math.round(seconds));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m${s % 60 ? ` ${String(s % 60).padStart(2, '0')}s` : ''}`;
  const m = Math.floor((s % 3600) / 60);
  return `${Math.floor(s / 3600)}h${m ? ` ${String(m).padStart(2, '0')}m` : ''}`;
}

// --- the daemon's codes in words ----------------------------------------------------------------

/** Trigger events as GitHub names them (`pull_request`), in words; an event not listed reads as its code. */
const EVENT_WORDS: Record<string, string> = {
  push: 'Push',
  pull_request: 'Pull request',
  pull_request_target: 'Pull request (target)',
  workflow_dispatch: 'Manual run',
  workflow_run: 'Workflow run',
  workflow_call: 'Workflow call',
  repository_dispatch: 'Repository dispatch',
  schedule: 'Schedule',
  merge_group: 'Merge group',
  release: 'Release',
  issue_comment: 'Issue comment',
};

export const eventWords = (event: string): string => EVENT_WORDS[event] ?? event;

/**
 * A gate verdict (`refuse:event:repository_dispatch`, src/reconcile.rs `verdict_str`) as the reason
 * alone: "repository_dispatch is not an allowed event". The skip and provision verdicts are
 * phrased on their own. A code not listed reads as itself.
 */
export function verdictReason(verdict: string): string {
  const [head, kind, ...rest] = verdict.split(':');
  const value = rest.join(':');
  if (head === 'refuse') {
    if (kind === 'public_repo') return 'public repositories are not served by this host';
    if (kind === 'fork') return 'the change comes from a fork';
    if (kind === 'event' && value) return `${value} is not an allowed event`;
    if (kind === 'actor' && value) return `${value} is not a trusted actor`;
  }
  if (head === 'skip') {
    if (kind === 'not_targeted') return 'its labels do not target faber';
    if (kind === 'not_queued') return 'it is no longer queued';
  }
  if (verdict === 'provision') return 'queued for admission';
  return verdict;
}

/** A verdict as a sentence: a refusal says so ("Refused: ..."); the rest read as their reason, capitalised. */
export function verdictText(verdict: string): string {
  const reason = verdictReason(verdict);
  if (reason === verdict) return verdict;
  return verdict.startsWith('refuse') ? `Refused: ${reason}` : reason.charAt(0).toUpperCase() + reason.slice(1);
}

const sentence = (w: string) => {
  const t = w.replace(/[_-]+/g, ' ').trim();
  return t.charAt(0).toUpperCase() + t.slice(1);
};
const firstLine = (s: string) => s.split('\n', 1)[0];

function fields(list: Array<[string, string, string | number | null | undefined]>): EventField[] {
  return list.filter(([, , v]) => v !== undefined && v !== null && v !== '').map(([key, label, v]) => ({ key, label, value: String(v) }));
}

const OUTCOME: Record<string, [string, EventTone]> = {
  success: ['Delivered', 'green'],
  failure: ['Failed', 'red'],
  infra_error: ['Infra error', 'red'],
  timeout: ['Timed out', 'amber'],
  timed_out: ['Timed out', 'amber'],
  reclaimed: ['Reclaimed', 'neutral'],
  oom: ['Out of memory', 'red'],
};

function outcomeEvents(s: DaemonState): ActivityEvent[] {
  return (s.outcomes ?? []).map((o, i) => {
    const raw = o.result ?? 'unknown';
    const [result, tone] = OUTCOME[raw] ?? [sentence(raw), 'neutral' as EventTone];
    const parts: string[] = [];
    if (o.cause) parts.push(firstLine(o.cause));
    else {
      if ((o.attempt ?? 1) > 1) parts.push(`attempt ${o.attempt}`);
      if (o.wait_s && o.wait_s > 0) parts.push(`${fmtDur(o.wait_s)} of it parked${o.wait_gate ? ` on ${o.wait_gate}` : ''}`);
    }
    return {
      key: `outcome:${i}:${o.job_id}:${o.ts_ms}`,
      ts_ms: o.ts_ms,
      kind: 'outcome',
      result,
      repo: o.repo ?? '',
      pr: o.pr ?? undefined,
      job: o.job_name ?? o.profile ?? '',
      jobId: o.job_id,
      runner: o.runner,
      attempt: o.attempt ?? undefined,
      detail: parts.join(' | '),
      took: o.duration_s !== undefined ? fmtDur(o.duration_s) : undefined,
      tone,
      message: o.cause ?? undefined,
      fields: fields([
        ['runner', 'Runner', o.runner],
        ['attempt', 'Attempt', o.attempt],
        ['profile', 'Profile', o.profile],
        ['wait', 'Wait', o.wait_s !== undefined ? fmtDur(o.wait_s) : undefined],
        ['duration', 'Duration', o.duration_s !== undefined ? fmtDur(o.duration_s) : undefined],
        ['job', 'Job ID', o.job_id],
      ]),
    };
  });
}

function decisionEvents(s: DaemonState): ActivityEvent[] {
  return (s.recent ?? []).map((d, i) => {
    const refused = d.verdict.startsWith('refuse');
    const skipped = d.verdict.startsWith('skip');
    const result = refused ? 'Refused' : skipped ? 'Skipped' : sentence(d.verdict);
    const why = d.verdict === 'provision' || d.verdict.includes(':') ? verdictText(d.verdict) : '';
    return {
      key: `decision:${i}:${d.job_id}:${d.ts_ms}`,
      ts_ms: d.ts_ms,
      kind: 'decision',
      result,
      repo: d.repo,
      job: '',
      jobId: d.job_id,
      actor: d.actor,
      detail: [d.event ? eventWords(d.event) : '', why].filter(Boolean).join(' | '),
      tone: refused ? 'amber' : 'neutral',
      fields: fields([
        ['actor', 'Actor', d.actor],
        ['profile', 'Profile', d.profile],
        ['event', 'Event', d.event],
        ['verdict', 'Verdict', d.verdict],
        ['job', 'Job ID', d.job_id],
      ]),
    };
  });
}

const FAULT_ENDS = new Set(['recovered', 'hold_cleared']);

function faultEvents(s: DaemonState): ActivityEvent[] {
  return (s.faults ?? []).map((f, i) => {
    const ended = FAULT_ENDS.has(f.action);
    return {
      key: `fault:${i}:${f.ts_ms}`,
      ts_ms: f.ts_ms,
      kind: 'fault',
      result: ended ? 'Recovered' : 'Failed',
      repo: f.repo,
      job: '',
      detail: firstLine(f.cause),
      tone: ended ? 'green' : 'red',
      message: f.cause,
      fields: fields([
        ['scope', 'Scope', f.scope],
        ['action', 'Action', f.action],
      ]),
    };
  });
}

function retryEvents(s: DaemonState): ActivityEvent[] {
  return (s.retries ?? []).map((r, i) => ({
    key: `retry:${i}:${r.run_id}:${r.ts_ms}`,
    ts_ms: r.ts_ms,
    kind: 'retry',
    result: 'Retried',
    repo: r.repo,
    job: '',
    detail: `re-dispatched ${r.jobs} ${r.jobs === 1 ? 'job' : 'jobs'} as attempt ${r.attempt}, mode ${r.mode}`,
    tone: 'accent',
    fields: fields([
      ['attempt', 'Attempt', r.attempt],
      ['jobs', 'Jobs', r.jobs],
      ['mode', 'Mode', r.mode],
      ['run', 'Run ID', r.run_id],
    ]),
  }));
}

function driftEvents(s: DaemonState): ActivityEvent[] {
  return (s.drift ?? []).map((d, i) => ({
    key: `drift:${i}:${d.job_id}:${d.ts_ms}`,
    ts_ms: d.ts_ms,
    kind: 'drift',
    result: 'Drift',
    repo: d.repo,
    job: '',
    jobId: d.job_id,
    detail: d.pinned && d.baked ? `pins ${d.pinned}, the runner image carries ${d.baked}` : `${d.status}, ${d.action}`,
    tone: 'amber',
    fields: fields([
      ['status', 'Status', d.status],
      ['baked', 'Baked', d.baked],
      ['pinned', 'Pinned', d.pinned],
      ['mode', 'Mode', d.mode],
      ['action', 'Action', d.action],
      ['job', 'Job ID', d.job_id],
    ]),
  }));
}

function bypassEvents(s: DaemonState): ActivityEvent[] {
  return (s.bypasses ?? []).map((b, i) => ({
    key: `bypass:${i}:${b.job_id}:${b.ts_ms}`,
    ts_ms: b.ts_ms,
    kind: 'bypass',
    result: 'Bypassed',
    repo: b.repo,
    job: b.job_name,
    jobId: b.job_id,
    runner: b.runner,
    detail: `${b.class} gate refused it (${verdictReason(b.verdict)}) but it ran`,
    tone: 'red',
    fields: fields([
      ['runner', 'Runner', b.runner],
      ['workflow', 'Workflow', b.workflow],
      ['class', 'Class', b.class],
      ['verdict', 'Verdict', b.verdict],
      ['job', 'Job ID', b.job_id],
    ]),
  }));
}

/** Jobs refused right now (S5): each row carries when it began, or none and reads "now". */
function refusedEvents(s: DaemonState): ActivityEvent[] {
  const base = (repo: string, job_name: string, job_id: number | undefined, since: number | undefined, key: string) => ({
    key,
    ts_ms: since ?? null,
    kind: 'refused' as const,
    result: 'Refused',
    repo,
    job: job_name,
    jobId: job_id,
    tone: 'amber' as EventTone,
  });
  return [
    ...(s.refused_queued ?? []).map((r, i) => ({
      ...base(r.repo ?? '', r.job_name ?? '', r.job_id, r.since_ms, `refused:queued:${i}:${r.job_id}`),
      detail: r.verdict ? verdictText(r.verdict) : '',
      fields: fields([
        ['workflow', 'Workflow', r.workflow],
        ['event', 'Event', r.event],
        ['verdict', 'Verdict', r.verdict],
        ['labels', 'Labels', r.labels?.join(', ')],
        ['job', 'Job ID', r.job_id],
        ['run', 'Run ID', r.run_id],
      ]),
    })),
    ...(s.dind_refused ?? []).map((r, i) => ({
      ...base(r.repo, r.job_name, r.job_id, r.at_ms, `refused:dind:${i}:${r.job_id}`),
      detail: r.reason,
      message: r.reason,
      fields: fields([
        ['profile', 'Profile', r.profile],
        ['job', 'Job ID', r.job_id],
        ['run', 'Run ID', r.run_id],
      ]),
    })),
    ...(s.service_refused ?? []).map((r, i) => ({
      ...base(r.repo, r.job_name, r.job_id, r.at_ms, `refused:service:${i}:${r.job_id}`),
      detail: r.reason,
      message: r.reason,
      fields: fields([
        ['profile', 'Profile', r.profile],
        ['label', 'Label', r.label],
        ['job', 'Job ID', r.job_id],
        ['run', 'Run ID', r.run_id],
      ]),
    })),
  ];
}

/** Every kind merged, newest first. Rows with no time come first; ties keep their source order. */
export function buildEvents(s: DaemonState): ActivityEvent[] {
  const all = [
    ...outcomeEvents(s),
    ...decisionEvents(s),
    ...faultEvents(s),
    ...retryEvents(s),
    ...driftEvents(s),
    ...bypassEvents(s),
    ...refusedEvents(s),
  ];
  return all
    .map((e, i) => ({ e, i }))
    .sort((a, b) => (b.e.ts_ms ?? Infinity) - (a.e.ts_ms ?? Infinity) || a.i - b.i)
    .map(({ e }) => e);
}

/** "2m ago", or "now" for a row with no time. */
export function whenLabel(e: ActivityEvent, now: number): string {
  return e.ts_ms === null ? 'now' : `${fmtAgo((now - e.ts_ms) / 1000)} ago`;
}

// --- filters ------------------------------------------------------------------------------------

export type FacetId = 'kind' | 'result' | 'repo' | 'actor';
export interface Filters {
  /** Chosen values by facet; any number per facet. Kind holds EventKind, the rest the row's own text. */
  facets: Record<FacetId, string[]>;
  q: string;
}
export const emptyFilters = (): Filters => ({ facets: { kind: [], result: [], repo: [], actor: [] }, q: '' });

export function isFiltered(f: Filters): boolean {
  return f.q.trim() !== '' || Object.values(f.facets).some((v) => v.length > 0);
}

const valueOf = (e: ActivityEvent, id: FacetId): string | undefined =>
  id === 'kind' ? e.kind : id === 'result' ? e.result : id === 'repo' ? e.repo || undefined : e.actor;

/** AND across facets, OR within one; the search matches repo, runner, job id or job name. */
export function applyFilters(events: ActivityEvent[], f: Filters): ActivityEvent[] {
  const q = f.q.trim().toLowerCase();
  const ids = Object.keys(f.facets) as FacetId[];
  return events.filter((e) => {
    for (const id of ids) {
      const chosen = f.facets[id];
      if (chosen.length === 0) continue;
      const v = valueOf(e, id);
      if (v === undefined || !chosen.includes(v)) return false;
    }
    if (!q) return true;
    return [e.repo, e.runner, e.jobId === undefined ? undefined : String(e.jobId), e.job].some((h) => h !== undefined && h.toLowerCase().includes(q));
  });
}

export interface FacetItemData {
  value: string;
  label: string;
  count: number;
  tone?: EventTone;
}
export interface FacetGroupData {
  id: FacetId;
  label: string;
  items: FacetItemData[];
}

/** The facet groups with counts over the whole list; Actor comes from decisions only. */
export function buildFacets(events: ActivityEvent[]): FacetGroupData[] {
  const tally = (id: FacetId) => {
    const m = new Map<string, FacetItemData>();
    for (const e of events) {
      const v = valueOf(e, id);
      if (v === undefined) continue;
      const hit = m.get(v);
      if (hit) hit.count += 1;
      else m.set(v, { value: v, label: v, count: 1, tone: id === 'result' ? e.tone : undefined });
    }
    return [...m.values()].sort((a, b) => b.count - a.count || a.label.localeCompare(b.label));
  };
  const kinds = KINDS.map((k) => ({ value: k, label: KIND_LABEL[k], count: events.filter((e) => e.kind === k).length })).filter((i) => i.count > 0);
  return [
    { id: 'kind', label: 'Kind', items: kinds },
    { id: 'result', label: 'Result', items: tally('result') },
    { id: 'repo', label: 'Repo', items: tally('repo') },
    { id: 'actor', label: 'Actor', items: tally('actor') },
  ];
}

// --- the query string (ADR-0054 § 5) ------------------------------------------------------------

/** Filters from a `location.search`: `?kind=fault&kind=retry&repo=acme/web&q=lint`. */
export function parseQuery(search: string): Filters {
  const p = new URLSearchParams(search);
  const f = emptyFilters();
  (Object.keys(f.facets) as FacetId[]).forEach((id) => {
    f.facets[id] = p.getAll(id).filter(Boolean);
  });
  f.q = p.get('q') ?? '';
  return f;
}

/** The filters as a query string with its `?`, or '' when none are set. */
export function toQuery(f: Filters): string {
  const p = new URLSearchParams();
  (Object.keys(f.facets) as FacetId[]).forEach((id) => f.facets[id].forEach((v) => p.append(id, v)));
  if (f.q.trim()) p.set('q', f.q);
  const s = p.toString();
  return s ? `?${s}` : '';
}

/** The page's own query string, or '' with no window (tests, server rendering). */
export function readLocationQuery(): string {
  return typeof location === 'undefined' ? '' : location.search;
}

/** Writes the query into the address bar without adding history entries; a no-op with no window. */
export function writeLocationQuery(search: string): void {
  if (typeof location === 'undefined' || typeof history === 'undefined') return;
  if (location.search === search) return;
  history.replaceState(history.state, '', `${location.pathname}${search}${location.hash}`);
}

