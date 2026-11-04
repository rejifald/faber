// The attention list (popover.md, Attention list): every active issue as its own row, worst
// first. Rows are data; the screen draws them. Daemon and GitHub text stays in string parts,
// which React sets as text, never as markup.
import { fmtAgo, fmtClock, fmtDateTime, gib1, plural, rearmWhen, shellWord, waitIn } from './format';
import { DAEMON_LABEL_SEP } from './groups';
import type { AppUpdate, AttentionState, Budget, DaemonState, MirrorRow, UpdateView, VmView } from './types';

export type Severity = 'bad' | 'warn' | 'info';
/** The row's leading glyph; the screen maps each to a lucide icon. */
export type AttentionIcon = 'alert' | 'pause' | 'frozen' | 'info' | 'update';

/** A run of row text: plain, the bold head, a mono value, or a quiet pipe between independent values. */
export type Part = string | { strong: string } | { mono: string } | { sep: true };

export interface RowAction {
  label: string;
  primary?: boolean;
  /** The Tauri command the action runs, and its arguments. */
  cmd: string;
  args?: Record<string, unknown>;
  /** Added to the error a failed command reports, when the fix is a command to run. */
  failHint?: string;
  /** Asked in the row before the command runs, for an action that grants more than faber would. */
  confirm?: { question: string; consequence: string; label: string };
}

export interface AttentionRow {
  key: string;
  sev: Severity;
  icon: AttentionIcon;
  parts: Part[];
  actions?: RowAction[];
  /**
   * ADR-0064 § 1: the row says jobs that would run here cannot start until something changes, or
   * that faber's own record cannot be trusted. It carries no Dismiss. A new row is dismissable
   * unless it sets this, so ask "does this stop jobs?" of every new row.
   */
  blocking?: boolean;
  /** ADR-0064 § 2: what a dismissal of this row remembers, when its words are not the best identity (an update's version). */
  ident?: string;
  /** A trust change's time (ADR-0028): what Acknowledge records, and what folds it. */
  ts_ms?: number;
  /** Lines the row opens to list, under a "Show <label>" control: the trust changes an acknowledgement folded. */
  details?: { label: string; lines: Part[][] };
}

/** Attention rows shown before the rest fold into "+N more". */
export const ATTENTION_MAX = 3;

const TRIP_CAUSES: Record<string, string> = {
  freeze: 'a memory freeze',
  host_oom: 'a host OOM kill',
  probe: 'a failed memory probe',
};

function reserveRow(b: Budget, now: number): AttentionRow | null {
  const r = b.reserve;
  if (!r || !r.cooling) return null;
  const cause = r.last_trip?.cause && TRIP_CAUSES[r.last_trip.cause] ? ` after ${TRIP_CAUSES[r.last_trip.cause]}` : '';
  const back = typeof r.next_rearm_ms === 'number' ? ` — p75 again ${rearmWhen(r.next_rearm_ms, now)}` : '';
  const trips = typeof r.trips_7d === 'number' && r.trips_7d > 0 ? ` (${plural(r.trips_7d, 'trip')} this week)` : '';
  return { key: 'reserve', sev: 'warn', icon: 'frozen', parts: [{ strong: `Reserving the full cap${cause}` }, `${back}${trips}`] };
}

/**
 * A retirement's identity (ADR-0064 § 2), shared with the dashboard's row for the same profile so a
 * dismissal holds on both: its profile and cutoff, and whether it is urgent, so turning urgent shows it again.
 */
export const retireIdent = (r: NonNullable<DaemonState['runner_retirements']>[number]) =>
  `${r.profile}@${r.cutoff ?? ''}${r.urgent ? ':urgent' : ''}`;

// Keyed by its first profile, like the dashboard's row for it.
function retirementRow(list: NonNullable<DaemonState['runner_retirements']>, sev: Severity): AttentionRow {
  const r = list[0];
  const more = list.length > 1 ? ` (+${list.length - 1} more)` : '';
  const d = r.days_remaining;
  const head =
    d > 1 ? `Runner retiring in ${d} days` : d === 1 ? 'Runner retiring tomorrow' : d === 0 ? 'Runner retiring today' : 'Runner retired';
  return {
    key: `retire:${r.profile}`,
    sev,
    icon: 'alert',
    // One profile: the dashboard's identity, so a dismissal holds on both. A group stands for
    // every profile in it, so any joining or leaving shows the row again.
    ident: list.length === 1 ? retireIdent(r) : list.map(retireIdent).join(','),
    // A runner already past its cutoff runs no job; one still retiring does.
    blocking: d < 0,
    parts: [
      { strong: `${head}: ${r.profile}` },
      `${more} — actions/runner ${r.runner_version ?? ''} stops on ${r.cutoff ?? ''}. Rebuild with a newer RUNNER_VERSION.`,
    ],
  };
}

/**
 * ADR-0053: a class OOM-killed on recent runs that faber stopped growing at the escalation
 * threshold. The worst one is named, the rest counted. Its label's separators become the quiet pipe.
 * Pin sets the caps the daemon says stop the OOMs; an older daemon without them gets no button.
 */
function oomHeldRow(list: NonNullable<Budget['oom_held']>): AttentionRow | null {
  const h = list[0];
  if (!h) return null;
  const more = list.length > 1 ? ` (+${list.length - 1} more)` : '';
  const label: Part[] = (h.label || h.key)
    .split(DAEMON_LABEL_SEP)
    .flatMap((seg, i): Part[] => (i ? [{ sep: true }, seg] : [seg]));
  const runs = h.ooms != null && h.runs != null ? ` on ${h.ooms} of its last ${plural(h.runs, 'run')}` : '';
  const pin = h.pin_runner_mib && h.pin_dind_mib ? h.pin_runner_mib + h.pin_dind_mib : 0;
  // Both figures are the class's whole reservation, runner + dind. With the button, the row stays
  // short enough for three lines at 240 px: the button is the ask.
  const held = h.reservation_mib || h.cap_mib;
  const ask = pin ? '.' : ', the most faber grants unpinned. Pin it higher, or make the job lighter.';
  return {
    key: 'oom-held',
    sev: 'bad',
    icon: 'alert',
    parts: [
      { strong: `OOM-killed${runs}` },
      ': ',
      ...label,
      `${more} — held at ${gib1(held)} GiB${ask}`,
    ],
    actions: pin
      ? [
          {
            label: `Pin at ${gib1(pin)} GiB`,
            cmd: 'set_pin',
            args: { class: h.key, runnerMib: h.pin_runner_mib, dindMib: h.pin_dind_mib, pin: true },
            // A pin past the escalation threshold is the owner accepting the risk (#385): asked first.
            confirm: {
              question: `Pin at ${gib1(pin)} GiB?`,
              consequence: `Each of its jobs reserves ${gib1(pin)} GiB, more than faber grants a class on its own, and faber stops learning its size. Unpin it in the panel.`,
              label: 'Pin',
            },
          },
        ]
      : undefined,
  };
}

// The gates only macOS jobs park on, so their parked count is macOS jobs. The ones a person must
// open (ADR-0051, ADR-0021) are a row even with no job waiting; VM memory clears on its own and
// shows only while it holds a job. The wall and the cap have rows of their own, and `disk` is
// Docker's gate too, so its count is not macOS jobs: the disk rows say it.
const MACOS_GATES = new Map<string, string | null>([
  ['vm_session', 'No macOS VM session'],
  ['share', 'No machine share for macOS VMs'],
  ['vm_memory', null],
]);

function capitalize(s: string): string {
  return s ? s[0].toUpperCase() + s.slice(1) : s;
}

function waitsLine(parked: number): string {
  return parked ? `${plural(parked, 'macOS job')} ${parked === 1 ? 'waits' : 'wait'}. ` : 'macOS jobs will wait. ';
}

/** Why macOS jobs are not starting, and how many wait on it: the parked rows alone bury it. */
function macosRefusalRow(vm: VmView, gates: Budget['gates']): AttentionRow | null {
  const gate = vm.refusal?.gate;
  if (!gate || !MACOS_GATES.has(gate)) return null;
  const parked = gates?.[gate]?.parked_now ?? 0;
  const head = MACOS_GATES.get(gate);
  // A session not probed yet is the first seconds after a start, not a fault.
  if ((!head || vm.session === 'unprobed') && !parked) return null;
  // The session's detail says what to do; the refusal's repeats the head before it.
  const detail = (gate === 'vm_session' && vm.session_detail) || vm.refusal?.detail || '';
  return {
    key: `macos-refusal:${gate}`,
    sev: head && parked ? 'bad' : 'warn',
    icon: 'alert',
    blocking: parked > 0,
    parts: [{ strong: head || 'macOS jobs held' }, ` — ${waitsLine(parked)}${capitalize(detail)}`],
  };
}

/**
 * A golden image that failed or was never verified (ADR-0049 §7): its jobs wait until it is. One
 * not judged yet (`unknown`) refuses too, but is a row only once a job waits on it.
 */
function goldenRow(vm: VmView, gates: Budget['gates']): AttentionRow | null {
  if (!vm.golden_required) return null;
  const parked = gates?.golden_image?.parked_now ?? 0;
  const bad = (vm.golden || []).filter(
    (g) => g.standing === 'unverified' || g.standing?.startsWith('failed') || (parked > 0 && g.standing === 'unknown'),
  );
  if (!bad.length) return null;
  const more = bad.length > 1 ? ` (+${bad.length - 1} more)` : '';
  const parts: Part[] = [{ strong: `Golden image not verified: ${bad[0].image}` }, `${more} — ${waitsLine(parked)}`];
  if (bad[0].detail) parts.push(capitalize(bad[0].detail));
  return { key: 'golden', sev: parked ? 'bad' : 'warn', icon: 'alert', blocking: parked > 0, parts };
}

// ADR-0026: the wall and the VM runtime. A wall that has never been read is not a row.
function vmRows(vm: VmView | null | undefined, gates?: Budget['gates']): AttentionRow[] {
  const rows: AttentionRow[] = [];
  if (vm) {
    if (vm.wall === 'red') {
      rows.push({
        key: 'wall-red',
        sev: 'bad',
        icon: 'alert',
        blocking: true,
        parts: [{ strong: 'Wall red' }, ` — no new VM job starts. ${vm.wall_detail || ''} Running jobs finish.`],
      });
    } else if (vm.wall === 'out_of_date') {
      rows.push({
        key: 'wall-old',
        sev: 'bad',
        icon: 'alert',
        blocking: true,
        parts: [{ strong: 'Wall anchor out of date' }, ' — no new VM job starts. Run ', { mono: 'sudo faber setup' }, '.'],
      });
    }
    // Only the Linux image goes out of date; a golden image is verified or not (goldenRow).
    if (vm.images?.linux === 'out_of_date') {
      rows.push({
        key: 'vm-image-old',
        sev: 'warn',
        icon: 'alert',
        parts: [{ strong: 'Linux image out of date' }, ' — run ', { mono: 'sudo faber setup' }, '.'],
      });
    }
    for (const r of [macosRefusalRow(vm, gates), goldenRow(vm, gates)]) if (r) rows.push(r);
    if (vm.runner_image) {
      rows.push({ key: 'runner-image', sev: 'bad', icon: 'alert', blocking: true, parts: [{ strong: 'Runner image' }, ` — ${vm.runner_image}`] });
    }
    if (vm.macos_cap && (vm.macos_running ?? 0) >= vm.macos_cap) {
      rows.push({
        key: 'macos-cap',
        sev: 'info',
        icon: 'info',
        parts: [{ strong: `macOS VMs at the cap of ${vm.macos_cap}` }, ' — a third job waits.'],
      });
    }
  }
  return rows;
}

/** A mirror row's words: its head (with how many jobs an outage affected), then the daemon's prose. */
export function mirrorParts(r: MirrorRow): Part[] {
  const affected = r.affected?.length ? `, ${plural(r.affected.length, 'job')} affected` : '';
  const since = typeof r.since_ms === 'number' ? ` since ${fmtClock(r.since_ms)}` : '';
  const parts: Part[] = [{ strong: `${r.title}${affected}` }, `${since} — `];
  if (r.image) parts.push({ mono: r.image }, ': ');
  parts.push(r.detail);
  if (r.command) parts.push(' Build it with ', { mono: r.command }, '.');
  if (r.error) parts.push(' Last: ', { mono: r.error });
  return parts;
}

/** The daemon's own issues, in today's priority order; `sortRows` then orders by severity. */
export function daemonRows(s: DaemonState, now: number): AttentionRow[] {
  const b = s.budget || {};
  const rows: AttentionRow[] = [];
  const missing = s.missing_images || [];
  const retiring = [...(s.runner_retirements || [])].sort((a, c) => a.days_remaining - c.days_remaining);
  const urgent = retiring.filter((r) => r.urgent);
  const later = retiring.filter((r) => !r.urgent);

  if (missing.length) {
    const m = missing[0];
    const more = missing.length > 1 ? ` (+${missing.length - 1} more)` : '';
    rows.push({
      key: 'missing',
      sev: 'bad',
      icon: 'alert',
      blocking: true,
      parts: [
        { strong: `Profile image missing: ${m.profile}` },
        `${more} — `,
        { mono: m.image },
        ' is not on this host. No runner is minted for it until it is rebuilt or pulled.',
      ],
    });
  }
  // The pull-through mirror: down, image missing (bad) or passing through (warn). The daemon
  // words each row; the text stays text. Its image, command and error are set in mono, like the
  // profile image above, and the jobs an outage affected are counted: the dashboard names them.
  for (const r of s.mirror?.attention || []) {
    rows.push({ key: r.key, sev: r.severity, icon: 'alert', parts: mirrorParts(r) });
  }
  if (urgent.length) rows.push(retirementRow(urgent, 'bad'));
  if (s.healthy === false) {
    const parts: Part[] = [{ strong: 'Can’t reach GitHub' }, ' — retrying each cycle.'];
    if (s.last_error) parts.push(' Last: ', { mono: s.last_error });
    rows.push({ key: 'unhealthy', sev: 'bad', icon: 'alert', blocking: true, parts });
  }
  if (s.paused) {
    rows.push({ key: 'paused', sev: 'warn', icon: 'pause', blocking: true, parts: [{ strong: 'Paused' }, ' — nothing new starts. Running jobs finish.'] });
  }
  if (b.hold) {
    rows.push({
      key: 'hold',
      sev: 'warn',
      icon: 'pause',
      blocking: true,
      parts: [{ strong: 'Docker admission held' }, ` — ${b.hold.reason || ''} — since ${fmtClock(b.hold.since_ms)}`],
      actions: [{ label: 'Release', cmd: 'unhold' }],
    });
  }
  // ADR-0030: one row per held repo, each with its own Clear. The reason names a job an
  // outsider may have shaped, so it is text only.
  for (const rh of s.repo_holds || []) {
    const clear = `sudo faber repo-hold clear ${shellWord(rh.repo)}`;
    rows.push({
      key: `repo-hold:${rh.repo}`,
      sev: 'warn',
      icon: 'alert',
      blocking: true,
      parts: [
        { strong: `Minting held for ${rh.repo}` },
        ` — ${rh.reason || ''} — since ${fmtClock(rh.since_ms)}. Clearing needs sudo: `,
        { mono: clear },
      ],
      actions: [{ label: 'Clear', cmd: 'clear_repo_hold', args: { repo: rh.repo }, failHint: `Run: ${clear}` }],
    });
  }
  if (s.draining) {
    rows.push({ key: 'draining', sev: 'warn', icon: 'pause', blocking: true, parts: [{ strong: 'Draining' }, ' — finishing in-flight jobs, not claiming new.'] });
  }
  rows.push(...vmRows(b.vm, b.gates));
  const rr = reserveRow(b, now);
  if (rr) rows.push(rr);
  if (b.frozen) {
    rows.push({ key: 'frozen', sev: 'warn', icon: 'frozen', blocking: true, parts: [{ strong: 'Frozen' }, ' — holding admission under memory pressure.'] });
  }
  // ADR-0032: jobs the disk floor has docker-paused. Their pips go dim; this row says why.
  const diskFrozen = (s.inflight || []).filter((j) => j.frozen && j.frozen.reason !== 'memory').length;
  if (diskFrozen) {
    rows.push({
      key: 'frozen-disk',
      sev: 'warn',
      icon: 'frozen',
      blocking: true,
      parts: [{ strong: 'Frozen' }, ` — ${plural(diskFrozen, 'job')} paused until the Docker volume has room.`],
    });
  }
  if (b.machine?.hold) {
    // ADR-0021 § 2c: the machine's own memory, not the VM's.
    const why = b.machine.hold === 'swap' ? 'swap is growing' : `memory pressure is ${b.machine.pressure || 'high'}`;
    rows.push({
      key: 'machine',
      sev: 'warn',
      icon: 'frozen',
      blocking: true,
      parts: [{ strong: 'Machine under pressure' }, ` — ${why}; nothing new starts until it eases.`],
    });
  }
  if (b.machine?.encroaches?.length) {
    // ADR-0021 § 2a: the Docker VM is larger than the machine's share leaves CI.
    rows.push({ key: 'vm-large', sev: 'warn', icon: 'alert', parts: [{ strong: 'Docker VM too large' }, ` — ${b.machine.encroaches[0]}`] });
  }
  const oom = oomHeldRow(b.oom_held || []);
  if (oom) rows.push(oom);
  const bypasses = s.trust_bypasses_7d || 0;
  if (bypasses > 0 && s.gate?.on_bypass !== 'enforce') {
    rows.push({
      key: 'bypass',
      sev: 'warn',
      icon: 'alert',
      parts: [{ strong: `${plural(bypasses, 'trust bypass', 'trust bypasses')} in 7 days` }, ' — enforcement off.'],
    });
  }
  const rq = s.refused_queued || [];
  if (rq.length) {
    const n = rq.length + (s.refused_queued_overflow || 0);
    // A fork PR or an untrusted actor's job controls its workflow name, so the row shows only
    // the repo for those.
    const verdict = rq[0].verdict || '';
    const outsider = verdict === 'refuse:fork' || verdict.startsWith('refuse:actor');
    const first: Part[] = [rq[0].repo || ''];
    if (!outsider && rq[0].workflow) first.push({ sep: true }, rq[0].workflow);
    rows.push({
      key: 'refused',
      sev: 'warn',
      icon: 'alert',
      parts: [
        { strong: `${plural(n, 'job')} refused here` },
        ' — no runner of its own; it waits, or takes one minted for another job. First: ',
        ...first,
      ],
    });
  }
  if (later.length) rows.push(retirementRow(later, 'warn'));
  return rows;
}

/** The daemon's update notices that need the operator or report trouble (ADR-0006, ADR-0031). */
export function updateRows(u: UpdateView | undefined): AttentionRow[] {
  const rows: AttentionRow[] = [];
  if (!u) return rows;
  const phase = u.phase?.phase || 'idle';
  const v = u.phase?.version || u.available;
  if (u.rolled_back_from) {
    rows.push({ key: 'upd-rb', sev: 'bad', icon: 'alert', ident: u.rolled_back_from ?? undefined, parts: [{ strong: 'Rolled back' }, ` from ${u.rolled_back_from}`] });
  }
  const sc = u.self_check;
  if (sc && (sc.status === 'unproven' || sc.status === 'refused')) {
    const since = sc.at_ms ? fmtDateTime(sc.at_ms) : 'boot';
    rows.push({
      key: 'upd-sc',
      sev: 'bad',
      icon: 'alert',
      parts:
        sc.status === 'unproven'
          ? [{ strong: 'Updater unproven' }, ` since ${since}: ${sc.detail || 'unknown'}`]
          : [{ strong: `Self-check refused ${sc.version ?? ''}` }, ` — rolling back: ${sc.detail || 'unknown'}`],
    });
  }
  const sr = u.supervisor_restart;
  if (sr) {
    rows.push({
      key: 'upd-sup',
      sev: 'warn',
      icon: 'alert',
      parts: [
        { strong: `VM supervisor restart ${String(sr.status ?? 'state unknown').replace(/_/g, ' ')}` },
        ` — ${sr.detail ?? ''} (running VMs unaffected)`,
      ],
    });
  }
  if (phase === 'waiting_idle') {
    rows.push({
      key: 'upd-staged',
      sev: 'warn',
      icon: 'update',
      ident: v ?? undefined,
      parts: [{ strong: 'Update staged' }, ` — faber ${v} installs when idle.`],
      actions: [
        { label: 'Install now', primary: true, cmd: 'apply_update' },
        { label: 'Skip', cmd: 'skip_update' },
      ],
    });
  } else if (u.available && u.install_in_secs != null && phase !== 'draining') {
    // Maturity wait (ADR-0031): the alert the GitHub account cannot silence.
    rows.push({
      key: 'upd-wait',
      sev: 'warn',
      icon: 'update',
      // The countdown moves every poll; the version is the occurrence.
      ident: u.available,
      parts: [{ strong: 'Update waiting' }, ` — faber ${u.available} installs in ${waitIn(u.install_in_secs)}.`],
      actions: [{ label: 'Skip', cmd: 'skip_update' }],
    });
  }
  return rows;
}

/** The app's own update (ADR-0034): it installs itself, so these only report. */
export function appUpdateRows(a: AppUpdate | null | undefined): AttentionRow[] {
  const rows: AttentionRow[] = [];
  if (!a) return rows;
  if (a.updated_to) rows.push({ key: 'app-updated', sev: 'info', icon: 'info', ident: a.updated_to, parts: [{ strong: `Updated to ${a.updated_to}` }] });
  if (a.rolled_back) {
    rows.push({ key: 'app-rb', sev: 'bad', icon: 'alert', ident: a.rolled_back, parts: [{ strong: 'App update rolled back' }, ` — ${a.rolled_back}`] });
  }
  if (a.status === 'held' && a.detail) {
    rows.push({ key: 'app-held', sev: 'warn', icon: 'info', ...(a.version ? { ident: a.version } : {}), parts: [{ strong: `App ${a.version || 'update'} held` }, ` — ${a.detail}`] });
  }
  if (a.floor_warning) {
    const d = a.daemon_maturity_secs ?? 0;
    rows.push({
      key: 'app-floor',
      sev: 'warn',
      icon: 'alert',
      parts: [{ strong: 'Update waits differ' }, ` — ${a.floor_warning}`],
      actions: d >= 3600 ? [{ label: 'Match daemon', cmd: 'set_update_floor', args: { secs: d } }] : [],
    });
  }
  return rows;
}

const VIA: Record<string, string> = { socket: 'the control socket', session: 'a dashboard session' };

/**
 * ADR-0028: every allowed trust widening of the last 7 days. None is dismissed; each is
 * acknowledged instead (ADR-0064 § 4, `foldTrust`). `shrank` is the app's own check that its copy
 * of the list lost an entry.
 */
export function trustRows(s: DaemonState, shrank: boolean | undefined, now: number): AttentionRow[] {
  const rows: AttentionRow[] = [];
  if (shrank) {
    rows.push({ key: 'trust-shrank', sev: 'bad', icon: 'alert', blocking: true, parts: [{ strong: 'faber’s record of trust changes shrank' }] });
  }
  for (const c of s.trust_changes || []) {
    rows.push({
      key: `trust-${c.ts_ms}-${c.key}`,
      sev: 'warn',
      icon: 'alert',
      ts_ms: c.ts_ms,
      parts: [{ strong: 'Trust widened:' }, ` ${c.sentence}, from ${VIA[c.via] ?? c.via} ${fmtAgo((now - c.ts_ms) / 1000)} ago`],
    });
  }
  return rows;
}

/** The text colour of a severity's icon: the row's mark, and the fold's tally (guidelines.md, Colour). */
export const MARK: Record<Severity, string> = { bad: 'text-red', warn: 'text-amber', info: 'text-muted' };

const SEV_RANK: Record<Severity, number> = { bad: 0, warn: 1, info: 2 };

/** Worst first, stable within a severity. */
export const sortRows = (rows: AttentionRow[]) =>
  rows
    .map((r, i) => ({ r, i }))
    .sort((a, c) => SEV_RANK[a.r.sev] - SEV_RANK[c.r.sev] || a.i - c.i)
    .map((x) => x.r);

/** How many of `rows` have each severity, for the severities present: bad, warn, info. */
export const severityTally = (rows: Pick<AttentionRow, 'sev'>[]): { sev: Severity; count: number }[] =>
  (['bad', 'warn', 'info'] as const)
    .map((sev) => ({ sev, count: rows.filter((r) => r.sev === sev).length }))
    .filter((t) => t.count > 0);

/** The rows to show and how many fold into "+N more": a lone extra row shows in place of "+1 more". */
export function foldRows(rows: AttentionRow[]): { shown: AttentionRow[]; hidden: number } {
  const shown = rows.length > ATTENTION_MAX + 1 ? rows.slice(0, ATTENTION_MAX) : rows;
  return { shown, hidden: rows.length - shown.length };
}

/** The row's words as plain text: its hover title, and its identity before digits are masked. */
export const plainText = (parts: Part[]) =>
  parts.map((p) => (typeof p === 'string' ? p : 'strong' in p ? p.strong : 'mono' in p ? p.mono : ' | ')).join('');

/**
 * What a dismissal of `row` remembers (ADR-0064 § 2): its declared identity, else its words with
 * every run of digits masked, so figures that move each run ("wants 6 cores") keep it hidden
 * and a different situation under the same key shows it again.
 */
export const rowIdent = (row: AttentionRow) => row.ident ?? plainText(row.parts).replace(/\d+/g, '#');

/** A trust row is acknowledged, never dismissed (§ 4); `trust-shrank` is blocking besides. */
const isTrust = (row: AttentionRow) => row.key.startsWith('trust-');

/** Whether the row carries Dismiss: not blocking (§ 1) and not about trust (§ 4). */
export const canDismiss = (row: AttentionRow) => !row.blocking && !isTrust(row);

/**
 * The rows still shown, and the active rows the operator dismissed, each in input order. A row is
 * dismissed while a dismissal names its key with its current identity; a blocking row never is.
 */
export function splitDismissed(rows: AttentionRow[], attention: AttentionState | null | undefined): { active: AttentionRow[]; dismissed: AttentionRow[] } {
  const by = new Map((attention?.dismissed || []).map((d) => [d.key, d.ident]));
  const active: AttentionRow[] = [];
  const dismissed: AttentionRow[] = [];
  for (const r of rows) (canDismiss(r) && by.has(r.key) && by.get(r.key) === rowIdent(r) ? dismissed : active).push(r);
  return { active, dismissed };
}

/** The key of the one row acknowledged trust changes fold into. */
export const TRUST_ACKED_KEY = 'trust-acked';

/**
 * ADR-0064 § 4: trust changes at or before `ackedMs` fold into one quiet row that opens to list
 * them, in place of the first; the rest stay full rows with Acknowledge, which records the newest
 * change's time. Other rows pass through.
 */
export function foldTrust(rows: AttentionRow[], ackedMs: number | null | undefined): AttentionRow[] {
  const changes = rows.filter((r) => isTrust(r) && typeof r.ts_ms === 'number');
  if (!changes.length) return rows;
  const newest = Math.max(...changes.map((r) => r.ts_ms!));
  const acked = (r: AttentionRow) => typeof ackedMs === 'number' && r.ts_ms! <= ackedMs;
  const folded = changes.filter(acked);
  const out: AttentionRow[] = [];
  for (const r of rows) {
    if (!changes.includes(r)) out.push(r);
    else if (!acked(r)) out.push({ ...r, actions: [...(r.actions || []), { label: 'Acknowledge', cmd: 'ack_trust', args: { untilMs: newest } }] });
    else if (r === folded[0]) {
      out.push({
        key: TRUST_ACKED_KEY,
        sev: 'info',
        icon: 'info',
        parts: [{ strong: `${plural(folded.length, 'trust change')} this week, acknowledged` }],
        details: { label: 'changes', lines: folded.map((c) => c.parts) },
      });
    }
  }
  return out;
}

/**
 * Every surface's view of its rows (ADR-0064): trust changes folded by the acknowledgement, then
 * split into the rows shown, worst first, and the dismissed ones still active.
 */
export function applyAttention(rows: AttentionRow[], attention: AttentionState | null | undefined): { active: AttentionRow[]; dismissed: AttentionRow[] } {
  const { active, dismissed } = splitDismissed(foldTrust(rows, attention?.trust_acked_ms), attention);
  return { active: sortRows(active), dismissed: sortRows(dismissed) };
}

/** The label Dismiss runs under while it is in flight: a row's `pending` names it like an action's. */
export const DISMISS = 'Dismiss';

/** What a dismissed list's `pending` holds while Restore all runs: no row key is empty. */
export const RESTORE_ALL = '';

/** The commands Dismiss, Restore and Restore all run, as `RowAction`s (Tauri argument names). */
export const dismissAction = (row: AttentionRow): RowAction => ({ label: DISMISS, cmd: 'dismiss_attention', args: { key: row.key, ident: rowIdent(row) } });
export const restoreAction = (key?: string): RowAction => ({ label: key ? 'Restore' : 'Restore all', cmd: 'restore_attention', args: key ? { key } : {} });
