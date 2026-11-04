// What the Maintenance view draws, shaped from the daemon's state (docs/design/dashboard/screens.md
// § 4, S13). Pure: the screen renders these view models, and ui/scripts/maintenance.test.mjs pins them.
import { fmtAgo, fmtDateTime, plural, shellWord, waitIn } from '../popover/format';
import type { DaemonState, UpdateView } from '../popover/types';

type Tone = 'green' | 'amber' | 'red' | 'neutral' | 'accent';

// --- host ----------------------------------------------------------------------------------------

export interface HostStateModel {
  /** The state word: Running, Paused or Draining. */
  word: 'Running' | 'Paused' | 'Draining';
  tone: Tone;
  /** What it means, in a phrase. */
  detail: string;
}

/** Jobs a drain still waits for: the admitted ones (a parked claim is cancelled, not waited for). */
export function runningJobs(s: DaemonState): number {
  return (s.inflight || []).filter((j) => j.admitted !== false).length;
}

/** The host's one state: a drain outranks a pause, since a drain cannot be undone. */
export function hostState(s: DaemonState): HostStateModel {
  if (s.draining) {
    const n = runningJobs(s);
    return { word: 'Draining', tone: 'amber', detail: n ? `${plural(n, 'job')} still running` : 'No job is running' };
  }
  if (s.paused) return { word: 'Paused', tone: 'amber', detail: 'No new job starts; running jobs finish' };
  return { word: 'Running', tone: 'green', detail: 'Claiming jobs' };
}

/** The sidebar's count for Maintenance: a short amber word when the host is not simply running, else none. */
export function navState(s: DaemonState): { count: string; label: string } | null {
  if (s.draining) return { count: 'Draining', label: 'the host is draining' };
  if (s.paused) return { count: 'Paused', label: 'the host is paused' };
  if (s.budget?.hold) return { count: 'Held', label: 'Docker admission is held' };
  return null;
}

// --- docker admission and repository holds -------------------------------------------------------

export interface HoldModel {
  held: boolean;
  tone: Tone;
  value: string;
  note?: string;
}

export function holdRow(s: DaemonState, now: number): HoldModel {
  const h = s.budget?.hold;
  if (!h) return { held: false, tone: 'green', value: 'Open' };
  const since = h.since_ms ? `Since ${fmtDateTime(h.since_ms, now)}` : '';
  return { held: true, tone: 'amber', value: 'Held', note: [h.reason || 'No reason given', since].filter(Boolean).join('. ') };
}

/** The command that clears a repo's hold, which needs sudo (ADR-0030). */
export const clearCommand = (repo: string) => `sudo faber repo-hold clear ${shellWord(repo)}`;

export interface RepoHoldRow {
  key: string;
  repo: string;
  /** Reason, since and by, as one line. */
  note: string;
  command: string;
}

export function repoHoldRows(s: DaemonState, now: number): RepoHoldRow[] {
  return (s.repo_holds || []).map((r) => ({
    key: r.repo,
    repo: r.repo,
    note: [r.reason, r.since_ms ? `since ${fmtDateTime(r.since_ms, now)}` : '', r.by ? `by ${r.by}` : ''].filter(Boolean).join(', '),
    command: clearCommand(r.repo),
  }));
}

// --- golden images -------------------------------------------------------------------------------

/** A golden image's standing in words (`verified`, `unverified`, `verifying`, `failed:<check>`, `unknown`). */
export function goldenStanding(standing: string | undefined): { word: string; tone: Tone } {
  if (standing === 'verified') return { word: 'Verified', tone: 'green' };
  if (standing === 'unverified') return { word: 'Not verified', tone: 'amber' };
  if (standing === 'verifying') return { word: 'Verifying', tone: 'accent' };
  if (standing?.startsWith('failed')) {
    const check = standing.slice('failed'.length).replace(/^:/, '');
    return { word: check ? `Failed: ${check}` : 'Failed', tone: 'red' };
  }
  return { word: 'Not judged yet', tone: 'neutral' };
}

export interface GoldenRow {
  key: string;
  image: string;
  word: string;
  tone: Tone;
  note?: string;
}

/** One row per golden image; none when no profile uses one. */
export function goldenRows(s: DaemonState): GoldenRow[] {
  return (s.budget?.vm?.golden || []).map((g) => ({ key: g.image, image: g.image, ...goldenStanding(g.standing), ...(g.detail ? { note: g.detail } : {}) }));
}

// --- updates -------------------------------------------------------------------------------------

export interface UpdateModel {
  current: string;
  /** The version on offer and why, when there is one. */
  available: { version: string; why?: string } | null;
  /** The phase in words, or null while idle with nothing to say. */
  phase: { word: string; tone: Tone; note?: string; fallbackNote?: true } | null;
  /** The buttons the phase allows. */
  canCheck: boolean;
  canInstall: boolean;
  canSkip: boolean;
  /** Last check, as an age. */
  lastCheck?: string;
}

/**
 * The updater's phase in words: the one wording Maintenance's Updates panel, the sidebar's update card
 * and the dashboard's update alarms share. An alarm stands alone, so it names the version
 * (`withVersion`, the default); the panel and the card sit under the version already and drop it.
 * `fallbackNote` marks a note the daemon gave no reason for, which reads fine in a panel but not
 * mid-sentence in an alarm.
 */
export function phaseWords(u: UpdateView, { withVersion = true }: { withVersion?: boolean } = {}): UpdateModel['phase'] {
  const phase = u.phase?.phase || 'idle';
  const ver = u.phase?.version || u.available || '';
  const v = withVersion && ver ? ` ${ver}` : '';
  switch (phase) {
    case 'checking':
      return { word: 'Checking for a release', tone: 'accent' };
    case 'downloading':
      return { word: `Downloading${v}`, tone: 'accent' };
    case 'waiting_idle':
      return { word: withVersion && ver ? `${ver} installs when this host goes idle` : 'Installs when this host goes idle', tone: 'amber' };
    case 'draining':
      return { word: `Installing${v}`, tone: 'amber', note: 'Refusing new jobs while the running ones finish.' };
    case 'deferred': {
      const reason = u.phase?.reason;
      return { word: withVersion && ver ? `${ver} held back` : 'Held back', tone: 'amber', note: reason || 'Deferred', ...(reason ? {} : { fallbackNote: true }) };
    }
    case 'failed': {
      const reason = u.phase?.reason;
      return { word: 'Update failed', tone: 'red', note: reason || 'No reason given', ...(reason ? {} : { fallbackNote: true }) };
    }
    case 'disabled':
      return { word: 'Updates are off', tone: 'neutral' };
    default:
      if (u.available && u.install_in_secs != null) {
        return { word: withVersion ? `${u.available} installs in ${waitIn(u.install_in_secs)}` : `Installs in ${waitIn(u.install_in_secs)}`, tone: 'amber' };
      }
      return null;
  }
}

export function updateModel(s: DaemonState, now: number): UpdateModel | null {
  const u = s.update;
  if (!u) return null;
  const phase = u.phase?.phase || 'idle';
  const busy = phase === 'checking' || phase === 'downloading' || phase === 'draining';
  const offered = Boolean(u.available) && !busy;
  return {
    current: u.current || 'unknown',
    available: u.available ? { version: u.available, ...(u.candidate_reason ? { why: u.candidate_reason } : {}) } : null,
    // Under a version row the words drop the version; with no version on offer they keep it.
    phase: phaseWords(u, { withVersion: !u.available }),
    canCheck: phase !== 'disabled' && !busy,
    canInstall: phase === 'waiting_idle',
    canSkip: offered && phase !== 'disabled',
    ...(u.last_check_ms ? { lastCheck: `${fmtAgo(Math.max(0, (now - u.last_check_ms) / 1000))} ago` } : {}),
  };
}
