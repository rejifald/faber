// Canned snapshots for the stories, one per reference artboard in docs/design/popover/reference/.
// Names are owner/demo style only (AGENTS.md: nothing private in the tree).
import { daemonRows, rowIdent } from './attention';
import { DAEMON_LABEL_SEP } from './groups';
import type { Budget, DaemonState, Inflight, Outcome, Snapshot, VmEntry } from './types';

/** The moment every fixture is read at. */
export const NOW = Date.UTC(2026, 9, 18, 12, 0, 0);
const ago = (s: number) => NOW - s * 1000;

const BASE_URL = 'http://127.0.0.1:8787';

/** Six slots, 8 GiB of memory, CPU in auto, a disk floor: the four rings of every artboard. */
export const budget = (over: Partial<Budget> = {}): Budget => ({
  hard_ceiling: 6,
  docker_inflight: 0,
  budget_mib: 8192,
  reserved_mib: 0,
  headroom: { jobs: 2, binds: 'memory', job_mib: 3072, job_mcpu: 2000 },
  cpu: { mode: 'auto', budget_mcpu: 8000, reserved_mcpu: 200 },
  disk: { mode: 'on', free_mib: 180 * 1024, held_mib: 10 * 1024, floor_gib: 50 },
  gates: {},
  ...over,
});

let jobId = 1000;
/** A live job: running unless `admitted: false`. */
export const job = (over: Partial<Inflight> = {}): Inflight => ({
  job_id: jobId++,
  profile: 'linux',
  repo: 'owner/demo',
  pr: 2741,
  branch: 'feature/cache',
  workflow: 'CI',
  job_name: 'api e2e',
  event: 'pull_request',
  elapsed_s: 120,
  admitted: true,
  step: { name: 'Run tests', index: 3, total: 9 },
  waiting: null,
  frozen: null,
  ...over,
});

const waits = (over: Partial<Inflight> = {}) =>
  job({ admitted: false, step: null, waiting: { gate: 'memory', detail: 'needs 3072 MiB, 1024 MiB free' }, ...over });

/** A runner that failed on faber's side (the red ring), with the identity the daemon sends (ADR-0044). */
export const fault = (over: Partial<Outcome> = {}): Outcome => ({
  ts_ms: ago(60),
  repo: 'owner/demo',
  job_id: jobId++,
  profile: 'linux',
  label: ['owner/demo', 'CI', 'web unit'].join(DAEMON_LABEL_SEP),
  result: 'infra_error',
  attempt: 1,
  pr: 2738,
  workflow: 'CI',
  job_name: 'web unit',
  conclusion: null,
  ...over,
});

/**
 * A run whose runner did its part and whose CI GitHub concluded `conclusion` (ADR-0044), read
 * as the run ended unless `conclusion_ts_ms` says later.
 */
export const verdict = (conclusion: string, over: Partial<Outcome> = {}): Outcome => {
  const o = fault({ result: 'success', conclusion, ...over });
  return { conclusion_ts_ms: o.ts_ms, ...o, ...over };
};

const online = (state: DaemonState, over: Partial<Snapshot> = {}): Snapshot => ({
  online: true,
  base_url: BASE_URL,
  app_version: '0.3.13',
  state: { healthy: true, paused: false, update: { current: '0.3.13' }, ...state },
  ...over,
});

const peer = (seenAgo: number) => ({
  instance: 'c3d4',
  last_seen_ms: ago(seenAgo),
  runners: { 'owner/demo': 2 },
  busy: { 'owner/demo': 1 },
  idle: { 'owner/demo': 1 },
});

export const idle = online({ budget: budget({ cpu: { mode: 'auto', budget_mcpu: 8000, reserved_mcpu: 200 } }) });

const started = (s: number) => Math.floor(ago(s) / 1000);
/** The Linux engine VM, up 2 h with 6 vCPU and 16 GiB; its disk is `budget.disk.vm_free_mib`. */
const linuxVm = (over: Partial<VmEntry> = {}): VmEntry => ({
  kind: 'linux',
  slot: 0,
  state: 'in_service',
  started_unix: started(2 * 3600),
  cpus: 6,
  memory_mib: 16 * 1024,
  job: null,
  ...over,
});
const macosVm = (over: Partial<VmEntry> = {}): VmEntry => ({
  kind: 'macos',
  slot: 0,
  state: 'in_service',
  started_unix: started(12 * 60),
  cpus: 4,
  memory_mib: 8 * 1024,
  job: { repo: 'owner/demo', job_name: 'release' },
  ...over,
});

/** Idle, with the Linux engine VM up: a running VM holds its memory, so the section shows (vms/anatomy.md § 2). */
export const idleVm = online({
  budget: budget({
    cpu: { mode: 'auto', budget_mcpu: 8000, reserved_mcpu: 200 },
    disk: { mode: 'on', free_mib: 180 * 1024, held_mib: 10 * 1024, floor_gib: 50, vm_free_mib: 45 * 1024 },
    vm: { listed_age_s: 4, vms: [linuxVm()] },
  }),
});

/** The supervisor cannot be reached: the VM list is 3 minutes old, so its rows are greyed. */
export const vmsStale = online({
  budget: budget({
    vm: { listed_age_s: 180, vms: [linuxVm(), macosVm({ job: null })] },
  }),
  update: { current: '0.3.13', supervisor_restart: { status: 'unreachable', detail: 'the supervisor does not answer' } },
});

export const idlePeerStale = online({ budget: budget(), peers: [peer(7 * 60)] });

export const running = online({
  budget: budget({
    docker_inflight: 3,
    reserved_mib: 5632,
    headroom: { jobs: 1, binds: 'memory', job_mib: 3072, job_mcpu: 2000 },
    disk: { mode: 'on', free_mib: 180 * 1024, held_mib: 10 * 1024, floor_gib: 50, vm_free_mib: 40 * 1024 },
    vm: { listed_age_s: 6, vms: [linuxVm(), macosVm(), macosVm({ slot: 1, state: 'booting', started_unix: started(40), job: null })] },
  }),
  inflight: [
    job({ elapsed_s: 412, step: { name: 'Run e2e', index: 7, total: 12 } }),
    job({ job_name: 'web unit', elapsed_s: 188, step: { name: 'Install', index: 5, total: 9 } }),
    job({ pr: 2738, job_name: 'lint', elapsed_s: 64, step: { name: 'Lint', index: 2, total: 4 } }),
  ],
});

export const waiting = online({
  budget: budget({ docker_inflight: 2, reserved_mib: 5632, gates: { memory: { parked_now: 3 } } }),
  inflight: [
    job({ elapsed_s: 300 }),
    job({ job_name: 'web unit', elapsed_s: 280 }),
    waits({ pr: null, repo: 'owner/site', workflow: 'Deploy', job_name: 'deploy', event: 'push', elapsed_s: 22 }),
    waits({ pr: null, repo: 'owner/site', workflow: 'Deploy', job_name: 'smoke', event: 'push', elapsed_s: 20 }),
    waits({ pr: 2750, job_name: 'build', elapsed_s: 15 }),
  ],
});

const busyState = (): DaemonState => ({
  budget: budget({
    docker_inflight: 3,
    reserved_mib: 5632,
    cpu: { mode: 'auto', budget_mcpu: 8000, reserved_mcpu: 3000 },
    gates: { memory: { parked_now: 2 } },
  }),
  update: { current: '0.3.12', phase: { phase: 'waiting_idle', version: '0.3.13' } },
  inflight: [
    job({ elapsed_s: 412, step: { name: 'Run e2e', index: 7, total: 12 } }),
    job({ job_name: 'web unit', elapsed_s: 188, step: { name: 'Install', index: 5, total: 9 } }),
    job({ job_name: 'integration', elapsed_s: 150, step: { name: 'Run', index: 4, total: 6 } }),
    waits({ job_name: 'visual', elapsed_s: 40 }),
    job({ pr: 2738, job_name: 'lint', elapsed_s: 64, step: { name: 'Lint', index: 2, total: 4 } }),
    waits({ pr: null, repo: 'owner/site', workflow: 'Deploy', job_name: 'deploy', event: 'push', elapsed_s: 22 }),
  ],
  outcomes: [fault({ ts_ms: ago(60) })],
  peers: [peer(40)],
});

export const busy = online(busyState(), { app_version: '0.3.12' });

export const busyOverflow = online(
  {
    ...busyState(),
    update: { current: '0.3.13' },
    inflight: [
      ...Array.from({ length: 5 }, (_, i) => job({ pr: 2750, job_name: `shard ${i + 1}`, elapsed_s: 130 - i })),
      ...Array.from({ length: 4 }, (_, i) => waits({ pr: 2750, job_name: `shard ${i + 6}`, elapsed_s: 20 })),
      ...(busyState().inflight ?? []),
    ],
    outcomes: [fault({ ts_ms: ago(60) })],
  },
  {},
);

export const pausedFrozen = online({
  paused: true,
  budget: budget({ docker_inflight: 3, reserved_mib: 5632, cpu: { mode: 'auto', budget_mcpu: 8000, reserved_mcpu: 3000 } }),
  inflight: [
    job({ elapsed_s: 560, frozen: { since_ms: ago(90), reason: 'disk', detail: 'free space under the floor' } }),
    job({ job_name: 'web unit', elapsed_s: 540, frozen: { since_ms: ago(90), reason: 'disk', detail: 'free space under the floor' } }),
  ],
});

/** Every kind of attention row the daemon can raise, more than fit: three, then "+N more". */
export const attention = online({
  healthy: false,
  last_error: 'GET /repos/owner/demo/actions/runs: 502',
  budget: budget({ hold: { reason: 'disk cleanup', since_ms: ago(1800) } }),
  repo_holds: [{ repo: 'owner/demo', reason: 'a fork job asked for dind', since_ms: ago(600) }],
  draining: true,
  missing_images: [{ profile: 'gpu', image: 'ghcr.io/owner/my-runner:gpu' }],
});

/** The mirror down for ten minutes, with three jobs it affected and the probe's last error. */
export const mirrorDown = online({
  mirror: {
    state: 'down',
    attention: [
      {
        key: 'mirror-down',
        severity: 'bad',
        title: 'Mirror down',
        detail: 'No healthy answer from the mirror; new jobs run without it.',
        since_ms: ago(600),
        error: 'health probe: exited 1',
        affected: ['faber-1001-1792581480513', 'faber-1002-1792581480514', 'faber-1003-1792581480515'],
      },
    ],
  },
});

/** The mirror answering but not caching, with the reason it gives. */
export const mirrorPassing = online({
  mirror: {
    state: 'passing_through',
    attention: [
      {
        key: 'mirror-passing',
        severity: 'warn',
        title: 'Mirror passing through',
        detail:
          "host disk is below faber's admission floor plus 5 GiB: storing nothing and evicting toward 80% of the cap. Jobs still fetch through it, uncached.",
      },
    ],
  },
});

/** The mirror's image not built on the runners' engine. */
export const mirrorImageMissing = online({
  mirror: {
    state: 'image_missing',
    attention: [
      {
        key: 'mirror-image',
        severity: 'bad',
        title: 'Mirror image missing',
        detail: "Not on the runners' engine; jobs run without the mirror until it is built.",
        since_ms: ago(3600),
        image: 'faber-mirror:0.3.12',
        command: 'deploy/build-mirror-image.sh',
      },
    ],
  },
});

/** macOS jobs parked behind gates only a person can open: no VM session, a golden image that failed. */
export const macosHeld = online({
  budget: budget({
    gates: { vm_session: { parked_now: 2 }, golden_image: { parked_now: 0 } },
    vm: {
      wall: 'open',
      session: 'down',
      session_detail: 'unlock the disk as _faber_mac after a reboot, at the FileVault screen: the VM supervisor is not reachable',
      refusal: { gate: 'vm_session', detail: 'no macOS VM session: unlock the disk as _faber_mac after a reboot, at the FileVault screen' },
      macos_cap: 2,
      macos_running: 0,
      images: { linux: 'current', macos: 'failed:boot' },
      golden_required: true,
      golden: [{ image: 'macos-golden', standing: 'failed:boot', detail: 'it did not boot; run faber golden verify macos-golden after fixing it' }],
    },
  }),
  inflight: [
    job({ profile: 'macos-native', job_name: 'release', admitted: false, step: null, waiting: { gate: 'vm_session', detail: 'no macOS VM session: unlock the disk as _faber_mac after a reboot, at the FileVault screen' },  elapsed_s: 900 }),
    job({ profile: 'macos-native', job_name: 'publish', admitted: false, step: null, waiting: { gate: 'vm_session', detail: 'no macOS VM session: unlock the disk as _faber_mac after a reboot, at the FileVault screen' },  elapsed_s: 900 }),
  ],
});

/** Two classes OOM-killed at the escalation threshold (ADR-0053): the worst named, the other counted. */
export const oomHeld = online({
  budget: budget({
    oom_held: [
      {
        key: 'owner/demo::1::ci',
        label: ['owner/demo', 'CI', 'test'].join(DAEMON_LABEL_SEP),
        container: 'runner',
        cap_mib: 8192,
        reservation_mib: 8704,
        pin_runner_mib: 10240,
        pin_dind_mib: 512,
        ooms: 5,
        runs: 10,
      },
      {
        key: 'owner/demo::2::e2e',
        label: ['owner/demo', 'E2E', 'web'].join(DAEMON_LABEL_SEP),
        container: 'dind',
        cap_mib: 4096,
        reservation_mib: 6144,
        ooms: 2,
        runs: 10,
      },
    ],
  }),
});

/** The three action rows: a hold to release, a repo hold to clear, and a staged update. */
export const attentionActions = online({
  budget: budget({ hold: { reason: 'disk cleanup', since_ms: ago(1800) } }),
  repo_holds: [{ repo: 'owner/demo', reason: 'a fork job asked for dind', since_ms: ago(600) }],
  update: { current: '0.3.12', phase: { phase: 'waiting_idle', version: '0.3.13' } },
});

export const offline: Snapshot = {
  online: false,
  base_url: BASE_URL,
  error: 'connection refused',
  app_version: '0.3.13',
};

/** More groups than the window can hold, under three attention rows: the list folds into "+N more groups". */
export const busyBeyondCap = online({
  ...busyState(),
  paused: true,
  draining: true,
  inflight: [
    ...(busyState().inflight ?? []),
    ...Array.from({ length: 18 }, (_, i) =>
      i % 3 === 2
        ? waits({ pr: 2800 + i, job_name: 'build', elapsed_s: 30 + i })
        : job({ pr: 2800 + i, job_name: 'test', elapsed_s: 60 + i * 7 }),
    ),
  ],
});

/**
 * Every finished pip: a live group with a passed and a CI-failed job beside it (both done), a
 * group whose only job just passed, one passed 25 s ago (inside its fade), one where CI failed in
 * two jobs (done, no red), and one where only faber's runner failed. A cancelled and a skipped
 * run draw nothing.
 */
export const finished = online({
  budget: budget({ docker_inflight: 2, reserved_mib: 4000 }),
  inflight: [
    job({ pr: 2760, job_name: 'api e2e', elapsed_s: 240, step: { name: 'Run e2e', index: 5, total: 12 } }),
    job({ pr: 2760, job_name: 'web unit', elapsed_s: 200, step: { name: 'Test', index: 4, total: 9 } }),
  ],
  outcomes: [
    verdict('success', { pr: 2760, job_name: 'lint', ts_ms: ago(5) }),
    verdict('failure', { pr: 2760, job_name: 'types', ts_ms: ago(12) }),
    verdict('success', { pr: 2761, job_name: 'build', ts_ms: ago(8) }),
    verdict('success', { pr: 2762, job_name: 'build', ts_ms: ago(25) }),
    verdict('failure', { pr: 2763, job_name: 'web unit', ts_ms: ago(26) }),
    verdict('timed_out', { pr: 2763, job_name: 'api e2e', ts_ms: ago(28) }),
    fault({ pr: 2764, job_name: 'integration', ts_ms: ago(150), result: 'timeout' }),
    verdict('cancelled', { pr: 2765, job_name: 'build', ts_ms: ago(10) }),
    verdict('skipped', { pr: 2766, job_name: 'deploy', ts_ms: ago(10) }),
  ],
});

/**
 * The Limits view's snapshot: a budget carrying the five knobs, with a 16 GiB budget and a 2 GiB
 * reserve (18 GiB of engine memory), for the limits stories.
 */
export const limits = (over: Partial<Budget> = {}): Snapshot =>
  online({
    budget: budget({
      budget_mib: 16384,
      reserved_mib: 6144,
      reserve_mib: 2048,
      headroom_mib: 1024,
      // eslint-disable-next-line design-tokens/no-raw-style-number -- the daemon's margin knob, not a style
      margin: 1.2,
      native_concurrency: 2,
      hard_ceiling: 4,
      hard_ceiling_setting: 4,
      docker_inflight: 2,
      ...over,
    }),
  });

/**
 * The busy fixture with five more groups, one per worst state a compact line can draw: running,
 * waiting, frozen, runner failed and done. They fold past the gauge column.
 */
export const folded = online({
  ...busyState(),
  inflight: [
    ...(busyState().inflight ?? []),
    job({ pr: 2770, job_name: 'unit', elapsed_s: 18, step: { name: 'Test', index: 3, total: 5 } }),
    waits({ pr: 2771, job_name: 'build', elapsed_s: 12 }),
    job({ pr: 2772, job_name: 'e2e', elapsed_s: 9, frozen: { since_ms: ago(5), reason: 'disk', detail: 'free space under the floor' } }),
  ],
  outcomes: [
    ...(busyState().outcomes ?? []),
    fault({ pr: 2774, job_name: 'integration', ts_ms: ago(45), result: 'timeout' }),
    verdict('success', { pr: 2775, job_name: 'lint', ts_ms: ago(4) }),
  ],
});

/** Running jobs under an `auto` hard ceiling the daemon resolved to 6: the Slots gate's setting line. */
export const ceilingAuto = online({
  ...running.state,
  budget: budget({ docker_inflight: 3, reserved_mib: 5632, hard_ceiling_setting: 'auto' }),
});

/** `state` with the rows under `keys` dismissed as they read now (ADR-0064 § 2). */
const dismissing = (state: DaemonState, keys: string[]): DaemonState => ({
  ...state,
  attention: {
    dismissed: daemonRows(state, NOW)
      .filter((r) => keys.includes(r.key))
      .map((r) => ({ key: r.key, ident: rowIdent(r), at_ms: ago(3600) })),
    trust_acked_ms: null,
  },
});

/**
 * Paused (blocking: no Dismiss), jobs refused here (dismissable), and an OOM-held class and a
 * trust bypass the operator dismissed: they fold into "2 dismissed" under the list.
 */
export const dismissed = online(
  dismissing(
    {
      paused: true,
      budget: budget({ oom_held: oomHeld.state!.budget!.oom_held }),
      refused_queued: [{ repo: 'owner/demo', workflow: 'Release', verdict: 'refuse:event:repository_dispatch' }],
      trust_bypasses_7d: 2,
    },
    ['oom-held', 'bypass'],
  ),
);

/** Every row dismissed: the popover is calm but for the "N dismissed" line. */
export const allDismissed = online(
  dismissing(
    {
      budget: budget({ oom_held: oomHeld.state!.budget!.oom_held }),
      trust_bypasses_7d: 2,
    },
    ['oom-held', 'bypass'],
  ),
);

/**
 * Three trust widenings this week (ADR-0064 § 4): two acknowledged, folded into one quiet row,
 * and one made since, a full row with Acknowledge.
 */
export const trustAcked = online({
  trust_changes: [
    { ts_ms: ago(3 * 86400), key: 'gate.allow_public_repos', sentence: 'public repos were allowed', via: 'session' },
    { ts_ms: ago(2 * 86400), key: 'services.postgres', sentence: 'a postgres service was declared', via: 'socket' },
    { ts_ms: ago(1800), key: 'gate.allow_fork_prs', sentence: 'fork pull requests were allowed', via: 'session' },
  ],
  attention: { dismissed: [], trust_acked_ms: ago(2 * 86400) },
});
