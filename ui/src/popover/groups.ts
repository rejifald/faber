// Job groups and their pips (popover/anatomy.md § 2, Job group rows and Pips). A group is one PR
// (`demo/PR-123`), or a repo and workflow when there is no PR. Live jobs come from `inflight[]`;
// recently finished runs from `outcomes[]` join the group of the job they ran.
import { fmtAgo, plural, spokenAgo } from './format';
import type { DaemonState, Inflight, Outcome } from './types';

/**
 * One mark per job. A finished run is `done` (faber delivered the runner and GitHub concluded
 * the job: a muted check, fading over its 30 s, whether CI passed or failed) or `fault` (the
 * runner failed on faber's side: a slashed red ring, 10 min). A job's own CI failure is not
 * faber's to report (ADR-0044, amended 2026-11-01). A live job is `running`, `frozen` or
 * `waiting`. PipStrip draws each kind.
 */
export type PipKind = 'done' | 'fault' | 'running' | 'frozen' | 'waiting' | 'idle';

/** The kinds a finished run can draw, in the order they sit in a strip. */
export type FinishedKind = 'done' | 'fault';

export interface Pip {
  kind: PipKind;
  /** Stable across ticks, so a running pip's arc keeps its phase and a done pip its fade. */
  key: string;
  /** For a done pip, how long ago its verdict arrived (`verdictAt`): where its fade starts. */
  ageMs?: number;
}

/** More pips than this in a group draw as the job count and a bar of one segment per job. It matches the slot ceiling. */
export const PIPS_MAX = 6;
/** How long a faber-fault ring lingers after the run ended (anatomy § 2, Linger). */
export const FAULT_LINGER_MS = 10 * 60 * 1000;
/** How long a done pip shows after its verdict arrived; it fades over the last `DONE_FADE_MS`. */
export const DONE_LINGER_MS = 30 * 1000;
export const DONE_FADE_MS = 10 * 1000;
/**
 * CI conclusions that mean the job ran to its end on faber's runner: a done pip. Pass or fail is
 * the job's business, not faber's. `cancelled`, `skipped` and the rest draw none.
 */
const DONE_CONCLUSIONS = new Set(['success', 'failure', 'timed_out']);
/** Runner results that are faber's fault, not CI's: the red ring. */
const FAULT_RESULTS = new Set(['failure', 'infra_error', 'timeout']);

/** What a job line says after its name. */
export type JobStatus =
  | { kind: 'step'; text: string }
  | { kind: 'waiting'; text: string }
  | { kind: 'frozen'; text: string }
  | { kind: 'fault'; text: string }
  | { kind: 'done'; text: string };

export interface JobLine {
  key: string;
  name: string;
  status: JobStatus;
  /** Elapsed for a live job; time since it ended for a finished one. */
  age: string;
  /** The identity string plus the live step's name, one fact per line, for the hover title. */
  title: string;
}

export interface Group {
  key: string;
  /** The row's name: `demo/PR-2741`, or a repo with its workflow beside it. */
  name: string;
  workflow?: string;
  /** One per job, finished first. Over PIPS_MAX, PipStrip draws them as a bar. */
  pips: Pip[];
  /** The oldest live job's elapsed time, or the time since the newest outcome. */
  age: string;
  /** A line under the row: a freeze, or which runner failed. */
  note?: { tone: 'amber' | 'red'; text: string };
  jobs: JobLine[];
  /** The facts the pips show, in words: the row's accessible name. */
  label: string;
  /** The branch and other identity too long for the row. */
  title: string;
}

/** Pip kinds from most to least urgent: a compact line's one mark is the first one present. */
// `idle` is a peer's runner with no job (Other hosts); a job group never draws one.
export const URGENCY: PipKind[] = ['fault', 'frozen', 'waiting', 'running', 'done', 'idle'];

/**
 * The group's most urgent pip, the one mark of its compact line (popover.md § Jobs, grouped):
 * runner failed, then frozen, waiting, running and done. Null for a group with no
 * pips, which buildGroups never returns.
 */
export function worstPip(g: Pick<Group, 'pips'>): Pip | null {
  for (const k of URGENCY) {
    const p = g.pips.find((x) => x.kind === k);
    if (p) return p;
  }
  return null;
}

/** Each pip kind in words, as a row's label and the fold's tally say it. */
export const PIP_WORD: Record<PipKind, string> = {
  fault: 'runner failed',
  frozen: 'frozen',
  waiting: 'waiting',
  running: 'running',
  done: 'done',
  idle: 'idle',
};

const urgencyOf = (g: Pick<Group, 'pips'>) => {
  const k = worstPip(g)?.kind;
  return k ? URGENCY.indexOf(k) : URGENCY.length;
};

/** Groups by their worst pip's urgency, runner failed first; stable, so equal groups keep their order. */
export const worstFirst = <G extends Pick<Group, 'pips'>>(groups: G[]): G[] =>
  groups
    .map((g, i) => ({ g, i, u: urgencyOf(g) }))
    .sort((a, c) => a.u - c.u || a.i - c.i)
    .map((x) => x.g);

/** How many of `groups` have each pip kind as their worst, for the kinds present, in urgency order. */
export function pipTally(groups: Pick<Group, 'pips'>[]): { kind: PipKind; count: number }[] {
  const n = new Map<PipKind, number>();
  for (const g of groups) {
    const k = worstPip(g)?.kind;
    if (k) n.set(k, (n.get(k) ?? 0) + 1);
  }
  return URGENCY.filter((k) => n.has(k)).map((kind) => ({ kind, count: n.get(kind)! }));
}

/** The repo's short name (the owner is the same everywhere), or the job id for an old daemon. */
function repoLabel(repo: string | undefined, jobId: number): string {
  if (repo) return repo.includes('/') ? repo.split('/').pop()! : repo;
  return `job ${jobId}`;
}

interface Identity {
  repo?: string;
  pr?: number | null;
  branch?: string | null;
  workflow?: string | null;
  jobName?: string | null;
  event?: string | null;
}

/** The workflow's name, or the triggering event's when the daemon sent none. */
const workflowOf = (id: Identity) => id.workflow || (id.event ? id.event.replace(/_/g, ' ') : '') || 'job';

function groupIdentity(id: Identity, jobId: number): { key: string; name: string; workflow?: string } {
  const repo = repoLabel(id.repo, jobId);
  if (id.pr) return { key: `${repo}/PR-${id.pr}`, name: `${repo}/PR-${id.pr}` };
  const workflow = workflowOf(id);
  return { key: `${repo}\u0000${workflow}`, name: repo, workflow };
}

/** How the daemon joins an outcome's `label` (`repo · workflow · job`, src/reconcile.rs): parsed, never shown. */
export const DAEMON_LABEL_SEP = ' · '; // eslint-disable-line design-tokens/no-dot-separator -- the daemon's wire format, split here and never displayed

/**
 * A finished run's identity, in the same shape as a live job's so it lands in its own group
 * (anatomy § 3: "outcomes must produce the same key"). In order: the outcome's own fields (PR 2),
 * the live job this app saw with the same id (the claim's identity, right when the executed job
 * is the minted one), then the daemon's `repo · workflow · job` label, with no PR. A minted job's
 * PR is never attached to a different executed job, since only an exact id match borrows it.
 */
export function outcomeIdentity(o: Outcome, seen: ReadonlyMap<number, Inflight>): Identity {
  if (o.pr != null || o.workflow != null || o.job_name != null) {
    return { repo: o.repo, pr: o.pr, branch: o.branch, workflow: o.workflow, jobName: o.job_name, event: o.event };
  }
  const live = seen.get(o.job_id);
  if (live && (!o.repo || live.repo === o.repo)) {
    return { repo: live.repo, pr: live.pr, branch: live.branch, workflow: live.workflow, jobName: live.job_name, event: live.event };
  }
  const parts = (o.label || '').split(DAEMON_LABEL_SEP);
  if (parts.length === 3) return { repo: o.repo, workflow: parts[1], jobName: parts[2] };
  return { repo: o.repo, jobName: o.profile };
}

/**
 * When a done pip's linger starts: when the daemon read the verdict (`conclusion_ts_ms`, owner
 * 2026-10-19), so a verdict that arrives late still shows for its full time. A daemon that
 * does not send it falls back to the run's end.
 */
export const verdictAt = (o: Outcome) => o.conclusion_ts_ms ?? o.ts_ms;

/**
 * The pip a finished run's CI verdict draws, from `outcomes[].conclusion` (ADR-0044): `done` for
 * `success`, `failure` or `timed_out` within 30 s of `verdictAt`, and nothing for `cancelled`,
 * `skipped`, `neutral` or a conclusion faber could not read. CI passing and CI failing draw the
 * same pip: a job's own failure is not faber's (ADR-0044, amended 2026-11-01). An older daemon
 * sends no conclusion, so it draws none.
 */
export function verdictPip(o: Outcome, now: number): 'done' | null {
  const age = now - verdictAt(o);
  return o.conclusion && DONE_CONCLUSIONS.has(o.conclusion) && age < DONE_LINGER_MS ? 'done' : null;
}

/** A finished run that was faber's fault and is still inside its linger. */
const isFault = (o: Outcome, now: number) => FAULT_RESULTS.has(o.result || '') && now - o.ts_ms < FAULT_LINGER_MS;

/**
 * The one pip a finished run draws, or none. A runner faber lost is a fault ring even when CI
 * then concluded `failure`: that failure is the lost runner's doing, and the ring says whose.
 */
export function finishedPip(o: Outcome, now: number): FinishedKind | null {
  if (isFault(o, now)) return 'fault';
  return verdictPip(o, now);
}

/** A booting runner waits like a parked claim: only a job GitHub has started is running. */
function livePip(j: Inflight): PipKind {
  if (j.frozen) return 'frozen';
  if (!j.admitted || j.phase === 'starting') return 'waiting';
  return 'running';
}

/**
 * What each admission gate waits for, as a waiting job's line says it: "waits for memory". Only on
 * the open group's job lines: the collapsed row's clock pips say it waits (owner, 2026-10-31).
 */
const WAITS_FOR: Record<string, string> = {
  memory: 'memory',
  'vm-memory': 'VM memory',
  cpu: 'CPU',
  'cpu-pressure': 'CPU',
  ceiling: 'a slot',
  disk: 'disk',
  docker: 'Docker',
  held: 'the hold',
  paused: 'resume',
  'machine-pressure': 'the machine',
  swap: 'the machine',
  image: 'its image',
  'image-pull': 'its image',
  'image-missing': 'its image',
  pool: 'an address',
  github: 'GitHub',
  adoption: 'adoption',
  'repo-held': 'the repo hold',
  'refused-job': 'a runner',
};


/** "waits for memory", from the gate; the daemon's full detail stays in the hover title. */
function waitText(w: Inflight['waiting']): string {
  const what = w?.gate ? WAITS_FOR[w.gate] : undefined;
  return what ? `waits for ${what}` : 'waiting';
}

function frozenText(j: Inflight): string {
  return j.frozen?.reason === 'memory' ? 'frozen for memory' : 'frozen for disk';
}

function jobLine(j: Inflight): JobLine {
  const name = j.job_name || j.profile || `job ${j.job_id}`;
  let status: JobStatus;
  if (j.frozen) status = { kind: 'frozen', text: frozenText(j) };
  else if (!j.admitted) status = { kind: 'waiting', text: waitText(j.waiting) };
  else if (j.phase === 'starting') status = { kind: 'waiting', text: 'starting' };
  else status = { kind: 'step', text: j.step?.total && j.step.total > 1 ? `${j.step.index ?? 0}/${j.step.total}` : '' };
  const title = [j.repo || `job ${j.job_id}`, j.workflow, j.job_name, `job ${j.job_id}`, j.profile, j.step?.name, j.waiting?.detail]
    .filter(Boolean)
    .join('\n');
  return { key: `j${j.job_id}`, name, status, age: fmtAgo(j.elapsed_s), title };
}

/** What a finished job's line says after its name. */
const FINISHED_WORD: Record<FinishedKind, string> = { done: 'done', fault: 'runner failed' };

const outcomeKey = (o: Outcome) => `o${o.job_id}-${o.attempt ?? 0}-${o.ts_ms}`;
const finishedName = ({ o, id }: Finished) => id.jobName || o.profile || `job ${o.job_id}`;

function finishedLine(f: Finished, now: number): JobLine {
  const { o, kind } = f;
  return {
    key: outcomeKey(o),
    name: finishedName(f),
    status: { kind, text: FINISHED_WORD[kind] },
    age: fmtAgo((now - o.ts_ms) / 1000),
    title: [o.repo, o.label, `job ${o.job_id}`, `result ${o.result}`, o.conclusion && `CI ${o.conclusion}`].filter(Boolean).join('\n'),
  };
}

interface Finished {
  o: Outcome;
  id: Identity;
  kind: FinishedKind;
}

interface Building {
  key: string;
  name: string;
  workflow?: string;
  branch: string;
  live: Inflight[];
  done: Finished[];
  first: number;
}

/**
 * The popover's groups, in order: live groups holding a job admitted here (running or frozen),
 * then live groups that only wait, each by oldest job first, then groups with only finished
 * entries by newest outcome first (anatomy § 6). Under demand provisioning (ADR-0004) most
 * waiting claims are the shared queue, older than the work, so oldest-first alone folds the jobs
 * the menu-bar count names out of sight. `seen` maps job ids to the live jobs this app has seen,
 * for `outcomeIdentity`.
 */
export function buildGroups(s: DaemonState, seen: ReadonlyMap<number, Inflight>, now: number): Group[] {
  const map = new Map<string, Building>();
  let order = 0;
  const groupFor = (id: Identity, jobId: number) => {
    const g = groupIdentity(id, jobId);
    let b = map.get(g.key);
    if (!b) map.set(g.key, (b = { ...g, branch: '', live: [], done: [], first: order++ }));
    if (!b.branch && id.branch) b.branch = id.branch;
    return b;
  };
  for (const j of s.inflight || []) {
    groupFor({ repo: j.repo, pr: j.pr, branch: j.branch, workflow: j.workflow, event: j.event }, j.job_id).live.push(j);
  }
  for (const o of s.outcomes || []) {
    const kind = finishedPip(o, now);
    if (!kind) continue;
    const id = outcomeIdentity(o, seen);
    groupFor(id, o.job_id).done.push({ o, id, kind });
  }
  const groups = [...map.values()].map((b) => finish(b, now));
  const live = groups
    .filter((g) => g.liveMax !== null)
    .sort((a, c) => Number(c.here) - Number(a.here) || c.liveMax! - a.liveMax! || a.first - c.first);
  const done = groups.filter((g) => g.liveMax === null).sort((a, c) => c.newest - a.newest);
  return [...live, ...done].map((g) => g.group);
}

/** "runner failed for web unit", or "2 runners failed". */
function faultText(list: Finished[]): string | null {
  if (!list.length) return null;
  const names = new Set(list.map(finishedName));
  return names.size === 1 ? `runner failed for ${[...names][0]}` : `${plural(list.length, 'runner')} failed`;
}

const FINISHED_ORDER: FinishedKind[] = ['done', 'fault'];

function finish(b: Building, now: number) {
  // Newest first within a kind, the kinds in strip order: done, fault (anatomy § 2).
  const done = [...b.done].sort(
    (a, c) => FINISHED_ORDER.indexOf(a.kind) - FINISHED_ORDER.indexOf(c.kind) || c.o.ts_ms - a.o.ts_ms,
  );
  const delivered = done.filter((f) => f.kind === 'done');
  const faults = done.filter((f) => f.kind === 'fault');
  const kinds = b.live.map(livePip);
  const running = kinds.filter((k) => k === 'running').length;
  const frozen = kinds.filter((k) => k === 'frozen').length;
  const waiting = kinds.filter((k) => k === 'waiting').length;

  // Finished first, then running, frozen, waiting (anatomy § 2).
  const order: PipKind[] = ['running', 'frozen', 'waiting'];
  const livePips = [...b.live]
    .map((j) => ({ kind: livePip(j), key: `j${j.job_id}` }))
    .sort((a, c) => order.indexOf(a.kind) - order.indexOf(c.kind));
  const finishedPips: Pip[] = done.map(({ o, kind }) =>
    kind === 'done' ? { kind, key: outcomeKey(o), ageMs: Math.max(0, now - verdictAt(o)) } : { kind, key: outcomeKey(o) },
  );
  const pips: Pip[] = [...finishedPips, ...livePips];

  const liveMax = b.live.length ? Math.max(...b.live.map((j) => j.elapsed_s || 0)) : null;
  const newest = done.length ? Math.max(...done.map((f) => f.o.ts_ms)) : 0;
  const ageS = liveMax ?? (now - newest) / 1000;
  const age = liveMax !== null ? fmtAgo(liveMax) : fmtAgo(ageS);

  let note: Group['note'];
  const redText = faultText(faults);
  if (!b.live.length && redText) {
    note = { tone: 'red', text: redText };
  } else if (frozen && !running) {
    note = { tone: 'amber', text: frozenText(b.live.find((j) => j.frozen)!) };
  }

  const words = [
    running && `${running} running`,
    frozen && `${frozen} frozen`,
    waiting && `${waiting} waiting`,
    delivered.length && `${delivered.length} done`,
    faults.length && `${plural(faults.length, 'runner')} failed`,
  ].filter(Boolean);
  const label = [b.workflow ? `${b.name} ${b.workflow}` : b.name, ...words, spokenAgo(ageS)].join(', ');

  const jobs = [...done.map((f) => finishedLine(f, now)), ...b.live.map(jobLine)];
  const title = [b.workflow ? `${b.name}\n${b.workflow}` : b.name, b.branch].filter(Boolean).join('\n');

  return {
    first: b.first,
    here: running + frozen > 0,
    liveMax,
    newest,
    group: { key: b.key, name: b.name, workflow: b.workflow, pips, age, note, jobs, label, title },
  };
}

/** Remembers each live job's identity across snapshots, for the outcomes that follow it. */
export function rememberLive(seen: Map<number, Inflight>, s: DaemonState, now: number, keepMs = FAULT_LINGER_MS) {
  for (const j of s.inflight || []) seen.set(j.job_id, j);
  // Forget a job once nothing can still refer to it: no longer live, and no outcome of it lingers.
  const live = new Set((s.inflight || []).map((j) => j.job_id));
  const recent = new Set((s.outcomes || []).filter((o) => now - o.ts_ms < keepMs).map((o) => o.job_id));
  if (seen.size > 500) {
    for (const id of seen.keys()) if (!live.has(id) && !recent.has(id)) seen.delete(id);
  }
}
