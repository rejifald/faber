// The shapes the popover reads. The app hands the webview the daemon's `GET /api/state` body
// verbatim (#180), wrapped in the app's own snapshot (desktop/src-tauri/src/lib.rs `Snapshot`).
// Every field is optional: an older daemon omits newer ones, and the popover renders what it
// gets rather than assuming a field (popover/anatomy.md § 3).

export interface StepView {
  name?: string;
  index?: number;
  total?: number;
}

export interface WaitView {
  gate?: string;
  detail?: string;
}

export interface FrozenView {
  since_ms?: number;
  reason?: 'disk' | 'memory' | string;
  detail?: string;
  /** A thaw that keeps failing is not a job waiting for room (#111). */
  thaw_failure?: { at_ms: number; error: string } | null;
}

export interface Inflight {
  job_id: number;
  profile?: string;
  repo?: string;
  pr?: number | null;
  branch?: string | null;
  workflow?: string | null;
  job_name?: string;
  event?: string;
  elapsed_s?: number;
  admitted?: boolean;
  step?: StepView | null;
  waiting?: WaitView | null;
  frozen?: FrozenView | null;
  /** `JobPhase` in src/reconcile.rs; absent from an older daemon, which only says `admitted`. */
  phase?: 'queued' | 'starting' | 'running' | 'frozen';
}

/**
 * A finished run (`OutcomeEvent` in src/reconcile.rs). `result` measures the runner container,
 * not CI. The identity fields and `conclusion` arrive with PR 2's daemon change; until then they
 * are absent and the popover falls back as `groups.ts` says.
 */
export interface Outcome {
  ts_ms: number;
  repo?: string;
  job_id: number;
  profile?: string;
  label?: string;
  result?: string;
  attempt?: number;
  pr?: number | null;
  branch?: string | null;
  workflow?: string | null;
  job_name?: string | null;
  event?: string | null;
  conclusion?: string | null;
  /** The runner's name (`faber-<instance>-<job>-<ts>`). */
  runner?: string;
  /** How long the runner lived, and how much of it a gate parked the job (issue #85). */
  duration_s?: number;
  wait_s?: number;
  wait_gate?: string | null;
  /** Free text from docker's stderr or GitHub's error bodies: set it as text, never markup. */
  cause?: string | null;
  /** When the daemon read `conclusion` (ADR-0044): CI pips linger from here. Absent from an older daemon. */
  conclusion_ts_ms?: number | null;
}

export interface Peer {
  instance: string;
  first_seen_ms?: number;
  last_seen_ms?: number;
  runners?: Record<string, number>;
  /** Of those, the online ones running a job, by repo. Absent from an older daemon. */
  busy?: Record<string, number>;
  /** Of those, the online ones with no job, by repo. Absent from an older daemon. */
  idle?: Record<string, number>;
}

export interface Headroom {
  jobs?: number;
  binds?: string;
  job_mib?: number;
  job_mcpu?: number;
}

export interface VmView {
  wall?: string;
  wall_detail?: string | null;
  /** Whether a red wall also stops Docker jobs (`[host] wall_required`). */
  wall_required?: boolean;
  macos_vm_mib?: number;
  linux_vm_mib?: number;
  images?: { linux?: string; macos?: string };
  runner_image?: string;
  /** The macOS VM session (ADR-0051): `up` and `not_required` admit; anything else refuses. */
  session?: string;
  session_detail?: string | null;
  /** The gate refusing a macOS VM job right now, if one does. */
  refusal?: { gate?: string; detail?: string } | null;
  /** macOS jobs wait for a verified golden image (ADR-0049 §7). */
  golden_required?: boolean;
  /** `standing`: `verified`, `unverified`, `verifying`, `failed:<check>` or `unknown`. */
  golden?: Array<{ image: string; standing?: string; detail?: string | null }> | null;
  macos_cap?: number;
  macos_running?: number;
  vms?: VmEntry[];
  /** Seconds since the last successful supervisor list; absent from an older daemon. */
  listed_age_s?: number | null;
}

/** One VM this host runs (popover/vms/anatomy.md § 5). A field an older daemon omits means "not known". */
export interface VmEntry {
  kind: 'linux' | 'macos' | string;
  slot: number;
  state: string;
  stop_reason?: string | null;
  started_unix?: number;
  cpus?: number | null;
  memory_mib?: number | null;
  /** What it serves; `job_name` is null for a fork or untrusted-actor job. */
  job?: { repo: string; job_name: string | null } | null;
}

/** Per-gate counters (issue #85): which limit has actually been binding. */
export interface GateCount {
  parked_now?: number;
  /** Arrivals since faber started. */
  parks?: number;
  parked_ms?: number;
  binding?: number;
}

/** One class's learned (or pinned) caps, `ClassCapView` in src/limiter.rs. */
export interface ClassCap {
  key: string;
  label: string;
  runner_mib: number;
  dind_mib: number;
  /** `profiling`, `learned`, `warned` or `pinned`. */
  status: string;
  pinned: boolean;
  samples: number;
  runner_mcpu: number;
  dind_mcpu: number;
  cpu_status: string;
  cpu_pinned: boolean;
  reserve_p75_runner_mib: number;
  reserve_p75_dind_mib: number;
  /** What a p75 reservation would debit; equal to the caps until the window is full. */
  reserve_p75_mib: number;
  p75_runs: number;
  p75_trips: number;
  /** The windows of measured peaks the caps are learned from, oldest first (MiB). Separate windows,
   * not one per run: a run that left no dind sample adds to the runner's only. Absent from an older daemon. */
  peaks_runner_mib?: number[];
  peaks_dind_mib?: number[];
  /** Parallel to each window: true where that sample was an OOM stand-in, not a measurement. */
  censored_runner?: boolean[];
  censored_dind?: boolean[];
}

/** What the Docker VM's other tenants use (`CoTenantView`, src/cotenant.rs). */
export interface CoTenantView {
  /** `measured` or `unstated`. */
  mode?: string;
  floor_mib?: number;
  latest_mib?: number | null;
  latest_age_s?: number | null;
  peak_mib?: number | null;
  history_s?: number;
  reserve_mib?: number;
  peak_binds?: boolean;
}

/** The machine itself (`MachineView`, src/machine.rs): its memory, cores, pressure and swap. */
export interface MachineView {
  /** `unstated` or declared. */
  share?: string;
  keep_memory_gib?: number | null;
  keep_cpus?: number | null;
  yields?: boolean;
  mem_mib?: number | null;
  cpus?: number | null;
  perf_cpus?: number | null;
  efficiency_cpus?: number | null;
  vm_mem_mib?: number | null;
  vm_cpus?: number | null;
  boundary?: boolean;
  leaves_mib?: number | null;
  leaves_cpus?: number | null;
  fresh?: boolean;
  pressure?: string | null;
  psi_some_avg60?: number | null;
  swap_used_mib?: number | null;
  swap_total_mib?: number | null;
  swap_rise_mib?: number;
  compressor_mib?: number | null;
  /** `machine-pressure` or `swap` while the machine holds admission. */
  hold?: string | null;
  /** Docker VM larger than the machine's share (ADR-0021 § 2a): each line is the fix, as the daemon words it. */
  encroaches?: string[];
}

/** One trip of the p75 reserve's fallback (`TripView`, src/reserve.rs). */
export interface ReserveTrip {
  at_ms?: number;
  cause?: string;
  cooloff_ms?: number;
  until_ms?: number;
}

export interface Budget {
  budget_mib?: number;
  reserved_mib?: number;
  hard_ceiling?: number | null;
  docker_reachable?: boolean;
  /** What the reservation would be under p75 (ADR-0033 step 1): measured, not debited. */
  reserved_p75_mib?: number;
  /** The autolimiter knobs the Limits view steps (settings/anatomy.md § 1); an older daemon omits them. */
  reserve_mib?: number;
  headroom_mib?: number;
  margin?: number;
  native_concurrency?: number;
  hard_ceiling_setting?: 'auto' | 'off' | number;
  available_mib?: number;
  docker_inflight?: number;
  headroom?: Headroom | null;
  gates?: Record<string, GateCount | null>;
  cpu?: {
    /** `auto`, `off`, `unstated` or a number as text. */
    mode?: string;
    ncpu?: number | null;
    budget_mcpu?: number;
    reserved_mcpu?: number;
    pressure?: number | null;
    /** Frozen under CPU pressure. */
    frozen?: boolean;
    overcommit?: number;
    reserve_cpus?: number;
    profiling_cpus?: number;
  } | null;
  disk?: {
    /** `on`, `off` or `unstated`. */
    mode?: string;
    floor_gib?: number | null;
    free_mib?: number | null;
    /** How old the free-space reading is; `fresh` is false once it no longer holds anything. */
    reading_age_s?: number | null;
    fresh?: boolean;
    /** Jobs still starting, whose disk is held (`held_mib`). */
    ramping?: number;
    held_mib?: number;
    /** What a parked job needs above the floor. */
    charge_mib?: number | null;
    /** No room for any job above the floor. */
    closed?: boolean;
    /** Free disk inside the Docker VM, where the binding gate reads it. */
    vm_free_mib?: number | null;
    /** The Docker VM's disk: real size, ceiling, a pending grow (ADR-0061). */
    vm_grow?: {
      disk_bytes?: number | null;
      disk_allocated_bytes?: number | null;
      max_bytes?: number;
      pending_to_bytes?: number | null;
      waiting_for?: string | null;
      fault?: string | null;
    } | null;
  } | null;
  hold?: { reason?: string; since_ms?: number } | null;
  frozen?: boolean;
  machine?: MachineView | null;
  cotenant?: CoTenantView | null;
  /** Pending warnings, as the daemon words them (an escalation awaiting a pin, say). */
  warnings?: string[];
  /** The same warnings with a stable key each (`memory:<class>`, `cpu:<class>`), in the same order. Absent from an older daemon. */
  warning_items?: Array<{ key: string; text: string }>;
  classes?: ClassCap[];
  reserve?: {
    /** `max` or `p75`: what admission debits. */
    mode?: string;
    /** The configured value when it is neither (the daemon ran as `max`). */
    unknown?: string | null;
    in_force?: string;
    cooling?: boolean;
    /** Memory squeeze: freezing is allowed until VM memory recovers. */
    squeeze?: boolean;
    freeze_armed?: boolean;
    last_trip?: ReserveTrip | null;
    next_rearm_ms?: number | null;
    armed_ms?: number;
    next_cooloff_ms?: number;
    trips_7d?: number;
    trips?: ReserveTrip[];
  } | null;
  vm?: VmView | null;
  /** ADR-0053: classes OOM-killed recently and held at the escalation threshold, most OOMs first. */
  oom_held?: OomHeld[] | null;
}

/** A class faber stopped growing (ADR-0053): `label` is `repo · workflow · job`, GitHub's text. */
export interface OomHeld {
  key: string;
  label: string;
  container?: 'runner' | 'dind' | string;
  cap_mib: number;
  reservation_mib?: number;
  /** The caps a pin needs to stop the OOMs; absent from a daemon before the popover's Pin. */
  pin_runner_mib?: number;
  pin_dind_mib?: number;
  ooms?: number;
  runs?: number;
}

export interface UpdateView {
  current?: string;
  enabled?: boolean;
  available?: string | null;
  /** Why this release is the candidate, when the daemon says. */
  candidate_reason?: string | null;
  install_in_secs?: number | null;
  install_after_ms?: number | null;
  /** `phase`: disabled, idle, checking, downloading, waiting_idle, draining, deferred or failed. */
  phase?: { phase?: string; version?: string; since_ms?: number; reason?: string } | null;
  last_check_ms?: number | null;
  last_error?: string | null;
  quarantined?: string[];
  rolled_back_from?: string | null;
  /** `[update] pin`: the only version this host takes. */
  pin?: string | null;
  /** When a failed update tries again. */
  next_attempt_ms?: number | null;
  self_check?: { status?: string; at_ms?: number; detail?: string | null; version?: string; attempts?: number } | null;
  supervisor_restart?: { status?: string; detail?: string; at_ms?: number } | null;
}

/**
 * One of the mirror's attention rows, as the daemon words it. Names, images, commands and errors
 * are fields, never inside `detail`, so they can be set in mono.
 */
export interface MirrorRow {
  key: string;
  severity: 'bad' | 'warn';
  title: string;
  detail: string;
  since_ms?: number;
  image?: string;
  command?: string;
  error?: string;
  /** The runners an outage affected: the popover counts them, the dashboard names them. */
  affected?: string[];
}

/**
 * One repo's counters for one ecosystem since the mirror service started (ADR-0047 § 10). For LFS a
 * request is an object; for npm a packument or a tarball. npm's `repo` may be `(unattributed)`.
 */
export interface MirrorRepoCounters {
  repo: string;
  hits: number;
  misses: number;
  bytes_from_cache: number;
  bytes_from_upstream: number;
  passed_through: number;
}

/** One ecosystem through the mirror: its share of the store, the store's one cap, and its repos. */
export interface MirrorEcosystemView {
  fill: 'relaxed' | 'off';
  objects: number;
  bytes: number;
  max_bytes: number;
  repos: MirrorRepoCounters[];
}

/** `/api/state` → `mirror` (docs/mirrors.md). */
export interface MirrorView {
  state?: string;
  max_size_gib?: number;
  attention?: MirrorRow[];
  /** Present with `[mirror.lfs]` on. */
  lfs?: MirrorEcosystemView | null;
  /** Present with `[mirror.npm]` on. */
  npm?: MirrorEcosystemView | null;
}

/** A profile as the daemon summarises it (`ProfileView`). */
export interface ProfileView {
  name: string;
  labels: string[];
  backend: string;
  /** `granted`, `denied` or `unstated`; null for a backend with no dind. */
  dind?: string | null;
  /** A granted (or unstated) dind on Linux with no account wall around it (ADR-0025). */
  needs_wall?: boolean;
  runner_version?: string | null;
}

/** The trust boundary in force (issue #42), `gate::Boundary`. */
export interface GateView {
  allowed_events?: string[];
  /** Events this host narrowed away. */
  excluded_events?: string[];
  /** Events never turned on (`repository_dispatch`, `release`). */
  opt_in_events?: string[];
  allow_public_repos?: boolean;
  allow_fork_prs?: boolean;
  /** `record` or `enforce` (ADR-0030). */
  on_bypass?: string;
}

/** Totals since faber started (`Counts` in src/reconcile.rs). */
export interface Counts {
  provisioned?: number;
  completed?: number;
  faults?: number;
  refused?: number;
  retried?: number;
  dind_provided?: number;
  dind_withheld?: number;
  dind_refused_total?: number;
  service_refused_total?: number;
}

/** One decision in the feed (`Decision`). */
export interface Decision {
  ts_ms: number;
  repo: string;
  job_id: number;
  event: string;
  actor: string;
  verdict: string;
  profile: string;
}

/** A queued job the gate refuses right now (`RefusedJobView`, issue #95). */
export interface RefusedJob {
  repo: string;
  job_id: number;
  run_id: number;
  job_name: string;
  workflow: string;
  event: string;
  /** E.g. `refuse:event:repository_dispatch`. */
  verdict: string;
  since_ms: number;
  labels?: string[];
}

/** A job that asked for `dind` on a profile that grants none (`DindRefusedView`). */
export interface DindRefused {
  repo: string;
  run_id: number;
  job_id: number;
  job_name: string;
  profile: string;
  reason: string;
  at_ms: number;
}

/** A job that declared a service this host does not provide (`ServiceRefusedView`, ADR-0046). */
export interface ServiceRefused {
  repo: string;
  run_id: number;
  job_id: number;
  job_name: string;
  profile: string;
  label: string;
  reason: string;
  at_ms: number;
}

/** A job the gate refuses that ran anyway (`BypassView`, ADR-0030). */
export interface Bypass {
  ts_ms: number;
  repo: string;
  run_id: number;
  job_id: number;
  job_name: string;
  workflow: string;
  runner: string;
  minted_for: number;
  /** `trust` or `policy`. */
  class: string;
  verdict: string;
}

/** A recent fault in faber itself (`FaultEvent`). */
export interface Fault {
  ts_ms: number;
  repo: string;
  scope: string;
  /** `recovered` and `hold_cleared` end a fault; anything else is one. */
  action: string;
  cause: string;
}

/** A runner-image drift check (`DriftEvent`, ADR-0001). */
export interface Drift {
  ts_ms: number;
  repo: string;
  run_id: number;
  job_id: number;
  status: string;
  baked?: string;
  pinned?: string;
  mode?: string;
  action: string;
}

/** An auto-retry action (`RetryEvent`, ADR-0003). */
export interface Retry {
  ts_ms: number;
  repo: string;
  run_id: number;
  attempt: number;
  jobs: number;
  mode: string;
}

/** Whether faber holds off idle sleep for its running jobs (`AwakeView`, issue #90). */
export interface AwakeView {
  enabled?: boolean;
  /** `[host] prevent_sleep`: off, while_busy or always. */
  mode?: 'off' | 'while_busy' | 'always';
  supported?: boolean;
  held?: boolean;
  held_since_ms?: number | null;
  release_at_ms?: number | null;
  error?: string | null;
}

/** A permission a served repository lacks, and what that costs (`MissingPermission`). */
export interface MissingPermission {
  repo: string;
  permission: string;
  feature: string;
  fix: string;
}

/** The GitHub credential (`CredentialView`, ADR-0027). Never the token. */
export interface CredentialView {
  /** `missing`, `stored`, `invalid` or `expiring`. */
  status?: string;
  owner?: string | null;
  expires_at_ms?: number | null;
  stored_at_ms?: number | null;
  reason?: string | null;
  missing_permissions?: MissingPermission[];
  unverified_permissions?: string[];
  mismatches?: { granted_not_opted_in?: string[]; opted_in_not_granted?: string[] };
  file_warning?: string | null;
}

/** A git mirror's size and refresh state (`git_mirror::MirrorView`, ADR-0035 § 5). */
export interface GitMirror {
  repo: string;
  ready?: boolean;
  refreshing?: boolean;
  size_kib?: number | null;
  last_refresh_ms?: number | null;
  last_attempt_ms?: number | null;
  last_error?: string | null;
  /** A volume faber did not label holds the name: only the operator can clear it. */
  blocked?: string | null;
}

export interface DaemonState {
  /** The org or user whose repos this daemon serves. */
  owner?: string;
  /** The repo topic that opts a repo in. */
  topic?: string;
  /** This daemon's instance id (#94), the suffix of every runner it mints. */
  instance?: string;
  profiles?: ProfileView[];
  /** Docker jobs admitted now: a readout, not a knob. */
  concurrency?: number;
  uptime_s?: number;
  audit_path?: string;
  counts?: Counts;
  paused?: boolean;
  draining?: boolean;
  healthy?: boolean;
  last_error?: string | null;
  inflight?: Inflight[];
  outcomes?: Outcome[];
  peers?: Peer[];
  budget?: Budget;
  update?: UpdateView;
  missing_images?: Array<{ profile: string; image: string; since_ms?: number; cause?: string }>;
  /** Profile images that would be pulled from a Docker Hub user namespace only because `pull_unqualified_images` is unstated (#156). */
  unqualified_images?: Array<{ profile: string; image: string; message: string }>;
  /** The pull-through mirror (docs/mirrors.md); `null` with `[mirror]` absent. The daemon words its rows. */
  mirror?: MirrorView | null;
  runner_retirements?: Array<{
    profile: string;
    days_remaining: number;
    urgent?: boolean;
    runner_version?: string;
    cutoff?: string;
    image?: string | null;
    job_id?: number;
    seen_ms?: number;
    /** The daemon's sentence for the notice. */
    message?: string;
  }>;
  /** Set by faber itself after an enforced trust bypass; only the operator clears one. */
  repo_holds?: Array<{ repo: string; reason?: string; since_ms?: number; by?: string }>;
  /** Oldest first. The popover reads the repo, workflow and verdict; the dashboard reads the rest. */
  refused_queued?: Array<Partial<RefusedJob>>;
  refused_queued_overflow?: number;
  dind_refused?: DindRefused[];
  service_refused?: ServiceRefused[];
  /** `[host] dind_requires_label`, resolved: `on`, `off` or `unstated` (ADR-0026). */
  dind_label_rule?: string;
  bypasses?: Bypass[];
  trust_bypasses_7d?: number;
  gate?: GateView | null;
  /** This boot's standing conditions (ADR-0025), plus the live `linux_vm_waiting`. */
  notices?: Array<{ kind: string; text: string }>;
  trust_changes?: Array<{ ts_ms: number; key: string; sentence: string; via: string; route?: string; uid?: number | null }>;
  /** Newest first. */
  recent?: Decision[];
  faults?: Fault[];
  drift?: Drift[];
  retries?: Retry[];
  awake?: AwakeView;
  credential?: CredentialView;
  git_mirrors?: GitMirror[];
  /** The operator's dismissals and trust acknowledgement (ADR-0064). Absent from an older daemon. */
  attention?: AttentionState;
}

/** One dismissed attention row (ADR-0064 § 2): its key and the identity it had when dismissed. */
export interface Dismissal {
  key: string;
  ident: string;
  at_ms: number;
}

/** What the daemon keeps for every surface (ADR-0064 § 3). */
export interface AttentionState {
  dismissed?: Dismissal[];
  /** Every trust change at or before this time is acknowledged (§ 4); null while none is. */
  trust_acked_ms?: number | null;
}

/** The app's own update (ADR-0034), as `updater::AppUpdateView` sends it. */
export interface AppUpdate {
  updated_to?: string | null;
  rolled_back?: string | null;
  status?: string;
  version?: string | null;
  detail?: string | null;
  floor_warning?: string | null;
  daemon_maturity_secs?: number;
  floor_secs?: number;
}

/** What the app pushes on each `snapshot` event and returns from `get_snapshot`. */
export interface Snapshot {
  online: boolean;
  state?: DaemonState | null;
  base_url?: string;
  error?: string | null;
  app_version?: string;
  app_update?: AppUpdate | null;
  trust_shrank?: boolean;
}

/** Calls a Tauri command; in a story, a stub. */
export type Invoke = (cmd: string, args?: Record<string, unknown>) => Promise<unknown>;
