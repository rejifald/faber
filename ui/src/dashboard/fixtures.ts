// Canned `/api/state` bodies for the dashboard's tests and stories: `quiet` is a healthy host
// with nothing to say; `full` carries every field the daemon serves and trips every alarm.
// Names are acme/web, acme/api, acme/mobile and owner/demo on the hosts alpha and beta only
// (AGENTS.md: nothing private in the tree).
import type { Budget, ClassCap, DaemonState, Inflight, Outcome } from '../popover/types';
import { rowIdent } from '../popover/attention';
import { dashboardAlarms } from './alarms';
import type { JobClass, JobsView, PhaseId, PhaseSpread, RunFootprint, Spread } from './jobs';

/** The moment every fixture is read at. */
export const NOW = Date.UTC(2026, 9, 28, 12, 0, 0);
const ago = (s: number) => NOW - s * 1000;

const classes: ClassCap[] = [
  {
    key: 'acme/web|CI|unit',
    label: 'acme/web CI unit',
    runner_mib: 2048,
    dind_mib: 0,
    status: 'learned',
    pinned: false,
    samples: 24,
    runner_mcpu: 1500,
    dind_mcpu: 0,
    cpu_status: 'learned',
    cpu_pinned: false,
    reserve_p75_runner_mib: 1536,
    reserve_p75_dind_mib: 0,
    reserve_p75_mib: 1536,
    p75_runs: 20,
    p75_trips: 1,
  },
  {
    key: 'acme/api|CI|e2e',
    label: 'acme/api CI e2e',
    runner_mib: 3072,
    dind_mib: 2048,
    status: 'pinned',
    pinned: true,
    samples: 6,
    runner_mcpu: 2000,
    dind_mcpu: 1000,
    cpu_status: 'pinned',
    cpu_pinned: true,
    reserve_p75_runner_mib: 3072,
    reserve_p75_dind_mib: 2048,
    reserve_p75_mib: 5120,
    p75_runs: 0,
    p75_trips: 0,
  },
];

const healthyBudget: Budget = {
  budget_mib: 16384,
  available_mib: 10240,
  docker_reachable: true,
  reserved_mib: 3584,
  reserved_p75_mib: 3072,
  docker_inflight: 1,
  frozen: false,
  reserve_mib: 3072,
  headroom_mib: 1536,
  margin: 1.25, // eslint-disable-line design-tokens/no-raw-style-number -- the autolimiter's margin, not a style
  native_concurrency: 0,
  hard_ceiling: 4,
  hard_ceiling_setting: 'auto',
  hold: null,
  headroom: { jobs: 3, binds: 'memory', job_mib: 2048, job_mcpu: 1500 },
  cpu: { mode: 'auto', ncpu: 10, budget_mcpu: 8000, reserved_mcpu: 1500, pressure: 22, frozen: false, overcommit: 1.5, reserve_cpus: 2, profiling_cpus: 2 },
  disk: { mode: 'on', floor_gib: 10, free_mib: 120 * 1024, reading_age_s: 4, fresh: true, ramping: 0, held_mib: 0, charge_mib: null, closed: false },
  machine: {
    share: 'declared',
    keep_memory_gib: 8,
    keep_cpus: 2,
    yields: true,
    mem_mib: 32 * 1024,
    cpus: 12,
    perf_cpus: 6,
    efficiency_cpus: 6,
    vm_mem_mib: 20 * 1024,
    vm_cpus: 10,
    boundary: true,
    leaves_mib: 24 * 1024,
    leaves_cpus: 10,
    fresh: true,
    pressure: 'normal',
    psi_some_avg60: null,
    swap_used_mib: 512,
    swap_total_mib: 4096,
    swap_rise_mib: 0,
    compressor_mib: 1024,
    hold: null,
    encroaches: [],
  },
  cotenant: { mode: 'measured', floor_mib: 3072, latest_mib: 2048, latest_age_s: 10, peak_mib: 3400, history_s: 86400, reserve_mib: 3072, peak_binds: false },
  warnings: [],
  gates: { memory: { parked_now: 0, parks: 12, parked_ms: 240000, binding: 3 } },
  classes,
  reserve: { mode: 'max', unknown: null, in_force: 'max', cooling: false, squeeze: false, freeze_armed: false, last_trip: null, next_rearm_ms: null, armed_ms: 0, next_cooloff_ms: 86400000, trips_7d: 0, trips: [] },
  vm: {
    wall: 'open',
    wall_detail: null,
    wall_required: false,
    images: { linux: 'current', macos: 'verified' },
    session: 'up',
    session_detail: null,
    refusal: null,
    golden_required: true,
    golden: [{ image: 'macos-golden', standing: 'verified', detail: null }],
    macos_cap: 2,
    macos_running: 0,
    macos_vm_mib: 8192,
    linux_vm_mib: 20480,
    vms: [],
  },
};

const running: Inflight = {
  job_id: 9001,
  profile: 'linux',
  repo: 'acme/web',
  pr: 482,
  branch: 'feature/cache',
  workflow: 'CI',
  job_name: 'unit',
  event: 'pull_request',
  elapsed_s: 140,
  admitted: true,
  step: { name: 'Run tests', index: 3, total: 9 },
  waiting: null,
  frozen: null,
};

const finished: Outcome = {
  ts_ms: ago(300),
  repo: 'acme/web',
  job_id: 8990,
  profile: 'linux',
  runner: 'faber-8990-1792000000-alpha',
  label: 'acme/web | CI | unit',
  result: 'success',
  duration_s: 212,
  wait_s: 0,
  wait_gate: null,
  attempt: 1,
  cause: null,
  pr: 480,
  branch: 'main',
  workflow: 'CI',
  job_name: 'unit',
  event: 'push',
  conclusion: 'success',
  conclusion_ts_ms: ago(280),
};

/** A healthy host with one job running and nothing that needs attention. */
export const quiet: DaemonState = {
  owner: 'acme',
  topic: 'faber',
  instance: 'alpha',
  profiles: [{ name: 'linux', labels: ['faber', 'docker'], backend: 'docker', dind: 'denied', needs_wall: false, runner_version: '2.330.0' }],
  concurrency: 1,
  paused: false,
  draining: false,
  healthy: true,
  last_error: null,
  uptime_s: 93600,
  audit_path: '/var/lib/faber/audit.log',
  counts: { provisioned: 41, completed: 40, faults: 0, refused: 0, retried: 0, dind_provided: 0, dind_withheld: 0, dind_refused_total: 0, service_refused_total: 0 },
  gate: { allowed_events: ['push', 'pull_request'], excluded_events: [], opt_in_events: [], allow_public_repos: false, allow_fork_prs: false, on_bypass: 'enforce' },
  credential: { status: 'stored', owner: 'acme', expires_at_ms: NOW + 90 * 86400000, stored_at_ms: ago(30 * 86400), reason: null, missing_permissions: [], unverified_permissions: [], mismatches: { granted_not_opted_in: [], opted_in_not_granted: [] }, file_warning: null },
  dind_label_rule: 'on',
  inflight: [running],
  outcomes: [finished],
  recent: [{ ts_ms: ago(320), repo: 'acme/web', job_id: 9001, event: 'pull_request', actor: 'octo-dev', verdict: 'provision', profile: 'linux' }],
  faults: [],
  drift: [],
  retries: [],
  peers: [],
  budget: healthyBudget,
  update: { current: '0.4.1', enabled: true, available: null, candidate_reason: null, install_in_secs: null, install_after_ms: null, phase: { phase: 'idle' }, last_check_ms: ago(600), last_error: null, quarantined: [], rolled_back_from: null, pin: null, next_attempt_ms: null, self_check: { status: 'verified', at_ms: ago(93000), detail: null, version: '0.4.1', attempts: 1 }, supervisor_restart: null },
  awake: { enabled: false, mode: 'off', supported: true, held: false, held_since_ms: null, release_at_ms: null, error: null },
  missing_images: [],
  unqualified_images: [],
  mirror: null,
  runner_retirements: [],
  repo_holds: [],
  refused_queued: [],
  refused_queued_overflow: 0,
  dind_refused: [],
  service_refused: [],
  bypasses: [],
  trust_bypasses_7d: 0,
  notices: [],
  trust_changes: [],
  git_mirrors: [{ repo: 'acme/web', ready: true, refreshing: false, size_kib: 20480, last_refresh_ms: ago(120), last_attempt_ms: ago(120), last_error: null, blocked: null }],
};

/** Every field the daemon serves, and every alarm the dashboard raises. */
export const full: DaemonState = {
  ...quiet,
  paused: true,
  draining: true,
  healthy: false,
  last_error: 'GitHub answered 502 listing queued runs',
  profiles: [
    { name: 'linux', labels: ['faber', 'docker'], backend: 'docker', dind: 'unstated', needs_wall: true, runner_version: '2.330.0' },
    { name: 'macos', labels: ['faber', 'macos'], backend: 'macos-vm', dind: null, needs_wall: false, runner_version: null },
  ],
  counts: { provisioned: 400, completed: 380, faults: 9, refused: 14, retried: 6, dind_provided: 30, dind_withheld: 2, dind_refused_total: 3, service_refused_total: 1 },
  gate: { allowed_events: ['push', 'pull_request'], excluded_events: ['workflow_dispatch'], opt_in_events: ['repository_dispatch'], allow_public_repos: true, allow_fork_prs: false, on_bypass: 'record' },
  credential: {
    status: 'expiring',
    owner: 'acme',
    expires_at_ms: NOW + 5 * 86400000,
    stored_at_ms: ago(80 * 86400),
    reason: 'the GitHub token expires in 5 days (2026-11-02): use "Regenerate token" on its page on GitHub',
    missing_permissions: [{ repo: 'acme/mobile', permission: 'checks: read', feature: 'auto-retry', fix: 'add Checks: read to the token' }],
    unverified_permissions: ['administration: write'],
    mismatches: { granted_not_opted_in: ['acme/api'], opted_in_not_granted: ['owner/demo'] },
    file_warning: 'the token file is readable by group',
  },
  dind_label_rule: 'unstated',
  notices: [
    { kind: 'legacy_layout', text: 'This host is still on the LEGACY layout.' },
    { kind: 'linux_vm_waiting', text: 'The Linux VM is switched off; Docker jobs wait for it.' },
  ],
  trust_changes: [{ ts_ms: ago(7200), key: 'gate.allow_public_repos', sentence: 'public repos were allowed', via: 'session', route: '/api/settings', uid: 501 }],
  missing_images: [
    { profile: 'linux', image: 'registry.example/acme/runner:2.330.0', since_ms: ago(1800), cause: 'pull access denied' },
    { profile: 'macos', image: 'registry.example/acme/runner-mac:1', since_ms: ago(900), cause: 'manifest unknown' },
  ],
  unqualified_images: [{ profile: 'linux', image: 'runner:latest', message: 'Profile linux pulls runner:latest from a Docker Hub user namespace because pull_unqualified_images is unstated' }],
  runner_retirements: [
    { profile: 'linux', days_remaining: 2, urgent: true, runner_version: '2.330.0', cutoff: '2026-10-30', image: 'registry.example/acme/runner:2.330.0', job_id: 9001, seen_ms: ago(3600), message: 'actions/runner 2.330.0 stops on 2026-10-30 (2 days) for linux' },
    { profile: 'macos', days_remaining: 20, urgent: false, runner_version: '2.331.0', cutoff: '2026-11-17', image: null, job_id: 9002, seen_ms: ago(3600), message: 'actions/runner 2.331.0 stops on 2026-11-17 (20 days) for macos' },
  ],
  peers: [{ instance: 'beta', first_seen_ms: ago(86400), last_seen_ms: ago(20), runners: { linux: 2 } }],
  git_mirrors: [
    { repo: 'acme/web', ready: true, refreshing: true, size_kib: 20480, last_refresh_ms: ago(120), last_attempt_ms: ago(5), last_error: 'fetch failed: connection reset', blocked: null },
    { repo: 'acme/api', ready: false, refreshing: false, size_kib: null, last_refresh_ms: null, last_attempt_ms: null, last_error: null, blocked: 'is held by a volume faber did not label' },
  ],
  mirror: {
    state: 'degraded',
    max_size_gib: 20,
    attention: [
      { key: 'mirror-down', severity: 'bad', title: 'Mirror down', detail: 'the mirror service is not answering.', since_ms: ago(600), error: 'connection refused', affected: ['acme/web CI unit', 'acme/api CI e2e'] },
      { key: 'mirror-image', severity: 'warn', title: 'Mirror image missing', detail: 'the image is not built.', image: 'faber-mirror:1', command: 'faber mirror build' },
    ],
    lfs: { fill: 'relaxed', objects: 120, bytes: 5 * 1024 ** 3, max_bytes: 20 * 1024 ** 3, repos: [{ repo: 'acme/web', hits: 90, misses: 30, bytes_from_cache: 4 * 1024 ** 3, bytes_from_upstream: 1024 ** 3, passed_through: 0 }] },
    npm: { fill: 'off', objects: 0, bytes: 0, max_bytes: 20 * 1024 ** 3, repos: [] },
  },
  repo_holds: [{ repo: 'owner/demo', reason: 'a trust bypass ran job 77 (refuse:actor:octo-bot)', since_ms: ago(5400), by: 'gate' }],
  refused_queued: [{ repo: 'acme/web', job_id: 9100, run_id: 5100, job_name: 'deploy', workflow: 'Release', event: 'repository_dispatch', verdict: 'refuse:event:repository_dispatch', since_ms: ago(660), labels: ['faber', 'docker'] }],
  refused_queued_overflow: 2,
  dind_refused: [{ repo: 'acme/api', run_id: 5200, job_id: 9200, job_name: 'e2e', profile: 'linux', reason: 'asked for dind, but profile linux grants none', at_ms: ago(120) }],
  service_refused: [{ repo: 'acme/mobile', run_id: 5300, job_id: 9300, job_name: 'db tests', profile: 'linux', label: 'faber.postgres-16', reason: 'this host provides no postgres', at_ms: ago(90) }],
  bypasses: [{ ts_ms: ago(5400), repo: 'owner/demo', run_id: 5000, job_id: 77, job_name: 'lint', workflow: 'CI', runner: 'faber-70-1792000000-alpha', minted_for: 70, class: 'trust', verdict: 'refuse:actor:octo-bot' }],
  trust_bypasses_7d: 3,
  inflight: [
    running,
    {
      ...running,
      job_id: 9002,
      repo: 'acme/api',
      pr: 77,
      job_name: 'e2e',
      admitted: false,
      step: null,
      waiting: { gate: 'memory', detail: 'needs 5120 MiB, 1024 MiB free of 16384 MiB' },
    },
    {
      ...running,
      job_id: 9003,
      repo: 'acme/mobile',
      pr: null,
      branch: 'main',
      job_name: 'build',
      frozen: { since_ms: ago(240), reason: 'disk', detail: 'the Docker volume is below the critical floor', thaw_failure: { at_ms: ago(30), error: 'docker unpause: no such container' } },
    },
  ],
  outcomes: [
    finished,
    { ...finished, job_id: 8991, ts_ms: ago(200), result: 'infra_error', duration_s: 3600, wait_s: 45, wait_gate: 'memory', attempt: 2, cause: 'runner lost communication', conclusion: null, conclusion_ts_ms: null },
  ],
  recent: quiet.recent,
  faults: [
    { ts_ms: ago(400), repo: 'acme/web', scope: 'docker', action: 'unreachable', cause: 'Docker is not answering' },
    { ts_ms: ago(380), repo: 'acme/web', scope: 'docker', action: 'recovered', cause: 'Docker answered again' },
  ],
  drift: [{ ts_ms: ago(1000), repo: 'acme/web', run_id: 5001, job_id: 9001, status: 'drifted', baked: '2.330.0', pinned: '2.331.0', mode: 'warn', action: 'warned' }],
  retries: [{ ts_ms: ago(500), repo: 'acme/web', run_id: 5002, attempt: 1, jobs: 2, mode: 'failed-jobs' }],
  awake: { enabled: true, mode: 'while_busy', supported: true, held: false, held_since_ms: null, release_at_ms: null, error: 'IOPMAssertionCreate failed' },
  budget: {
    ...healthyBudget,
    frozen: true,
    hold: { reason: 'maintenance on the shared cache', since_ms: ago(7200) },
    warnings: ['acme/api CI e2e learned a cap above the escalation threshold: confirm or pin it'],
    cpu: { ...healthyBudget.cpu, pressure: 91, frozen: true },
    disk: { mode: 'on', floor_gib: 10, free_mib: 9 * 1024, reading_age_s: 400, fresh: false, ramping: 2, held_mib: 6144, charge_mib: 8192, closed: true },
    machine: { ...healthyBudget.machine, hold: 'machine-pressure', pressure: 'warn', encroaches: ['The Docker VM (20.0 GiB) is larger than the 12.0 GiB the machine share leaves for CI.', 'The VM has 10 cores; the share leaves 6.'] },
    cotenant: { ...healthyBudget.cotenant, peak_binds: true },
    gates: {
      memory: { parked_now: 1, parks: 40, parked_ms: 900000, binding: 8 },
      disk: { parked_now: 0, parks: 4, parked_ms: 60000, binding: 1 },
      vm_session: { parked_now: 2, parks: 2, parked_ms: 30000, binding: 0 },
      golden_image: { parked_now: 1, parks: 1, parked_ms: 10000, binding: 0 },
    },
    reserve: {
      mode: 'p75',
      unknown: null,
      in_force: 'max',
      cooling: true,
      squeeze: true,
      freeze_armed: true,
      last_trip: { at_ms: ago(7200), cause: 'freeze', cooloff_ms: 86400000, until_ms: NOW + 79200000 },
      next_rearm_ms: NOW + 79200000,
      armed_ms: 0,
      next_cooloff_ms: 172800000,
      trips_7d: 2,
      trips: [{ at_ms: ago(7200), cause: 'freeze', cooloff_ms: 86400000, until_ms: NOW + 79200000 }],
    },
    vm: {
      wall: 'red',
      wall_detail: 'The pf anchor is not loaded.',
      wall_required: true,
      images: { linux: 'out_of_date', macos: 'unverified' },
      runner_image: 'the runner image is missing',
      session: 'down',
      session_detail: 'no login session; unlock the Mac once',
      refusal: { gate: 'vm_session', detail: 'No macOS VM session' },
      golden_required: true,
      golden: [{ image: 'macos-golden', standing: 'failed:boot', detail: 'the guest did not boot' }],
      macos_cap: 2,
      macos_running: 2,
      macos_vm_mib: 8192,
      linux_vm_mib: 20480,
      vms: [{ kind: 'macos', slot: 1, state: 'in_service', stop_reason: null }, { kind: 'linux', slot: 0, state: 'stopped', stop_reason: 'guest_shutdown' }],
    },
  },
  update: {
    current: '0.4.1',
    enabled: true,
    available: '0.4.2',
    candidate_reason: 'newest release',
    install_in_secs: null,
    install_after_ms: null,
    phase: { phase: 'failed', reason: 'the signature did not verify' },
    last_check_ms: ago(60),
    last_error: 'the signature did not verify',
    quarantined: ['0.4.0'],
    rolled_back_from: '0.4.0',
    pin: '0.4.1',
    next_attempt_ms: NOW + 1800000,
    self_check: { status: 'unproven', at_ms: ago(3600), detail: 'could not reach the release host', version: '0.4.1', attempts: 3 },
    supervisor_restart: { status: 'not_back', detail: 'the VM supervisor did not come back', at_ms: ago(1800) },
  },
};

// --- the Jobs view (ADR-0056) --------------------------------------------------------------------

const spread = (median: number | null, p90: number | null, runs: number): Spread => ({ median, p90, runs });

/** A class's phases from medians and p90s, in order; `null` is a phase no run measured. */
function phasesOf(rows: [PhaseId, number | null, number | null][], runs: number, skewed: Partial<Record<PhaseId, number>> = {}): PhaseSpread[] {
  return rows.map(([phase, median, p90]) => ({ phase, ...spread(median, p90, median === null ? 0 : runs), skewed_runs: skewed[phase] ?? 0 }));
}

/** A busy class that mostly waits for memory. */
const parkedClass: JobClass = {
  key: 'acme/api::42::e2e',
  label: 'acme/api CI e2e',
  repo: 'acme/api',
  workflow: 'CI',
  job: 'e2e',
  runs: 64,
  profiling: false,
  unmeasured_resource_runs: 0,
  wall: spread(1_260_000, 2_400_000, 64),
  total_ms: 64 * 1_260_000,
  phases: phasesOf(
    [
      ['pickup', 11_000, 21_000],
      ['prepare', 1_200, 48_000],
      ['parked', 540_000, 1_380_000],
      ['sandbox', 38_000, 61_000],
      ['register', 9_000, 14_000],
      ['job', 620_000, 830_000],
      ['teardown', 12_000, 19_000],
    ],
    64,
  ),
  split: { setup: spread(212_000, 260_000, 61), steps: spread(392_000, 548_000, 61), teardown: spread(16_000, 22_000, 61) },
  parked_share: 0.43,
  top_gate: { gate: 'memory', runs: 48 },
  cpu_used_mcpu: spread(1_400, 2_100, 64),
  cpu_reserved_mcpu: spread(3_000, 3_000, 64),
  throttled_ms: spread(0, 1_200, 64),
  io_bytes: spread(1_800_000_000, 3_100_000_000, 64),
};

/** A quick class whose CPU quota throttles it. */
const throttledClass: JobClass = {
  key: 'acme/web::7::unit',
  label: 'acme/web CI unit',
  repo: 'acme/web',
  workflow: 'CI',
  job: 'unit',
  runs: 212,
  profiling: false,
  unmeasured_resource_runs: 0,
  wall: spread(312_000, 455_000, 212),
  total_ms: 212 * 312_000,
  phases: phasesOf(
    [
      ['pickup', 10_000, 20_000],
      ['prepare', 400, 900],
      ['parked', 0, 4_000],
      ['sandbox', 6_000, 9_000],
      ['register', 7_000, 11_000],
      ['job', 281_000, 402_000],
      ['teardown', 4_000, 7_000],
    ],
    212,
    { pickup: 3 },
  ),
  split: { setup: spread(48_000, 71_000, 212), steps: spread(226_000, 322_000, 212), teardown: spread(7_000, 9_000, 212) },
  parked_share: 0.01,
  top_gate: { gate: 'cpu', runs: 9 },
  cpu_used_mcpu: spread(1_950, 2_000, 212),
  cpu_reserved_mcpu: spread(2_000, 2_000, 212),
  throttled_ms: spread(41_000, 96_000, 212),
  io_bytes: spread(420_000_000, 690_000_000, 212),
};

/** A macOS class: phases and steps only, no cgroups. */
const macosClass: JobClass = {
  key: 'acme/mobile iOS build',
  label: 'acme/mobile iOS build',
  repo: 'acme/mobile',
  workflow: 'iOS',
  job: 'build',
  runs: 31,
  profiling: false,
  unmeasured_resource_runs: 31,
  wall: spread(1_520_000, 1_910_000, 31),
  total_ms: 31 * 1_520_000,
  phases: phasesOf(
    [
      ['pickup', 12_000, 19_000],
      ['prepare', 100, 300],
      ['parked', 60_000, 420_000],
      ['sandbox', 74_000, 96_000],
      ['register', 11_000, 18_000],
      ['job', 1_340_000, 1_620_000],
      ['teardown', 9_000, 14_000],
    ],
    31,
  ),
  split: { setup: spread(388_000, 455_000, 29), steps: spread(931_000, 1_140_000, 29), teardown: spread(14_000, 21_000, 29) },
  parked_share: 0.05,
  top_gate: { gate: 'vm-memory', runs: 7 },
  cpu_used_mcpu: spread(null, null, 0),
  cpu_reserved_mcpu: spread(null, null, 0),
  throttled_ms: spread(null, null, 0),
  io_bytes: spread(null, null, 0),
};

/** A class with too few runs to rank: shown, labelled, sorted last. */
const profilingClass: JobClass = {
  key: 'owner/demo::3::lint',
  label: 'owner/demo CI lint',
  repo: 'owner/demo',
  workflow: 'CI',
  job: 'lint',
  runs: 4,
  profiling: true,
  unmeasured_resource_runs: 0,
  wall: spread(96_000, 140_000, 4),
  total_ms: 4 * 96_000,
  phases: phasesOf(
    [
      ['pickup', 9_000, 18_000],
      ['prepare', 300, 600],
      ['parked', 0, 0],
      ['sandbox', 5_000, 8_000],
      ['register', null, null],
      ['job', null, null],
      ['teardown', null, null],
    ],
    4,
  ),
  split: { setup: spread(null, null, 0), steps: spread(null, null, 0), teardown: spread(null, null, 0) },
  parked_share: 0,
  top_gate: null,
  cpu_used_mcpu: spread(800, 1_100, 4),
  cpu_reserved_mcpu: spread(1_000, 1_000, 4),
  throttled_ms: spread(0, 0, 4),
  io_bytes: spread(80_000_000, 120_000_000, 4),
};

/** A week of four classes: one parked, one throttled, a macOS one and one still profiling. */
export const jobsWeek: JobsView = {
  window_days: 7,
  from_ms: NOW - 7 * 86_400_000,
  to_ms: NOW,
  profiling_runs: 20,
  classes: [parkedClass, throttledClass, macosClass, profilingClass],
};

/** A host with no recorded run in the window. */
export const jobsEmpty: JobsView = { ...jobsWeek, classes: [] };

/** One Docker run, every phase measured, with its steps. */
export const runFootprint: RunFootprint = {
  runner: 'faber-8990-1792000000-alpha',
  job_id: 8990,
  label: 'acme/api CI e2e',
  result: 'success',
  finished_ms: ago(300),
  phases: [
    { phase: 'pickup', ms: 12_000, skewed: false },
    { phase: 'prepare', ms: 900, skewed: false },
    { phase: 'parked', ms: 410_000, skewed: false },
    { phase: 'sandbox', ms: 36_000, skewed: false },
    { phase: 'register', ms: 8_000, skewed: false },
    { phase: 'job', ms: 598_000, skewed: false },
    { phase: 'teardown', ms: 11_000, skewed: false },
  ],
  split: { setup: 205_000, steps: 377_000, teardown: 16_000 },
  steps: null,
  wait_gate: 'memory',
  cpu_used_mcpu: 1_400,
  cpu_reserved_mcpu: 3_000,
  throttled_ms: 0,
  io_bytes: 1_700_000_000,
};

/** A macOS run whose pickup crossed clocks, and whose steps were never read. */
export const runFootprintPartial: RunFootprint = {
  ...runFootprint,
  runner: 'faber-8991-1792000100-alpha',
  job_id: 8991,
  label: 'acme/mobile iOS build',
  phases: [
    { phase: 'pickup', ms: 0, skewed: true },
    { phase: 'prepare', ms: 100, skewed: false },
    { phase: 'parked', ms: 30_000, skewed: false },
    { phase: 'sandbox', ms: 81_000, skewed: false },
    { phase: 'register', ms: 12_000, skewed: false },
    { phase: 'job', ms: 1_210_000, skewed: false },
    { phase: 'teardown', ms: null, skewed: false },
  ],
  split: null,
  wait_gate: 'vm-memory',
  cpu_used_mcpu: null,
  cpu_reserved_mcpu: null,
  throttled_ms: null,
  io_bytes: null,
};

/** `state` with the alarms under `keys` dismissed as they read now (ADR-0064 § 2), and trust acknowledged to `acked`. */
const dismissing = (state: DaemonState, keys: string[], acked: number | null = null): DaemonState => ({
  ...state,
  attention: {
    dismissed: dashboardAlarms(state, NOW)
      .filter((a) => keys.includes(a.key))
      .map((a) => ({ key: a.key, ident: rowIdent(a), at_ms: ago(3600) })),
    trust_acked_ms: acked,
  },
});

const E2E_WARNING = 'acme/api CI e2e learned a cap above the escalation threshold: confirm or pin it';
const MOBILE_WARNING = 'acme/mobile build wants 6 cores, held at 4: pin to allow';

/**
 * `full` from a daemon that keys its warnings (ADR-0064): two of them, the e2e one, the public-repos
 * gate and the dind wall dismissed, and the one trust change acknowledged into its quiet row.
 */
export const dismissedAlarms: DaemonState = dismissing(
  {
    ...full,
    budget: {
      ...full.budget,
      warnings: [E2E_WARNING, MOBILE_WARNING],
      warning_items: [
        { key: 'memory:acme/api|CI|e2e', text: E2E_WARNING },
        { key: 'cpu:acme/mobile|CI|build', text: MOBILE_WARNING },
      ],
    },
  },
  ['warning:memory:acme/api|CI|e2e', 'gate-public', 'dind-wall'],
  ago(3600),
);
