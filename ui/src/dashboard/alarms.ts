// Everything on the daemon that needs attention, worst first: the dashboard's attention list.
// It composes the popover's producers (popover/attention.ts, untouched) and adds the notices the
// inline page showed that the popover folds away or lacks: the credential, the gate, the dind
// wall and label rule, every missing image, the mirrors, the update's other phases. Rows are
// data; daemon and GitHub text stays in string parts, which React sets as text, never as markup.
import { applyAttention, daemonRows, mirrorParts, retireIdent, rowIdent, sortRows, trustRows, updateRows } from '../popover/attention';
import type { AttentionRow, Part, Severity } from '../popover/attention';
import { fmtClock, fmtDateTime, plural } from '../popover/format';
import type { DaemonState } from '../popover/types';
import { phaseWords } from './maintenance';

/** An alarm is a popover attention row: `sev` is its tone, `actions` name what the Release/Clear/Install buttons run. */
export type Alarm = AttentionRow;

/**
 * Action commands, as `RowAction.cmd` names them. The first four are the popover's own; `check_update`
 * is the dashboard's "Check now", and `set_pin` is the OOM-held row's Pin (asked first: its `confirm`).
 * The last three are ADR-0064's Dismiss, Restore and Acknowledge. The screen maps each to dashboard/api.ts.
 */
export const ALARM_CMDS = [
  'unhold',
  'clear_repo_hold',
  'apply_update',
  'skip_update',
  'check_update',
  'set_pin',
  'dismiss_attention',
  'restore_attention',
  'ack_trust',
] as const;

const alarm = (key: string, sev: Severity, parts: Part[], actions?: AttentionRow['actions']): Alarm => ({
  key,
  sev,
  icon: sev === 'info' ? 'info' : 'alert',
  parts,
  ...(actions ? { actions } : {}),
});

/** The alarm says jobs cannot start until something changes (ADR-0064 § 1): no Dismiss. */
const blocking = (a: Alarm): Alarm => ({ ...a, blocking: true });

/** Mono values joined by quiet pipes: `a | b | c`. */
const monos = (values: string[]): Part[] => values.flatMap((v, i): Part[] => (i ? [{ sep: true }, { mono: v }] : [{ mono: v }]));

function credentialAlarms(s: DaemonState): Alarm[] {
  const c = s.credential;
  if (!c) return [];
  const out: Alarm[] = [];
  if (c.status === 'missing' || c.status === 'invalid') {
    out.push(blocking(alarm(`credential-${c.status}`, 'bad', [{ strong: `GitHub token ${c.status}` }, ` — nothing new is claimed: ${c.reason || ''}`])));
  } else if (c.status === 'expiring') {
    out.push(alarm('credential-expiring', 'warn', [{ strong: 'GitHub token expiring' }, ` — ${c.reason || 'The GitHub token expires soon.'}`]));
  }
  if (c.file_warning) out.push(alarm('credential-file', 'warn', [{ strong: 'GitHub token file' }, ` — ${c.file_warning}`]));
  for (const m of c.missing_permissions || []) {
    out.push(
      alarm(`credential-perm:${m.repo}:${m.permission}`, 'warn', [
        { strong: m.repo },
        ' — the token lacks ',
        { mono: m.permission },
        `, so ${m.feature} is off there: ${m.fix}`,
      ]),
    );
  }
  const extra = c.mismatches?.granted_not_opted_in || [];
  if (extra.length) {
    out.push(
      alarm('credential-granted-not-opted-in', 'warn', [
        { strong: 'Token reaches repositories that are not opted in' },
        ' — remove them from it on GitHub: ',
        ...monos(extra),
      ]),
    );
  }
  const unreachable = c.mismatches?.opted_in_not_granted || [];
  if (unreachable.length) {
    out.push(
      blocking(
        alarm('credential-opted-in-not-granted', 'warn', [
          { strong: 'Opted in, but the token cannot administer them' },
          ' — none of their jobs run here: ',
          ...monos(unreachable),
        ]),
      ),
    );
  }
  // Information only while nothing else is wrong with the token.
  if ((c.unverified_permissions || []).length && !out.length && c.owner) {
    const expires = c.expires_at_ms ? `, expires ${new Date(c.expires_at_ms).toISOString().slice(0, 10)}` : '';
    out.push(
      alarm('credential-unverified', 'info', [
        { strong: `GitHub token for ${c.owner}` },
        `${expires}. Not yet proved by a write: ${c.unverified_permissions!.join(', ')}.`,
      ]),
    );
  }
  return out;
}

function gateAlarms(s: DaemonState): Alarm[] {
  if (!s.gate?.allow_public_repos) return [];
  return [alarm('gate-public', 'warn', [{ strong: 'Public repos allowed' }, ' — the one gate setting that widens what this machine runs.'])];
}

function noticeAlarms(s: DaemonState): Alarm[] {
  const seen = new Map<string, number>();
  return (s.notices || []).map((n) => {
    const nth = seen.get(n.kind) ?? 0;
    seen.set(n.kind, nth + 1);
    return alarm(nth ? `notice:${n.kind}:${nth}` : `notice:${n.kind}`, 'warn', [n.text]);
  });
}

// ADR-0025 § "On Linux, dind waits for a wall"; ADR-0026 § Consequences for the label rule.
function dindAlarms(s: DaemonState): Alarm[] {
  const out: Alarm[] = [];
  const walled = (s.profiles || []).filter((p) => p.needs_wall).map((p) => p.name);
  if (walled.length) {
    out.push(
      alarm('dind-wall', 'warn', [
        { strong: 'Docker access has no account wall' },
        ' for ',
        ...monos(walled),
        ' — a granted (or unstated) dind is root on this whole machine. Set ',
        { mono: 'dind = false' },
        ', or run jobs behind a wall once ADR-0026 ships. Only a warning today; a later, release-noted version will refuse it.',
      ]),
    );
  }
  const rule = s.dind_label_rule;
  if (rule != null && rule !== 'on') {
    out.push(
      alarm('dind-rule', 'warn', [
        { strong: `dind label rule ${rule}` },
        ' — every job on a profile that grants dind gets a Docker daemon, whether or not its runs-on includes ',
        { mono: 'dind' },
        '. Set ',
        { mono: '[host] dind_requires_label = true' },
        ' to require the label.',
      ]),
    );
  }
  return out;
}

function imageAlarms(s: DaemonState): Alarm[] {
  const out: Alarm[] = [];
  for (const m of s.missing_images || []) {
    const since = m.since_ms ? ` since ${fmtDateTime(m.since_ms)}` : '';
    const parts: Part[] = [{ strong: `Profile image missing: ${m.profile}` }, ' (', { mono: m.image }, `)${since} — no runner is minted for it until it is rebuilt or pulled.`];
    if (m.cause) parts.push(' Pull: ', { mono: m.cause });
    out.push(blocking(alarm(`missing:${m.profile}:${m.image}`, 'bad', parts)));
  }
  // #156: a warning, not a fault.
  for (const u of s.unqualified_images || []) {
    out.push(alarm(`unqualified:${u.profile}`, 'warn', [{ strong: `Unqualified image: ${u.profile}` }, ` — ${u.message}`]));
  }
  return out;
}

// A healthy mirror is data for the Memory view, not an alarm: only a blocked one or a failed refresh is.
function gitMirrorAlarms(s: DaemonState): Alarm[] {
  const out: Alarm[] = [];
  for (const m of s.git_mirrors || []) {
    if (m.blocked) {
      out.push(blocking(alarm(`git-mirror:${m.repo}`, 'bad', [{ strong: `Git mirror ${m.repo}` }, ` ${m.blocked}.`])));
    } else if (m.last_error) {
      out.push(alarm(`git-mirror:${m.repo}`, 'warn', [{ strong: `Git mirror ${m.repo}` }, ' — last refresh failed: ', { mono: m.last_error }]));
    }
  }
  return out;
}

// #64: from three days out the daemon marks it urgent; jobs run until the cutoff either way.
function retirementAlarms(s: DaemonState): Alarm[] {
  return [...(s.runner_retirements || [])]
    .sort((a, b) => a.days_remaining - b.days_remaining)
    .map((r) => {
      const a: Alarm = {
        ...alarm(`retire:${r.profile}`, r.urgent ? 'bad' : 'warn', [
          { strong: `Runner retiring: ${r.profile}` },
          ` — ${r.message || `actions/runner ${r.runner_version ?? ''} stops on ${r.cutoff ?? ''}`}`,
        ]),
        ident: retireIdent(r),
      };
      // Past its cutoff the runner takes no job; before it, it does.
      return r.days_remaining < 0 ? blocking(a) : a;
    });
}

function encroachAlarms(s: DaemonState): Alarm[] {
  // The first is keyed and worded as the popover's one row, so a dismissal holds on both.
  return (s.budget?.machine?.encroaches || []).map((line, i) =>
    alarm(i ? `vm-large:${i}` : 'vm-large', 'warn', [{ strong: 'Docker VM too large' }, ` — ${line}`]),
  );
}

/**
 * The warnings the autolimiter raises (an escalation waiting for a pin, say), keyed by the class
 * they name (`warning:memory:<class>`), so a dismissal follows the warning and not its place in the
 * list. An older daemon sends no keys: those fall back to the index. Memory's cards are these too.
 */
export function warningAlarms(s: DaemonState): Alarm[] {
  const items = s.budget?.warning_items;
  if (items?.length) return items.map((w) => alarm(`warning:${w.key}`, 'warn', [w.text]));
  return (s.budget?.warnings || []).map((w, i) => alarm(`warning:${i}`, 'warn', [w]));
}

function peerAlarms(s: DaemonState): Alarm[] {
  const peers = s.peers || [];
  if (!peers.length) return [];
  const names: Part[] = peers.flatMap((p, i): Part[] => {
    const n = Object.values(p.runners || {}).reduce((x, y) => x + y, 0);
    return [...(i ? [{ sep: true } as const] : []), { mono: p.instance }, ` (${plural(n, 'runner')} now)`];
  });
  return [
    alarm('peers', 'info', [
      { strong: 'Peer faber instances share this pool' },
      ` — this one is ${s.instance || '?'}; peers: `,
      ...names,
      '. Their lost runners are logged as peer_runner_lost.',
    ]),
  ];
}

function awakeAlarms(s: DaemonState): Alarm[] {
  const a = s.awake;
  if (!a?.enabled) return [];
  if (!a.supported) {
    return [alarm('awake-unsupported', 'info', [{ strong: 'prevent_sleep does nothing here' }, ' — macOS only.'])];
  }
  if (a.error && !a.held) return [alarm('awake-error', 'warn', [{ strong: 'Could not hold off idle sleep' }, ` — ${a.error}`])];
  return [];
}

/** The mirror's attention rows name the jobs an outage affected; the popover only counts them. */
function mirrorAlarms(s: DaemonState): Alarm[] {
  return (s.mirror?.attention || []).map((r) => {
    const parts = mirrorParts(r);
    // Identified by the popover's words, without the jobs, so a dismissal holds on both.
    const ident = rowIdent({ key: r.key, sev: 'warn', icon: 'alert', parts: mirrorParts(r) });
    if (r.affected?.length) parts.push(' Jobs affected: ', ...monos(r.affected), '.');
    return { ...alarm(r.key, r.severity === 'bad' ? 'bad' : 'warn', parts), ident };
  });
}

// The updater's phases beyond what the popover reports (it shows the staged and maturity rows).
function updatePhaseAlarms(s: DaemonState): Alarm[] {
  const u = s.update;
  if (!u) return [];
  const phase = u.phase?.phase || 'idle';
  const out: Alarm[] = [];
  // The phases worth an alarm, worded once in phaseWords (Maintenance's Updates panel says the same).
  const sev: Record<string, Severity> = { draining: 'warn', downloading: 'info', deferred: 'warn', failed: 'bad' };
  const words = phaseWords(u);
  if (sev[phase] && words) {
    const next = phase === 'failed' && u.next_attempt_ms ? `. Next attempt ${fmtClock(u.next_attempt_ms)}.` : '';
    out.push(alarm(`upd-${phase}`, sev[phase], [{ strong: words.word }, words.note && !words.fallbackNote ? ` — ${words.note}${next}` : next || '…']));
  } else if (phase !== 'waiting_idle' && u.available && u.install_in_secs == null) {
    const why = u.candidate_reason ? ` ${u.candidate_reason}` : '';
    out.push({ ...alarm('upd-available', 'info', [{ strong: `${u.available} available.` }, why], [{ label: 'Check now', cmd: 'check_update' }]), ident: u.available });
  }
  if (u.pin) out.push({ ...alarm('upd-pin', 'info', [{ strong: 'Update pinned' }, ` to ${u.pin}`]), ident: u.pin });
  return out;
}

/**
 * Everything that needs attention, worst first (stable within a tone), dismissed rows included.
 * `now` is the clock the trust-change ages read against. A view shows `activeAlarms`.
 */
export function dashboardAlarms(s: DaemonState, now: number): Alarm[] {
  // What the dashboard words itself, so the popover's one-line versions step aside.
  const own = new Set(['missing', 'vm-large', ...(s.mirror?.attention || []).map((r) => r.key)]);
  const wallNote = s.budget?.vm?.wall_required;
  const popover = daemonRows(s, now)
    .filter((r) => !own.has(r.key) && !r.key.startsWith('retire:'))
    .map((r): AttentionRow => {
      // A required wall also stops Docker jobs; the popover's row says only VM jobs.
      if (r.key === 'wall-red' && wallNote) {
        return { ...r, parts: [{ strong: 'Wall red' }, ` — no new VM job starts and no Docker job. ${s.budget?.vm?.wall_detail || ''} Running jobs finish.`] };
      }
      return r;
    });
  // The maturity row is the chain's last resort on the inline page: a download, a deferral or a failure outranks it.
  const phase = s.update?.phase?.phase;
  const updates = updateRows(s.update).filter((r) => !(r.key === 'upd-wait' && (phase === 'downloading' || phase === 'deferred' || phase === 'failed')));
  return sortRows([
    ...credentialAlarms(s),
    ...popover,
    ...updates,
    ...updatePhaseAlarms(s),
    ...imageAlarms(s),
    ...mirrorAlarms(s),
    ...retirementAlarms(s),
    ...gitMirrorAlarms(s),
    ...dindAlarms(s),
    ...gateAlarms(s),
    ...encroachAlarms(s),
    ...warningAlarms(s),
    ...noticeAlarms(s),
    ...awakeAlarms(s),
    ...peerAlarms(s),
    ...trustRows(s, undefined, now),
  ]);
}

/**
 * The alarms a view shows and the dismissed ones still active (ADR-0064): acknowledged trust
 * changes folded into one row, then split by the daemon's dismissals.
 */
export const activeAlarms = (s: DaemonState, now: number) => applyAttention(dashboardAlarms(s, now), s.attention);
