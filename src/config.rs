//! Optional config file: routing profiles + daemon settings.
//!
//! faber starts with no config, but serves nothing: the CLI flags used to
//! synthesize a native profile, and that backend is gone (ADR-0026, ADR-0052),
//! so a host with no file has no profiles until one is saved. A config file at
//! `~/Library/Application Support/faber/config.toml` (or `--config <path>`)
//! unlocks multiple profiles and the Docker backend:
//!
//! ```toml
//! topic = "faber"                     # GitHub repo topic marking repos faber services
//! poll_interval_secs = 20
//! sweep_interval_secs = 900           # janitor cadence for leaked docker resources (0 disables)
//! runner_timeout_secs = 10800         # kill a docker runner after 3h (backstop; 0 disables)
//! provisioning = "per-job"            # per-job (default) | demand — see ADR-0004.
//!                                     # "demand" lets several faber instances share
//!                                     # one repo pool (mint only the shortfall vs.
//!                                     # runners already registered on GitHub).
//! trusted_actors = []                 # empty = provenance trust
//! denied_actors  = ["dependabot[bot]"]
//!
//! # Autolimiter — memory-budget admission for Docker jobs (all keys optional).
//! # See docs/autolimiter.md. Concurrency is emergent from the budget.
//! # `native_concurrency` is still accepted and shown, but limits nothing: the
//! # native backend it counted is gone (ADR-0052).
//! [autolimit]
//! reserve_mib = 5120                  # budget = VM MemTotal − reserve (with keep_memory_gib
//!                                     # above 0, the floor under a measured co-tenant peak)
//! headroom_mib = 1536                 # Gate 2 slack
//! profiling_cap_mib = 4096            # per-container ceiling for an unlearned class
//! margin = 1.4                        # learned cap = rolling-max(peaks) × margin
//! native_concurrency = 2
//! # hard_ceiling = "auto"             # "auto" (faber picks — 4 today) | "off" (default) | N
//! # reserve = "max"                   # "max" (default) | "p75": debit the typical peak, freeze
//!                                     # the newest job when memory runs critically short, and
//!                                     # fall back to max on any sign of overrun (ADR-0033)
//!
//! # priority_repos = ["acme/tooling"]  # owner/repo, case-insensitive: their parked claims queue
//!                                     # ahead of every other repo's (ADR-0036). Absent = strict FIFO
//!
//! # CPU-aware admission (ADR-0019, all keys optional). Absent `cpu` is
//! # "unstated", which behaves as "off" but is warned about at boot — see
//! # AGENTS.md on why a tightening key defaults loud instead of silent.
//! # cpu = "auto"                      # "auto" | "off" (default: unstated)
//! # cpu_overcommit = 1.25             # CPU budget = VM cores × overcommit − reserve_cpus
//! # reserve_cpus = 1.0                # cores held back for Docker itself + dev stacks
//! # profiling_cpus = 4.0              # runner share for an unlearned class; cap on a learned one
//! # cpu_pause_pressure = 40.0         # pause admission at this PSI `some avg60` percentage
//! # cpu_resume_pressure = 25.0        # ...and resume once it falls back to this
//!
//! # Runner-image drift detection (ADR-0001). Absent = surface. A watched repo
//! # may override this via its in-repo `.faber/config.toml`.
//! [playwright_drift]
//! mode = "surface"                    # off | surface | gate  (gate is Phase 2)
//!
//! # On-demand runner-image variants (ADR-0008). Off by default; with it off the
//! # provisioning path is exactly what it was. Turned on, faber builds a
//! # per-version variant of the profile's base image when a job's lockfile pins a
//! # version the base doesn't bake, and keeps it cached while it stays hot.
//! [runner_image]
//! build = "off"                       # off (default) | on-demand
//! keep = 3                            # most-recently-used variants kept
//! idle_ttl_hours = 168                # evict a variant untouched this long
//! build_timeout_secs = 1200           # cap on one build
//! # dockerfile = "/path/to/variant.Dockerfile"   # blank = faber's own template
//!
//! # Operator-owned ceiling on what [runner_image] may ever be asked to build
//! # (ADR-0010 §4). A job's runs-on label only ever names a version its own
//! # lockfile already pins (ADR-0010 §3); this bounds what the operator is
//! # willing to have that turn into on the host, independent of any repo's
//! # lockfile. A tool with no entry here is not provisionable at any version.
//! [needs.playwright]
//! versions = ">=1.55, <1.70"          # range form: every constraint ANDed
//! # versions = "1.61.1, 1.62.1"       # ...or an exact-list form, ORed
//!
//! # Auto-retry of faber-caused failures (ADR-0003). Opt-in: off by default. When
//! # on, faber re-runs jobs its own runner killed/lost (OOM, lifetime-cap, idle
//! # reclaim, unexpected shutdown) — never a genuine step failure.
//! [retry]
//! enabled = false                     # master switch (grants faber Actions re-run)
//! max_attempts = 2                    # highest run_attempt faber creates (1 retry)
//! lookback_secs = 1800                # only re-run runs updated in the last 30 min
//!
//! # faber is the Docker manager on this box, so keeping the daemon usable is its
//! # job and not the operator's (ADR-0007). Turn `revive_docker` off on a host
//! # where something else owns Docker.
//! [host]
//! revive_docker = true                # restart a wedged docker service (Linux hosts only)
//! revive_vms = true                   # restart a wedged VM: a per-job VM stuck before its
//!                                     # runner console, or faber's Linux VM (macOS)
//! revive_after_secs = 600             # unresponsive this long = a wedge, not load
//! revive_cooldown_secs = 1800         # minimum gap between restarts, and how long Docker
//!                                     # must then stay up before they count as working
//! disk_floor_gib = 25                 # reclaim CI images/build cache below this; 0 = off
//! # Start no Docker job, and don't restart Docker, while the Docker volume has less
//! # than this free (ADR-0024). 0 = off. Absent is "unstated": no gate, as before,
//! # but warned about at boot. At most `disk_floor_gib`, so reclamation starts first.
//! disk_admission_floor_gib = 10
//! # What the Docker VM and CI must leave to the machine faber runs on (ADR-0021).
//! # Once stated, CI is planned within the rest, faber warns while the VM is
//! # larger than the rest, and with keep_memory_gib above 0 admission (macOS VM
//! # jobs included) pauses while the machine reports memory pressure or swap grows by
//! # 1 GiB in 5 minutes, and Gate 1 holds back the larger of reserve_mib and the
//! # 24 h peak of what the VM's other tenants use. 0 = no share. Absent is
//! # "unstated": as before, warned at boot.
//! keep_memory_gib = 16                # RAM the Docker VM and CI must leave the machine (example)
//! keep_cpus = 2                       # cores CI must leave idle for the machine (example)
//! # recreatable_projects and recreatable_volumes (ADR-0016) are still read, so an
//! # old config boots, but they do nothing since ADR-0026: faber no longer reclaims
//! # from Docker Desktop. Boot and `faber check-config` warn while either is set.
//! # The block faber carves per-job network subnets from, so its sandboxes stop
//! # competing for the daemon's shared default pools (31 slots for the whole
//! # machine). Empty = let Docker allocate, as before. Pick a private range
//! # nothing on this host or its VPNs already routes.
//! network_pool = "10.63.0.0/16"       # empty (default) = Docker's own pools
//! network_subnet_size = 24            # per-job prefix; /16 + /24 = 256 slots
//! # Delete a cache volume's top-level entries (ADR-0005) that nothing has read,
//! # written or changed for this many days — ADR-0024. 0 = no age limit. The
//! # minimum is 2: the Docker VM mounts its disk relatime, so a read refreshes
//! # atime at most once a day. With neither this nor cache_max_gib set (the
//! # default), faber decides: it deletes nothing on a healthy disk and, under disk
//! # pressure, the least recently used entries (ADR-0055). If either is set, only
//! # the limits that are set apply.
//! # cache_max_age_days = 14
//! # A cache volume holding more than this many GiB loses its least recently used
//! # top-level entries, oldest first, until it fits — ADR-0055. An age limit alone
//! # cannot bound a cache that is in use. 0 = no size limit. An entry used in the
//! # last 2 hours is never deleted for size.
//! # cache_max_gib = 20
//! # Whether faber may pull an absent profile image whose reference names no
//! # registry host (`owner/x`, `ubuntu:24.04`) — #156. Runners start with
//! # --pull=never, so faber's own pull is the only one. Absent is "unstated":
//! # pulled as before, but a Docker Hub user-namespace image (`owner/x`) is warned
//! # about at boot and on the dashboard. false = never pull one; missing at once.
//! # pull_unqualified_images = false
//! # Hold off idle sleep (issue #90). "off" (the default): never. "while_busy":
//! # while any job's runner is up, letting the machine sleep a few minutes after
//! # the last one ends. "always": for as long as the daemon runs, jobs or not.
//! # Idle sleep only: a closed laptop lid still sleeps. macOS only; elsewhere it
//! # does nothing and boot says so. Narrower than `pmset -a sleep 0`
//! # (deploy/keep-awake.sh), which needs sudo and never sleeps at all. Replaces
//! # the old `prevent_sleep_while_busy = true`, still read as "while_busy".
//! prevent_sleep = "off"
//! # Give a Docker daemon only to jobs whose runs-on includes `dind`, on a profile
//! # that grants one (ADR-0026). Absent is "unstated": every job on a granting
//! # profile gets a daemon, as before, warned at boot. The cut sets it true.
//! # dind_requires_label = true
//!
//! # Registry credentials for the images a *job* pulls. HELD: a credential mounted
//! # into a job can be read by the job's code, so faber mounts none, and
//! # `check-config` refuses `enabled = true`. A daemon booting with it on warns and
//! # pulls anonymously. The section returns when faber pulls images for the job.
//! # Every such pull is anonymous meanwhile — metered by Docker Hub against a quota
//! # keyed to this host's public IP. There is deliberately no `password` key.
//! [registry_auth]
//! enabled = false
//! server = "https://index.docker.io/v1/"
//! username = "myaccount"              # blank = take it from ~/.docker/config.json
//! password_env = "FABER_REGISTRY_PASSWORD"
//!
//! # Services a job may declare with a label instead of dind (ADR-0046). No
//! # table, no service: a job asking for one is refused and gets no runner.
//! [services.postgres]                 # faber.postgres-<major>; one shared server per major
//! versions = "16, 17"                 # exact majors, 13 or later (required)
//! image = "postgres:{version}-alpine" # {version} is the major (required)
//! extensions = ["pg_trgm"]            # installed into every job's database
//! memory_mib = 2048                   # reserved while the server runs, and its cap
//! idle_stop_secs = 900                # stop it after this long unused (default 900)
//! [services.redis]                    # faber.redis; one container per job. Takes
//! image = "redis:7-alpine"            # no versions, extensions or idle_stop_secs,
//! memory_mib = 256                    # and neither does [services.mailpit].
//! [services.s3]                       # any other name is a per-job service too: a
//! image = "versity/versitygw:v1.10.0" # job asks with faber.s3, and the name is its
//! args = ["--port", ":7070", "--health", "/health", "posix", "/tmp/vgw"] # host name
//! env = { ROOT_ACCESS_KEY = "faber", ROOT_SECRET_KEY = "{secret}" } # the container's;
//!                                     # {secret} is a fresh random value per job
//! health_cmd = "wget -qO- http://127.0.0.1:7070/health" # the job waits for it (Docker
//!                                     # Engine 25+); without it, it does not wait
//! memory_mib = 256                    # args and health_cmd are an argv: no {secret}
//! [services.s3.job_env]               # what the runner gets (redis and mailpit
//! S3_ENDPOINT = "http://s3:7070"      # default theirs); {secret} is the same value
//! AWS_ACCESS_KEY_ID = "faber"         # env got. Neither map may name PATH, HOME,
//! AWS_SECRET_ACCESS_KEY = "{secret}"  # DOCKER_*... Buckets are the job's to create,
//! AWS_REGION = "us-east-1"            # public ones too (ADR-0062).
//!
//! # The pull-through mirror jobs fetch through (docs/mirrors.md). The section is
//! # the switch: absent, faber makes no Docker call for it at all. Present, faber
//! # runs one `faber-mirror` container (image `faber-mirror:<faber version>`,
//! # built by deploy/build-mirror-image.sh) and profiles with `mirror = true`
//! # are pointed at it while it is healthy. Applies on restart. `[mirror.lfs]`
//! # holds faber's GitHub token on the Docker engine, so like `git_mirror` it is
//! # refused while a docker profile grants dind (docs/git-mirror.md).
//! [mirror]
//! max_size_gib = 20                   # the cache volume's limit (default 20, > 0)
//!
//! # This host's VMs (ADR-0026, ADR-0051), on a Mac set up with `sudo faber
//! # setup`. Every VM's memory comes out of the machine share ([host]). All keys
//! # optional; absent keeps today's layout. Applies on restart.
//! [linux_vm]                          # the Linux VM that serves faber's Docker
//! enabled = true                      # false: docker profiles get no runner
//!                                     # here, and the VM stops once the jobs
//!                                     # left in it finish (idle runners at once,
//!                                     # any at runner_timeout_secs); its disk
//!                                     # is kept
//! cpus = 6
//! memory_mib = 16384
//! [macos_vm]                          # the VMs `macos-vm` profiles run in
//! count = 2                           # 0 to 2 at once (Apple's limit); 0 = none.
//!                                     # Setting it above 0 needs the session
//!                                     # account `sudo faber setup` creates
//! memory_mib = 8192                   # what each takes of the share; no
//!                                     # profile's VM may be larger
//!
//! # The trust boundary this host enforces (issue #42). Every key here NARROWS:
//! # there is nothing in this section that lets faber run something it would
//! # otherwise refuse, except `allow_public_repos`. Omit the whole section for
//! # faber's default boundary.
//! [gate]
//! # Trigger events provisioned for. This is intersected with faber's own
//! # compiled-in ceiling, so listing `pull_request_target` (or any other trigger
//! # whose provenance cannot be trusted) does nothing but earn a startup error.
//! # `repository_dispatch` and `release` are under the ceiling but opt-in: they
//! # are off unless you append them to this list.
//! allowed_events = ["push", "pull_request", "workflow_dispatch", "schedule", "merge_group"]
//! # Service public repos. Default false = a public repo is refused outright,
//! # ahead of every other check. Turning it on weakens nothing else — fork PRs
//! # stay refused and the event ceiling is unchanged — but it does mean strangers
//! # can queue jobs against a repo faber watches.
//! allow_public_repos = false
//! # There is deliberately no `allow_fork_prs`, and writing one is a startup
//! # error rather than an ignored line. Job code reaches a privileged Docker
//! # daemon (issue #43), so accepting a stranger's pull request would be handing
//! # out host root behind a toggle.
//! #
//! # What faber does when a job it refused runs anyway, on a runner it minted for
//! # another job (ADR-0030). "record" (the default) records it; "enforce" also
//! # kills the runner, deletes the repo's cache volume, holds the repo and pauses
//! # its minting while such a job is queued — for a trust bypass only.
//! on_bypass = "record"
//! # Run a workflow_dispatch or repository_dispatch that a job's own token made
//! # (actor github-actions[bot]). Default false = refused, whatever the actor
//! # lists say: it runs whatever code a job pushed with that token, under the
//! # dispatched workflow's labels and secrets (ADR-0059). Turning it on warns at
//! # every boot.
//! allow_job_token_dispatches = false
//!
//! # Each profile leads with the `faber` tag (so a job's `runs-on` self-documents
//! # who provisions it) plus a per-executor label that routes to the backend.
//! [[profile]]
//! name = "macos"
//! labels = ["faber", "macOS", "ARM64"]
//! backend = "macos-vm"        # a per-job macOS VM (ADR-0026). `backend = "native"`
//! image = "macos-27"          # is refused (ADR-0052): it ran job code on the host.
//!
//! [[profile]]
//! name = "linux-docker"
//! labels = ["faber", "docker"]
//! backend = "docker"
//! image = "ghcr.io/owner/my-runner:latest"
//! dind = false                # whether this profile's jobs get a Docker daemon
//!                             # of their own. That daemon is `--privileged` and
//!                             # unauthenticated on the job's network, so granting
//!                             # it is granting host root (issue #43). Say `false`
//!                             # unless the jobs actually run `docker` — most
//!                             # don't. Omitting the key grants it, for configs
//!                             # written before it existed, and warns at startup.
//!                             # Either way the daemon goes only to jobs whose
//!                             # `runs-on` includes `dind` (ADR-0026); a job
//!                             # asking for it here with `false` is refused.
//! env = { PLAYWRIGHT_BROWSERS_PATH = "/opt/ms-playwright" }
//! cache_volume = true         # mount a per-repo build cache at /runner/_cache
//!                             # and export $FABER_CACHE_DIR (ADR-0005). Default
//!                             # false: this is the ONLY state that crosses the
//!                             # job boundary, so it is opted into per profile.
//!                             # The job workspace (_work) stays per-job either
//!                             # way.
//! git_mirror = true           # keep a git mirror of each watched repo that only
//!                             # faber writes, mount it read-only at /runner/_git
//!                             # and export $FABER_GIT_MIRROR (ADR-0035). Default
//!                             # false. The workflow needs one step and
//!                             # `clean: false` on checkout: docs/git-mirror.md.
//!                             # Refused while any docker profile grants
//!                             # dind: the refresh holds faber's token.
//! mirror = true               # point jobs at the pull-through mirror while it is
//!                             # healthy: it joins the job's network as
//!                             # `faber-mirror`, and faber exports $FABER_MIRROR.
//!                             # Default false; needs [mirror] (docs/mirrors.md).
//! drop_grants = ["192.0.2.5/32 5432/tcp"]
//!                             # grants in /etc/faber/grants.toml this profile's
//!                             # jobs must not have (ADR-0026). It only narrows:
//!                             # a grant is made by root, `sudo faber grant add`.
//!                             # Applied at the next `sudo faber grant` change,
//!                             # or at once with `sudo faber grant apply`.
//! ```
//!
//! A top-level `concurrency` key is still accepted (for back-compat) but ignored:
//! the autolimiter supersedes it.
//!
//! Every key here is also editable from the dashboard's Settings page, which
//! reads this file, validates a proposed one ([`FileConfig::validate`]) and writes
//! it back: re-serialised from the form ([`FileConfig::save_to`]), or verbatim
//! from its text view ([`FileConfig::save_text_to`]). See [`crate::settings`] —
//! in particular for why a form save can't preserve the comments above, and what
//! it does about that.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::backend::{Backend, MacosVmConfig, Profile};
use crate::docker::{DindGrant, DockerConfig, Limits, RegistryAuth};
use crate::gate::TrustPolicy;

pub const DEFAULT_TOPIC: &str = "faber";
pub const DEFAULT_CONCURRENCY: usize = 3;
pub const DEFAULT_POLL_SECS: u64 = 20;
pub const DEFAULT_SWEEP_SECS: u64 = 900;

/// Prepended to every config faber writes itself, so nobody is surprised that
/// their comments went missing on the next save.
const TOML_HEADER: &str = "\
# faber config — written by the dashboard's Settings page.
#
# Editing this file by hand still works, but a save from the Settings form
# rewrites it from data: comments and layout are not preserved (a save from its
# config.toml text view keeps them). The file it replaced is kept as
# config.toml.bak.
#
# Most keys take effect on the next daemon restart; the live-tunable autolimiter
# knobs (reserve/headroom/margin/native_concurrency/hard_ceiling) apply at once.

";

// --- autolimiter defaults (see docs/autolimiter.md) ------------------------
/// Held back from the probed VM memory as Docker-Desktop overhead + a growth
/// cushion for co-tenant dev stacks. `budget = MemTotal − reserve`.
pub const DEFAULT_RESERVE_MIB: u64 = 5120;
/// Gate 2 slack: never admit down to the last byte of live-available memory.
pub const DEFAULT_HEADROOM_MIB: u64 = 1536;
/// Anti-flap band: once frozen (available < headroom), don't resume admitting
/// until available climbs back above `headroom + band`.
pub const DEFAULT_BAND_MIB: u64 = 1024;
/// Per-container bounded profiling ceiling for an unlearned class.
pub const DEFAULT_PROFILING_CAP_MIB: u64 = 4096;
/// Learned cap = rolling-max(recent peaks) × margin.
pub const DEFAULT_MARGIN: f64 = 1.4;
/// A learned reservation above this fraction of budget escalates to a warning
/// (held at the profiling ceiling) instead of auto-applying.
pub const DEFAULT_ESCALATION_FRAC: f64 = 0.5;
/// How many recent per-class peaks the rolling window keeps.
pub const DEFAULT_WINDOW: usize = 20;
/// `[autolimit] native_concurrency` when absent. It limits nothing since ADR-0052
/// removed the last native job; the key is kept so the knob still reads and saves.
pub const DEFAULT_NATIVE_CONCURRENCY: usize = 2;
/// Tiny image for the Gate 2 `MemAvailable` probe container.
pub const DEFAULT_PROBE_IMAGE: &str = "busybox";
/// What `hard_ceiling = "auto"` resolves to: faber's own pick of a concurrent
/// Docker job ceiling. A constant today (ADR-0019); `auto` is a setting of its own
/// so a later release can derive the number without another config migration.
pub const AUTO_HARD_CEILING: usize = 4;
/// CPU admission (ADR-0019): reservations may add up to the VM's cores × this.
pub const DEFAULT_CPU_OVERCOMMIT: f64 = 1.25;
/// Cores held back from the CPU budget for Docker Desktop and co-tenant stacks.
pub const DEFAULT_RESERVE_CPUS: f64 = 1.0;
/// Runner-container CPU share (cores) for a class with no learned share yet, and
/// the most a learned share may grow to without an operator pin.
pub const DEFAULT_PROFILING_CPUS: f64 = 4.0;
/// Pause CPU admission while the VM's `/proc/pressure/cpu` `some avg60` is at or
/// above this percentage…
pub const DEFAULT_CPU_PAUSE_PRESSURE: f64 = 40.0;
/// …and resume once it has fallen to this percentage (anti-flap band).
pub const DEFAULT_CPU_RESUME_PRESSURE: f64 = 25.0;
/// Highest `run_attempt` auto-retry will create (ADR-0003): one automatic re-run.
pub const DEFAULT_RETRY_MAX_ATTEMPTS: u64 = 2;
/// Only auto-retry runs updated within this window, so a fresh daemon doesn't
/// re-run historical faber-deaths (ADR-0003). 30 minutes.
pub const DEFAULT_RETRY_LOOKBACK_SECS: u64 = 1800;

fn default_topic() -> String {
    DEFAULT_TOPIC.to_string()
}
fn default_concurrency() -> usize {
    DEFAULT_CONCURRENCY
}
fn default_poll_secs() -> u64 {
    DEFAULT_POLL_SECS
}
fn default_sweep_secs() -> u64 {
    DEFAULT_SWEEP_SECS
}
fn default_denied() -> Vec<String> {
    vec!["dependabot[bot]".to_string()]
}

/// The repository faber's releases are published from (ADR-0031): the
/// default `[update] repo`, and the only one `sudo faber upgrade-cli` reads.
pub const RELEASE_REPO: &str = "rejifald/faber";

fn default_update_repo() -> String {
    RELEASE_REPO.to_string()
}

fn default_update_poll_secs() -> u64 {
    1800
}

fn default_update_escalate_secs() -> u64 {
    6 * 60 * 60
}

fn default_update_probation_secs() -> u64 {
    120
}

fn default_update_maturity_secs() -> u64 {
    24 * 60 * 60
}
fn default_reserve_mib() -> u64 {
    DEFAULT_RESERVE_MIB
}
fn default_headroom_mib() -> u64 {
    DEFAULT_HEADROOM_MIB
}
fn default_band_mib() -> u64 {
    DEFAULT_BAND_MIB
}
fn default_profiling_cap_mib() -> u64 {
    DEFAULT_PROFILING_CAP_MIB
}
fn default_margin() -> f64 {
    DEFAULT_MARGIN
}
fn default_escalation_frac() -> f64 {
    DEFAULT_ESCALATION_FRAC
}
fn default_window() -> usize {
    DEFAULT_WINDOW
}
fn default_native_concurrency() -> usize {
    DEFAULT_NATIVE_CONCURRENCY
}
fn default_retry_max_attempts() -> u64 {
    DEFAULT_RETRY_MAX_ATTEMPTS
}
fn default_retry_lookback_secs() -> u64 {
    DEFAULT_RETRY_LOOKBACK_SECS
}
fn default_probe_image() -> String {
    DEFAULT_PROBE_IMAGE.to_string()
}
fn default_cpu_overcommit() -> f64 {
    DEFAULT_CPU_OVERCOMMIT
}
fn default_reserve_cpus() -> f64 {
    DEFAULT_RESERVE_CPUS
}
fn default_profiling_cpus() -> f64 {
    DEFAULT_PROFILING_CPUS
}
fn default_cpu_pause_pressure() -> f64 {
    DEFAULT_CPU_PAUSE_PRESSURE
}
fn default_cpu_resume_pressure() -> f64 {
    DEFAULT_CPU_RESUME_PRESSURE
}

/// `autolimit.hard_ceiling`: a cap on concurrent Docker jobs, on top of the
/// budget. Written as `"auto"`, `"off"` or a number.
///
/// An absent key is [`HardCeiling::Off`] — what an absent key has always meant —
/// and `Off` is left out when the config is written, so a file that never set a
/// ceiling still says nothing about one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HardCeiling {
    /// faber picks the ceiling: [`AUTO_HARD_CEILING`] for now.
    Auto,
    /// No ceiling: concurrency is whatever the budget admits.
    #[default]
    Off,
    /// An operator-chosen ceiling. Zero parses, and validation rejects it.
    Fixed(usize),
}

impl HardCeiling {
    /// The concurrent-job limit this setting enforces, if any.
    pub fn limit(self) -> Option<usize> {
        match self {
            HardCeiling::Auto => Some(AUTO_HARD_CEILING),
            HardCeiling::Off => None,
            HardCeiling::Fixed(n) => Some(n),
        }
    }

    pub fn is_off(&self) -> bool {
        matches!(self, HardCeiling::Off)
    }
}

impl Serialize for HardCeiling {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            HardCeiling::Auto => s.serialize_str("auto"),
            HardCeiling::Off => s.serialize_str("off"),
            HardCeiling::Fixed(n) => s.serialize_u64(*n as u64),
        }
    }
}

impl<'de> Deserialize<'de> for HardCeiling {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Number(u64),
            Word(String),
        }
        // `null` (the settings form's blank input) means no ceiling, as it did
        // when this field was an `Option<usize>`.
        match Option::<Repr>::deserialize(d)? {
            None => Ok(HardCeiling::Off),
            Some(Repr::Number(n)) => Ok(HardCeiling::Fixed(n as usize)),
            Some(Repr::Word(w)) => match w.as_str() {
                "auto" => Ok(HardCeiling::Auto),
                "off" => Ok(HardCeiling::Off),
                other => Err(serde::de::Error::custom(format!(
                    "hard_ceiling must be \"auto\", \"off\" or a number, not {other:?}"
                ))),
            },
        }
    }
}

/// `autolimit.cpu`: whether admission reserves, enforces and learns CPU as well
/// as memory (ADR-0019).
///
/// Tightens admission, so per AGENTS.md an absent key keeps the old behaviour:
/// [`CpuMode::Unstated`] behaves as [`CpuMode::Off`], but is warned about at boot
/// and shown as its own word, so "an operator chose off" and "nobody has said"
/// stay distinguishable until the default flips in a release-noted change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CpuMode {
    /// Reserve, enforce (`--cpus`) and learn CPU shares; gate on VM CPU pressure.
    Auto,
    /// Memory-only admission, as before ADR-0019. CPU is still measured.
    Off,
    /// The key is absent. Behaves as `Off`.
    #[default]
    Unstated,
}

impl CpuMode {
    /// Whether CPU admission is in force.
    pub fn enabled(self) -> bool {
        matches!(self, CpuMode::Auto)
    }

    pub fn is_unstated(&self) -> bool {
        matches!(self, CpuMode::Unstated)
    }

    /// A short, stable word for logs and the dashboard.
    pub fn as_str(self) -> &'static str {
        match self {
            CpuMode::Auto => "auto",
            CpuMode::Off => "off",
            CpuMode::Unstated => "unstated",
        }
    }
}

impl Serialize for CpuMode {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            // Skipped by the field's `skip_serializing_if`; `null` is only reached
            // when serialized on its own (JSON), which reads back as `Unstated`.
            CpuMode::Unstated => s.serialize_none(),
            other => s.serialize_str(other.as_str()),
        }
    }
}

impl<'de> Deserialize<'de> for CpuMode {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match Option::<String>::deserialize(d)?.as_deref() {
            None => Ok(CpuMode::Unstated),
            Some("auto") => Ok(CpuMode::Auto),
            Some("off") => Ok(CpuMode::Off),
            Some(other) => Err(serde::de::Error::custom(format!(
                "cpu must be \"auto\" or \"off\", not {other:?}"
            ))),
        }
    }
}

/// `autolimit.reserve`: what a Docker admission debits (ADR-0033 §2).
///
/// Two values only. Absent and `"max"` are today's behaviour — the ledger debits
/// each class's cap — so a host that did not ask is unchanged. `"p75"` debits the
/// class's p75 reservation instead, with the memory freeze and the fallback to max
/// behind it ([`crate::reserve`]). Any other value is kept as written so that
/// `faber check-config` and the settings save can reject it ([`FileConfig::validate`]),
/// while a daemon booting on it warns and runs as `"max"` (AGENTS.md: never refuse
/// to boot on a config the release did not write).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ReserveSetting {
    /// The key is absent. Behaves as `Max`, and says nothing at boot: `"max"` is
    /// the old behaviour, not a tightening.
    #[default]
    Unstated,
    Max,
    P75,
    /// A value this release does not know. Runs as `Max`.
    Unknown(String),
}

impl ReserveSetting {
    /// The mode the limiter runs: an unknown value is `"max"`.
    pub fn mode(&self) -> crate::reserve::ReserveMode {
        match self {
            ReserveSetting::P75 => crate::reserve::ReserveMode::P75,
            _ => crate::reserve::ReserveMode::Max,
        }
    }

    pub fn is_unstated(&self) -> bool {
        matches!(self, ReserveSetting::Unstated)
    }

    /// The value as written, when it is one this release does not know.
    pub fn unknown(&self) -> Option<&str> {
        match self {
            ReserveSetting::Unknown(v) => Some(v),
            _ => None,
        }
    }
}

impl Serialize for ReserveSetting {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        match self {
            // Skipped by the field's `skip_serializing_if`; `null` only when
            // serialized on its own, which reads back as `Unstated`.
            ReserveSetting::Unstated => s.serialize_none(),
            ReserveSetting::Max => s.serialize_str("max"),
            ReserveSetting::P75 => s.serialize_str("p75"),
            ReserveSetting::Unknown(v) => s.serialize_str(v),
        }
    }
}

impl<'de> Deserialize<'de> for ReserveSetting {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(match Option::<String>::deserialize(d)?.as_deref() {
            None => ReserveSetting::Unstated,
            Some("max") => ReserveSetting::Max,
            Some("p75") => ReserveSetting::P75,
            Some(other) => ReserveSetting::Unknown(other.to_string()),
        })
    }
}

/// The CPU half of the autolimiter's runtime config (ADR-0019).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuConfig {
    pub mode: CpuMode,
    /// CPU budget = VM cores × overcommit − `reserve_cpus`.
    pub overcommit: f64,
    pub reserve_cpus: f64,
    /// Unlearned runner share, and the ceiling on a learned one (cores).
    pub profiling_cpus: f64,
    /// PSI `some avg60` percentage at which CPU admission pauses…
    pub pause_pressure: f64,
    /// …and the percentage it must fall to before admission resumes.
    pub resume_pressure: f64,
}

impl Default for CpuConfig {
    fn default() -> Self {
        Self {
            mode: CpuMode::Unstated,
            overcommit: DEFAULT_CPU_OVERCOMMIT,
            reserve_cpus: DEFAULT_RESERVE_CPUS,
            profiling_cpus: DEFAULT_PROFILING_CPUS,
            pause_pressure: DEFAULT_CPU_PAUSE_PRESSURE,
            resume_pressure: DEFAULT_CPU_RESUME_PRESSURE,
        }
    }
}

/// The autolimiter's settled runtime tuning — file defaults with any machine-local
/// [`Overrides`] layered on. Held by the limiter; the live-tunable fields
/// (`reserve_mib`, `headroom_mib`, `margin`, `native_concurrency`,
/// `hard_ceiling`) can change at runtime through the control panel.
#[derive(Debug, Clone)]
pub struct AutoLimitConfig {
    pub reserve_mib: u64,
    pub headroom_mib: u64,
    pub band_mib: u64,
    pub profiling_cap_mib: u64,
    pub margin: f64,
    pub escalation_frac: f64,
    pub window: usize,
    /// Stored and reported so the knob still reads back; limits nothing since
    /// ADR-0052 removed the native jobs it counted.
    pub native_concurrency: usize,
    pub probe_image: String,
    /// Hard ceiling on concurrent Docker jobs, on top of the budget. Default off.
    pub hard_ceiling: HardCeiling,
    /// CPU admission (ADR-0019). Restart-applied, not live-tunable.
    pub cpu: CpuConfig,
    /// What admission debits (ADR-0033 §2). Restart-applied, like `cpu`.
    pub reserve: ReserveSetting,
    /// Repos whose parked claims queue ahead of every other repo's (ADR-0036).
    /// Host config only. Restart-applied, like `cpu`.
    pub priority_repos: Vec<String>,
}

/// The `[autolimit]` config block. Every field defaults, so the whole section is
/// optional and an absent one yields faber's tuned defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoLimitFile {
    #[serde(default = "default_reserve_mib")]
    pub reserve_mib: u64,
    #[serde(default = "default_headroom_mib")]
    pub headroom_mib: u64,
    #[serde(default = "default_band_mib")]
    pub band_mib: u64,
    #[serde(default = "default_profiling_cap_mib")]
    pub profiling_cap_mib: u64,
    #[serde(default = "default_margin")]
    pub margin: f64,
    #[serde(default = "default_escalation_frac")]
    pub escalation_frac: f64,
    #[serde(default = "default_window")]
    pub window: usize,
    /// Still parsed and written, so a file or the knob that sets it loads; it
    /// limits nothing since ADR-0052 removed the native jobs it counted.
    #[serde(default = "default_native_concurrency")]
    pub native_concurrency: usize,
    #[serde(default = "default_probe_image")]
    pub probe_image: String,
    /// `"auto"`, `"off"` or a number. Absent = off; `off` is skipped when writing,
    /// so a file that never set a ceiling keeps saying nothing about one.
    #[serde(default, skip_serializing_if = "HardCeiling::is_off")]
    pub hard_ceiling: HardCeiling,
    /// `"auto"` or `"off"` (ADR-0019). Absent = unstated, which behaves as off.
    #[serde(default, skip_serializing_if = "CpuMode::is_unstated")]
    pub cpu: CpuMode,
    #[serde(default = "default_cpu_overcommit")]
    pub cpu_overcommit: f64,
    #[serde(default = "default_reserve_cpus")]
    pub reserve_cpus: f64,
    #[serde(default = "default_profiling_cpus")]
    pub profiling_cpus: f64,
    #[serde(default = "default_cpu_pause_pressure")]
    pub cpu_pause_pressure: f64,
    #[serde(default = "default_cpu_resume_pressure")]
    pub cpu_resume_pressure: f64,
    /// `"max"` or `"p75"` (ADR-0033 §2). Absent = `"max"`, and skipped when
    /// writing, so a file that never set it keeps saying nothing about it.
    #[serde(default, skip_serializing_if = "ReserveSetting::is_unstated")]
    pub reserve: ReserveSetting,
    /// `owner/repo` entries, matched case-insensitively (ADR-0036). A claim from
    /// one of these repos queues ahead of every other repo's. Absent or empty =
    /// today's strict first-come-first-served, and skipped when writing. It lives
    /// here and not in `.faber/config.toml`: a repo must never raise its own
    /// priority.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub priority_repos: Vec<String>,
}

impl Default for AutoLimitFile {
    fn default() -> Self {
        Self {
            reserve_mib: DEFAULT_RESERVE_MIB,
            headroom_mib: DEFAULT_HEADROOM_MIB,
            band_mib: DEFAULT_BAND_MIB,
            profiling_cap_mib: DEFAULT_PROFILING_CAP_MIB,
            margin: DEFAULT_MARGIN,
            escalation_frac: DEFAULT_ESCALATION_FRAC,
            window: DEFAULT_WINDOW,
            native_concurrency: DEFAULT_NATIVE_CONCURRENCY,
            probe_image: DEFAULT_PROBE_IMAGE.to_string(),
            hard_ceiling: HardCeiling::Off,
            cpu: CpuMode::Unstated,
            cpu_overcommit: DEFAULT_CPU_OVERCOMMIT,
            reserve_cpus: DEFAULT_RESERVE_CPUS,
            profiling_cpus: DEFAULT_PROFILING_CPUS,
            cpu_pause_pressure: DEFAULT_CPU_PAUSE_PRESSURE,
            cpu_resume_pressure: DEFAULT_CPU_RESUME_PRESSURE,
            reserve: ReserveSetting::Unstated,
            priority_repos: Vec::new(),
        }
    }
}

impl AutoLimitFile {
    /// Resolve the runtime config, layering machine-local overrides over the file
    /// defaults for the live-tunable fields.
    pub fn resolve(&self, ov: &Overrides) -> AutoLimitConfig {
        AutoLimitConfig {
            reserve_mib: ov.reserve_mib.unwrap_or(self.reserve_mib),
            headroom_mib: ov.headroom_mib.unwrap_or(self.headroom_mib),
            band_mib: self.band_mib,
            profiling_cap_mib: self.profiling_cap_mib,
            margin: ov.margin.unwrap_or(self.margin),
            escalation_frac: self.escalation_frac,
            window: self.window,
            native_concurrency: ov.native_concurrency.unwrap_or(self.native_concurrency),
            probe_image: self.probe_image.clone(),
            hard_ceiling: ov.hard_ceiling_setting().unwrap_or(self.hard_ceiling),
            cpu: CpuConfig {
                mode: self.cpu,
                overcommit: self.cpu_overcommit,
                reserve_cpus: self.reserve_cpus,
                profiling_cpus: self.profiling_cpus,
                pause_pressure: self.cpu_pause_pressure,
                resume_pressure: self.cpu_resume_pressure,
            },
            reserve: self.reserve.clone(),
            priority_repos: self.priority_repos.clone(),
        }
    }
}

/// Fleet autoupdate policy (ADR-0006).
///
/// Off unless a host opts in, and deliberately so: installing faber must not
/// also mean granting it permission to replace its own binary and restart
/// itself. The builder Mac — the one a release is cut on — leaves this false and
/// is installed by `deploy/release.sh` as part of cutting.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdatePolicy {
    /// Whether this host installs releases on its own.
    #[serde(default)]
    pub enabled: bool,
    /// `owner/repo` the releases are read from. A fork tracks its own.
    #[serde(default = "default_update_repo")]
    pub repo: String,
    /// Seconds between release checks. The check is a conditional GET, so a
    /// short interval mostly costs 304s — but there is nothing to gain from
    /// noticing a release faster than the host can go idle to install it.
    #[serde(default = "default_update_poll_secs")]
    pub poll_interval_secs: u64,
    /// How long a staged update waits for a *natural* idle window before it
    /// escalates to refusing new jobs and draining. The wait costs nothing; the
    /// escalation costs one stalled intake, so this trades convergence speed
    /// against CI throughput. `0` waits for idle forever.
    #[serde(default = "default_update_escalate_secs")]
    pub escalate_after_secs: u64,
    /// How long the freshly-swapped binary must stay up before its boot marker
    /// is cleared. Until then a second boot into the same version reads as a
    /// crash-loop and rolls back — so this must comfortably exceed the time it
    /// takes to bind the dashboard and complete a reconcile cycle.
    #[serde(default = "default_update_probation_secs")]
    pub probation_secs: u64,
    /// Hold this host at an exact version (`"0.2.0"`). The manual way to keep
    /// one machine back while a release proves itself on another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<String>,
    /// How long a release waits before it installs (ADR-0031, #99): the later of
    /// its `published_at` and the moment this host first saw it as its
    /// candidate, plus this. Absent means 24 h, never zero; under
    /// [`MIN_MATURITY_SECS`] is refused by `check-config` and the settings save,
    /// and falls back to the default at boot ([`Self::effective_maturity_secs`]).
    ///
    /// Lowering it is trust-widening (ADR-0028): a shorter wait shrinks the
    /// window in which a release the owner did not make can be caught. No
    /// settings-level trust marking exists for `[update]` keys yet, so this
    /// comment is the only marker; a canary host may run a shorter, non-zero wait.
    #[serde(default = "default_update_maturity_secs")]
    pub maturity_secs: u64,
}

/// Floor for `[update] maturity_secs` (one hour).
pub const MIN_MATURITY_SECS: u64 = 3600;

impl UpdatePolicy {
    /// The wait actually applied: a configured value under the floor (which
    /// only reaches a host by a route that skipped validation) falls back to the
    /// 24 h default, never to zero.
    pub fn effective_maturity_secs(&self) -> u64 {
        if self.maturity_secs < MIN_MATURITY_SECS {
            default_update_maturity_secs()
        } else {
            self.maturity_secs
        }
    }
}

impl Default for UpdatePolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            repo: default_update_repo(),
            poll_interval_secs: default_update_poll_secs(),
            escalate_after_secs: default_update_escalate_secs(),
            probation_secs: default_update_probation_secs(),
            pin: None,
            maturity_secs: default_update_maturity_secs(),
        }
    }
}

/// The parsed config file.
///
/// **Field order is load-bearing for serialization.** TOML demands that every
/// plain value precede the tables that follow it (a key written after a `[table]`
/// header would land *inside* that table), so the scalars come first, then the
/// single tables, and the `[[profile]]` array of tables last. Deserialization
/// doesn't care about order, so an existing hand-written file is unaffected.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default = "default_topic")]
    pub topic: String,
    /// Superseded by the autolimiter (concurrency is now emergent from the memory
    /// budget). Still parsed so an existing config file doesn't error under
    /// `deny_unknown_fields`, but ignored.
    /// Never written back: a file faber rewrites shouldn't keep advertising a key
    /// faber ignores.
    #[serde(default = "default_concurrency", skip_serializing)]
    pub concurrency: usize,
    #[serde(default = "default_poll_secs")]
    pub poll_interval_secs: u64,
    /// Seconds between [`crate::sweep`] passes that reap leaked Docker
    /// resources and prune dangling anonymous volumes. 0 disables the janitor.
    #[serde(default = "default_sweep_secs")]
    pub sweep_interval_secs: u64,
    /// Hard cap (seconds) on a single Docker runner container's lifetime, after
    /// which faber kills it — a backstop against a surplus/idle or hung runner
    /// pinning the provision task forever. Absent = [`crate::docker::DEFAULT_RUNNER_TIMEOUT`];
    /// `0` disables the cap. Set it a comfortable margin above your longest job.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runner_timeout_secs: Option<u64>,
    /// Provisioning strategy (ADR-0004). Absent = `per-job` (single-instance
    /// behavior). Set `provisioning = "demand"` to run several faber instances
    /// against the same repos as one shared pool without extra infrastructure.
    #[serde(default)]
    pub provisioning: ProvisioningMode,
    #[serde(default)]
    pub trusted_actors: Vec<String>,
    #[serde(default = "default_denied")]
    pub denied_actors: Vec<String>,
    /// Autolimiter tuning. Absent = defaults (see [`AutoLimitFile`]).
    #[serde(default)]
    pub autolimit: AutoLimitFile,
    /// Global runner-image drift policy (ADR-0001). Absent = surface, no auto-PR.
    /// A watched repo may override it with a strict in-repo [`RepoConfig`].
    #[serde(default)]
    pub playwright_drift: DriftPolicy,
    /// Auto-retry of faber-caused failures (ADR-0003). Absent = disabled (faber
    /// stays a pure provisioner; it re-runs nothing).
    #[serde(default)]
    pub retry: RetryPolicy,
    /// What faber does with a claim that is likely wasted (ADR-0063). Absent = every
    /// signal off, and nothing is read. Never written back while every signal is off.
    #[serde(default, skip_serializing_if = "WastePolicy::is_off")]
    pub waste: WastePolicy,
    /// Host custody of the Docker daemon (ADR-0007). Absent = the defaults, so
    /// every config file that predates this section keeps parsing.
    #[serde(default)]
    pub host: HostPolicy,
    /// Registry credentials for the images a job pulls. Held: parsed, refused on
    /// by [`Self::validate`], warned of at boot ([`registry_auth_held_warning`]),
    /// and never carried into a profile. Absent = disabled, faber's historical
    /// behavior: anonymous pulls against an IP-keyed quota. See [`RegistryAuth`].
    #[serde(default)]
    pub registry_auth: RegistryAuth,
    /// Fleet autoupdate (ADR-0006). Absent = disabled: faber replaces its own
    /// binary only where a host has opted in.
    #[serde(default)]
    pub update: UpdatePolicy,
    /// On-demand runner-image variants (ADR-0008). Absent = disabled, which is
    /// byte-for-byte faber's historical behavior: one static image per profile.
    #[serde(default)]
    pub runner_image: RunnerImagePolicy,
    /// Operator-owned ceiling on which tool versions faber may ever provision a
    /// runner for (ADR-0010 §4), keyed by tool name (e.g. `"playwright"`).
    /// Absent = an empty map, and an empty map is no ceiling at all
    /// ([`crate::image::VariantCache`] builds any version, ADR-0010 §5). Once
    /// any entry exists the map is authoritative and an unlisted tool fails
    /// closed as [`NeedsVerdict::UnknownTool`].
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub needs: HashMap<String, ToolNeeds>,
    /// The services a job may declare (ADR-0046 §2), keyed by service name
    /// (`postgres`, `redis`, `mailpit`). Absent = an empty map: no service can be
    /// asked for, and the host provisions exactly as before.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub services: BTreeMap<String, ServiceFile>,
    /// The trust boundary this host enforces (issue #42). Absent = faber's
    /// historical boundary: the events in [`crate::gate::DEFAULT_EVENTS`] (not
    /// the opt-in `repository_dispatch` and `release`), private repos only.
    #[serde(default)]
    pub gate: GatePolicy,
    /// The `[linux_vm]` section: the size of the Linux VM that serves faber's
    /// Docker on a set-up macOS host (ADR-0026). Absent keys take the defaults.
    #[serde(default)]
    pub linux_vm: LinuxVmFile,
    /// The `[macos_vm]` section: how many macOS VMs this host runs at once, and
    /// what each takes of the machine share (ADR-0051). Absent keys keep today's
    /// layout: up to [`crate::vm::admit::MACOS_VM_CAP`] at
    /// [`crate::vm::admit::MACOS_VM_MIB`] each, wherever a `macos-vm` profile is
    /// configured.
    #[serde(default, skip_serializing_if = "MacosVmFile::is_empty")]
    pub macos_vm: MacosVmFile,
    /// The `[mirror]` section: the pull-through mirror service jobs fetch
    /// through (ADR-0047). **Absent is
    /// off**, completely: no image check, no container, no Docker call at all.
    /// Present, faber runs one `faber-mirror` container on the runners' engine,
    /// and profiles with `mirror = true` are pointed at it. Never written back
    /// when absent, so a config that never set it saves byte-identical.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror: Option<MirrorFile>,
    /// The routing profiles. Last, because an array of tables swallows every key
    /// written after it.
    #[serde(default, rename = "profile")]
    pub profiles: Vec<FileProfile>,
}

impl Default for FileConfig {
    fn default() -> Self {
        Self {
            topic: default_topic(),
            concurrency: default_concurrency(),
            poll_interval_secs: default_poll_secs(),
            sweep_interval_secs: default_sweep_secs(),
            runner_timeout_secs: None,
            provisioning: ProvisioningMode::default(),
            trusted_actors: Vec::new(),
            denied_actors: default_denied(),
            autolimit: AutoLimitFile::default(),
            playwright_drift: DriftPolicy::default(),
            retry: RetryPolicy::default(),
            waste: WastePolicy::default(),
            host: HostPolicy::default(),
            registry_auth: RegistryAuth::default(),
            update: UpdatePolicy::default(),
            runner_image: RunnerImagePolicy::default(),
            needs: HashMap::new(),
            services: BTreeMap::new(),
            gate: GatePolicy::default(),
            linux_vm: LinuxVmFile::default(),
            macos_vm: MacosVmFile::default(),
            mirror: None,
            profiles: Vec::new(),
        }
    }
}

/// `[linux_vm]`: the Linux VM faber's Docker runs in on a set-up macOS host
/// (ADR-0026, "Size and recovery"). It keeps the memory it takes for as long as it
/// runs, so its size comes out of the machine share (ADR-0021), next to the two
/// macOS VMs. Absent = [`crate::vm::engine::DEFAULT_LINUX_VM_CPUS`] and
/// [`crate::vm::engine::DEFAULT_LINUX_VM_MEMORY_MIB`], and an absent key is not
/// written back. A host with no engine (LEGACY, or Linux) ignores the section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinuxVmFile {
    /// Whether faber runs the Linux VM at all (ADR-0051, "Per-host VM layout").
    /// Absent = on, as before the key existed. `false` keeps it stopped, and
    /// this host's `docker` profiles get no runner ([`ignore_vm_layout`]). A VM
    /// still running docker jobs from the last boot is stopped once they finish:
    /// idle runners are withdrawn and removed at once, and any runner at its
    /// lifetime cap ([`crate::vm::engine::keep_off`]). Its disk and the runner image in it
    /// stay: the engine's disk is never recreated (AGENTS.md). Only a set-up Mac
    /// has this VM; elsewhere the key is a warning ([`vm_layout_warnings`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// vCPUs, 1 to 64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u8>,
    /// Memory in MiB, 1024 to 262144.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u32>,
    /// The ceiling the VM's disk may grow to, GiB (ADR-0061 §1). Absent = 128;
    /// at least the built size, 64, and `64` turns growth off. The disk grows in
    /// 32 GiB steps and never shrinks, so a value below its current size changes
    /// nothing on disk and only stops further growth.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_max_gib: Option<u64>,
}

impl LinuxVmFile {
    /// What is wrong with the section, one sentence each: the supervisor
    /// protocol's own ranges ([`crate::vm::proto`]), so a value that passes here
    /// cannot be refused on the wire.
    pub fn errors(&self) -> Vec<String> {
        use crate::vm::proto::{Cpus, MemoryMib};
        let mut errs = Vec::new();
        if self.cpus.is_some_and(|c| Cpus::try_from(c).is_err()) {
            errs.push("linux_vm.cpus must be 1 to 64".to_string());
        }
        if self
            .memory_mib
            .is_some_and(|m| MemoryMib::try_from(m).is_err())
        {
            errs.push("linux_vm.memory_mib must be 1024 to 262144".to_string());
        }
        if self.disk_max_gib.is_some_and(|g| {
            !(crate::vm::grow::BUILT_GIB..=crate::vm::grow::MAX_CEILING_GIB).contains(&g)
        }) {
            errs.push(format!(
                "linux_vm.disk_max_gib must be {} to {} (the disk's built size turns growth \
                 off)",
                crate::vm::grow::BUILT_GIB,
                crate::vm::grow::MAX_CEILING_GIB
            ));
        }
        errs
    }

    /// Whether the Linux VM runs, where this host has one. Only an explicit
    /// `enabled = false` turns it off.
    pub fn runs(&self) -> bool {
        self.enabled != Some(false)
    }
}

/// `[macos_vm]`: the macOS VMs this host runs for `macos-vm` profiles (ADR-0051,
/// "Per-host VM layout"). An absent key keeps today's layout and is not written
/// back. Turning macOS VMs on with `count` needs the session account only
/// `sudo faber setup` creates ([`crate::platform::layout::FABER_MAC_ACCOUNT`]):
/// `faber check-config` and the settings save refuse it until that exists.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MacosVmFile {
    /// How many macOS VMs may run at once: 0 to
    /// [`crate::vm::admit::MACOS_VM_CAP`], Apple's limit. Absent = that limit.
    /// `0` turns them off, and this host's `macos-vm` profiles get no runner
    /// ([`ignore_vm_layout`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u8>,
    /// What each takes of the machine share (ADR-0021), in MiB, 1024 to 262144.
    /// Absent = [`crate::vm::admit::MACOS_VM_MIB`]. Once set, no `macos-vm`
    /// profile may give its VM more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u32>,
}

impl MacosVmFile {
    fn is_empty(&self) -> bool {
        self == &Self::default()
    }

    /// What is wrong with the section, one sentence each.
    pub fn errors(&self) -> Vec<String> {
        use crate::vm::admit::MACOS_VM_CAP;
        use crate::vm::proto::MemoryMib;
        let mut errs = Vec::new();
        if self.count.is_some_and(|n| usize::from(n) > MACOS_VM_CAP) {
            errs.push(format!(
                "macos_vm.count must be 0 to {MACOS_VM_CAP}: Apple allows {MACOS_VM_CAP} macOS \
                 VMs per Mac"
            ));
        }
        if self
            .memory_mib
            .is_some_and(|m| MemoryMib::try_from(m).is_err())
        {
            errs.push("macos_vm.memory_mib must be 1024 to 262144".to_string());
        }
        errs
    }

    /// Whether the file turns macOS VMs on explicitly, which is what needs the
    /// session account. An absent `count` is today's layout, not a choice.
    pub fn turned_on(&self) -> bool {
        self.count.is_some_and(|n| n > 0)
    }
}

/// The VM layout a config resolves to (ADR-0051, "Per-host VM layout"): whether
/// the Linux VM runs, and the limiter's macOS cap and per-VM charge. The default
/// is the layout before the keys existed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmLayout {
    /// Whether faber's Linux VM runs, where this host has one.
    pub linux_vm: bool,
    /// How many macOS VMs may run at once.
    pub macos_vms: usize,
    /// What each macOS VM takes of the machine share, MiB.
    pub macos_vm_mib: u64,
}

impl Default for VmLayout {
    fn default() -> Self {
        Self {
            linux_vm: true,
            macos_vms: crate::vm::admit::MACOS_VM_CAP,
            macos_vm_mib: crate::vm::admit::MACOS_VM_MIB,
        }
    }
}

impl FileConfig {
    /// The layout `[linux_vm]` and `[macos_vm]` resolve to. A value
    /// `check-config` would refuse is clamped to Apple's limit or left at the
    /// default, never fatal to the boot (AGENTS.md).
    pub fn vm_layout(&self) -> VmLayout {
        let d = VmLayout::default();
        VmLayout {
            linux_vm: self.linux_vm.runs(),
            macos_vms: self
                .macos_vm
                .count
                .map_or(d.macos_vms, |n| usize::from(n).min(d.macos_vms)),
            macos_vm_mib: self
                .macos_vm
                .memory_mib
                .filter(|m| crate::vm::proto::MemoryMib::try_from(*m).is_ok())
                .map_or(d.macos_vm_mib, u64::from),
        }
    }
}

/// `[mirror]`: the pull-through mirror service (ADR-0047,
/// docs/mirrors.md). The section's
/// presence is the switch; its keys only size it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MirrorFile {
    /// The most the mirror's cache volume may hold, in GiB. Above 0. Absent =
    /// [`DEFAULT_MIRROR_MAX_SIZE_GIB`].
    #[serde(default = "default_mirror_max_size_gib")]
    pub max_size_gib: u32,
    /// `[mirror.lfs]`: Git LFS through the mirror (ADR-0047 §4). Absent = LFS is
    /// not redirected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lfs: Option<MirrorLfsFile>,
    /// `[mirror.npm]`: the public npm registry through the mirror (ADR-0047
    /// §5). Absent = the default registry is not redirected.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub npm: Option<MirrorNpmFile>,
    /// The file named `[mirror.lfs]` or `[mirror.npm]` without ever writing
    /// `[mirror]`: TOML then
    /// makes the parent table implicitly, which would turn the mirror on by
    /// accident. Set by [`FileConfig::load_from_str`], refused by [`Self::errors`].
    #[serde(skip)]
    pub implicit: bool,
}

/// `[mirror] max_size_gib` when the key is absent.
pub const DEFAULT_MIRROR_MAX_SIZE_GIB: u32 = 20;

fn default_mirror_max_size_gib() -> u32 {
    DEFAULT_MIRROR_MAX_SIZE_GIB
}

impl Default for MirrorFile {
    fn default() -> Self {
        Self {
            max_size_gib: DEFAULT_MIRROR_MAX_SIZE_GIB,
            lfs: None,
            npm: None,
            implicit: false,
        }
    }
}

impl MirrorFile {
    /// What is wrong with the section, one sentence each.
    pub fn errors(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if self.max_size_gib == 0 {
            errs.push("mirror.max_size_gib must be above 0".to_string());
        }
        if self.implicit {
            errs.push(
                "[mirror.lfs] and [mirror.npm] need a [mirror] section: [mirror] is the switch \
                 that turns the mirror on, so write it explicitly"
                    .to_string(),
            );
        }
        errs
    }
}

/// `[mirror.lfs]` (ADR-0047 §4, §6). In this release `fill` is `"relaxed"`
/// (the default) or `"off"`; `"strict"` is reserved, and `warm_on_push` arrives
/// with warming. Both are parse errors here rather than keys a later release
/// would silently start honouring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MirrorLfsFile {
    pub fill: faber_mirror::Fill,
}

impl Default for MirrorLfsFile {
    fn default() -> Self {
        Self {
            fill: faber_mirror::Fill::Relaxed,
        }
    }
}

impl<'de> Deserialize<'de> for MirrorLfsFile {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            fill: Option<String>,
            warm_on_push: Option<toml::Value>,
        }
        let raw = Raw::deserialize(d)?;
        if raw.warm_on_push.is_some() {
            return Err(D::Error::custom(
                "mirror.lfs.warm_on_push is not in this release: warming LFS objects on push \
                 is a later slice of ADR-0047 (§10)",
            ));
        }
        let fill = match raw.fill.as_deref() {
            None | Some("relaxed") => faber_mirror::Fill::Relaxed,
            Some("off") => faber_mirror::Fill::Off,
            Some("strict") => {
                return Err(D::Error::custom(
                    "mirror.lfs.fill = \"strict\" is reserved and not in this release \
                     (ADR-0047 §3); use \"relaxed\" or \"off\"",
                ))
            }
            Some(other) => {
                return Err(D::Error::custom(format!(
                    "mirror.lfs.fill must be \"relaxed\" or \"off\", not {other:?}"
                )))
            }
        };
        Ok(Self { fill })
    }
}

/// `[mirror.npm]` (ADR-0047 §5, §6): `fill` is `"relaxed"` (the default) or
/// `"off"`. `"strict"` is reserved, and so is an upload setting (§5: none in
/// v1); both, and any other key, are parse errors rather than keys a later
/// release would silently start honouring.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MirrorNpmFile {
    pub fill: faber_mirror::Fill,
}

impl Default for MirrorNpmFile {
    fn default() -> Self {
        Self {
            fill: faber_mirror::Fill::Relaxed,
        }
    }
}

impl<'de> Deserialize<'de> for MirrorNpmFile {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            fill: Option<String>,
        }
        let raw = Raw::deserialize(d)?;
        let fill = match raw.fill.as_deref() {
            None | Some("relaxed") => faber_mirror::Fill::Relaxed,
            Some("off") => faber_mirror::Fill::Off,
            Some("strict") => {
                return Err(D::Error::custom(
                    "mirror.npm.fill = \"strict\" is reserved and not in this release \
                     (ADR-0047 §3); use \"relaxed\" or \"off\"",
                ))
            }
            Some(other) => {
                return Err(D::Error::custom(format!(
                    "mirror.npm.fill must be \"relaxed\" or \"off\", not {other:?}"
                )))
            }
        };
        Ok(Self { fill })
    }
}

/// Whether `text` names `[mirror.lfs]` (or any `mirror.*` table) without a
/// `[mirror]` table of its own: TOML then creates `mirror` implicitly.
pub(crate) fn mirror_table_is_implicit(text: &str) -> bool {
    let Ok(doc) = text.parse::<toml_edit::DocumentMut>() else {
        return false;
    };
    doc.get("mirror")
        .and_then(|i| i.as_table())
        .is_some_and(|t| t.is_implicit())
}

/// The `[gate]` section: the two edges of the trust boundary an operator owns.
///
/// **This lives in faber's own `config.toml` and nowhere else.** A watched repo's
/// in-repo `.faber/config.toml` ([`RepoConfig`]) is reachable from a pull
/// request, so a gate key there would let a fork PR widen the boundary that is
/// meant to be judging it. `RepoConfig`'s strict schema rejects every key but
/// `[playwright]`, and `repo_config_cannot_widen_trust` pins that.
///
/// `deny_unknown_fields` is load-bearing rather than tidy: it is what turns
/// `allow_fork_prs = true` — the key an operator will reach for first — into a
/// startup error naming the fields that do exist, instead of a line silently
/// ignored by a host that then looks like it accepted it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatePolicy {
    /// Trigger events this host provisions for. Absent = [`crate::gate::DEFAULT_EVENTS`],
    /// which is faber's historical behaviour. `repository_dispatch` and `release` are
    /// inside the ceiling ([`crate::gate::MAX_EVENTS`]) but not the default: they
    /// are opt-in, and only an explicit entry here turns them on.
    ///
    /// Only ever a **subset** of that ceiling: an event outside it is a
    /// validation error, not a silent no-op, so `pull_request_target` cannot be
    /// smuggled in and `puhs` cannot silently stop provisioning every push job.
    #[serde(default = "default_allowed_events")]
    pub allowed_events: Vec<String>,
    /// Service public repositories. `false` (the default, and faber's historical
    /// behaviour) refuses one outright, ahead of every other check.
    ///
    /// Turning it on does not weaken anything else — a fork PR is still refused,
    /// and the event ceiling is still the ceiling — but it does mean strangers
    /// can queue jobs against a repo faber watches, so it is opted into
    /// deliberately. There is deliberately **no** `allow_fork_prs`: while job code
    /// reaches a privileged Docker daemon (issue #43), accepting a stranger's PR
    /// is handing out host root, and that is not a toggle.
    #[serde(default)]
    pub allow_public_repos: bool,
    /// What a gate bypass triggers (ADR-0030). Absent = `record`, which only
    /// records it: hosts rely on today's behaviour, so a release must not start
    /// killing their jobs unattended. While `record` sees trust bypasses, faber
    /// warns at every boot and the dashboard says so. A watched repo's
    /// [`RepoConfig`] can no more name this than any other gate key.
    #[serde(default)]
    pub on_bypass: crate::gate::OnBypass,
    /// Run a `workflow_dispatch` or `repository_dispatch` that a job's own
    /// token made (ADR-0059 §3). Absent = `false`: refused, whatever the
    /// actor lists say, because it runs whatever code a job pushed with that
    /// token, under the dispatched workflow's labels and secrets.
    ///
    /// A deliberate exception to "absent keeps the old behaviour" (AGENTS.md):
    /// the owner chose refusal over a warning on existing hosts. While it is
    /// on, faber warns at every boot and on the dashboard.
    #[serde(default)]
    pub allow_job_token_dispatches: bool,
}

/// The boot warning and dashboard notice while `[gate]
/// allow_job_token_dispatches` is on (ADR-0059 §3); `None` while it is off.
pub fn job_token_dispatches_warning(gate: &GatePolicy) -> Option<String> {
    gate.allow_job_token_dispatches.then(|| {
        "gate.allow_job_token_dispatches is on — this host runs workflow_dispatch and \
         repository_dispatch runs that a job's own token made (actor github-actions[bot]). \
         Such a run executes whatever code a job pushed with that token, under the \
         dispatched workflow's labels and secrets (ADR-0059). Remove the key to refuse them."
            .to_string()
    })
}

fn default_allowed_events() -> Vec<String> {
    crate::gate::DEFAULT_EVENTS
        .iter()
        .map(|e| e.to_string())
        .collect()
}

impl Default for GatePolicy {
    fn default() -> Self {
        Self {
            allowed_events: default_allowed_events(),
            allow_public_repos: false,
            on_bypass: crate::gate::OnBypass::default(),
            allow_job_token_dispatches: false,
        }
    }
}

/// One `[[profile]]` block. Same TOML ordering rule as [`FileConfig`]: `env` is a
/// table, so it goes last.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileProfile {
    pub name: String,
    pub labels: Vec<String>,
    pub backend: BackendKind,
    /// The image the job runs on: the runner image for `backend = "docker"`, the
    /// golden macOS image's name in the image store for `backend = "macos-vm"`
    /// (ADR-0026). Required by both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// `macos-vm` only: vCPUs per VM. Absent = [`DEFAULT_VM_CPUS`], and an absent
    /// key is not written back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpus: Option<u8>,
    /// `macos-vm` only: memory per VM in MiB. Absent = [`DEFAULT_VM_MEMORY_MIB`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u32>,
    /// Docker only: grant this profile's jobs a Docker daemon of their own
    /// (docker-in-docker). **This is a privilege grant, not a convenience
    /// toggle** — see [`crate::docker::DindGrant`] for exactly what it hands a
    /// job, which is host root.
    ///
    /// `false` starts no privileged container and puts no daemon on the job's
    /// network at all. Prefer it: most jobs (`cargo test`, `npm test`, a linter)
    /// never invoke `docker`, and for those the grant is pure exposure.
    ///
    /// Absent is deliberately **not** the same as `false`. It means a config
    /// written before this key existed, and behaves as `true` so an unattended
    /// update (ADR-0006) cannot withdraw a capability a host's workflows already
    /// depend on. It is warned about at startup and reported as `unstated`
    /// everywhere the grant is shown, so it stays visible rather than settling in
    /// as a silent default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dind: Option<bool>,
    /// Docker only: override the dind image (defaults to `docker:dind`). Unread
    /// when [`FileProfile::dind`] is `false`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dind_image: Option<String>,
    /// Docker only: mount a per-repo persistent build cache into every runner
    /// container (see [`crate::docker::CACHE_MOUNT`] and ADR-0005). Default
    /// `false` — it is the only state faber lets cross the job boundary, so it is
    /// opted into deliberately, per profile.
    #[serde(default)]
    pub cache_volume: bool,
    /// Docker only: mount the repo's git mirror read-only and export
    /// `FABER_GIT_MIRROR` (ADR-0035). It turns a feature on rather than tightening
    /// anything, so absent is simply `false`, and `false` is not written back, so
    /// a config that never set it saves byte-identical.
    #[serde(default, skip_serializing_if = "is_false")]
    pub git_mirror: bool,
    /// Docker only: point this profile's jobs at the pull-through mirror, when
    /// `[mirror]` is on and the mirror is healthy (ADR-0047).
    /// Like `git_mirror` it turns
    /// a feature on, so absent is `false` and `false` is not written back.
    #[serde(default, skip_serializing_if = "is_false")]
    pub mirror: bool,
    /// Grants this profile's jobs must not have, though `/etc/faber/grants.toml`
    /// gives them (ADR-0026): each `"ADDRESS/PREFIX PORT/PROTO"`, such as
    /// `"192.0.2.5/32 5432/tcp"`, naming one grant exactly.
    ///
    /// Config may drop a grant, never add one (ADR-0025): `_faber` can write this
    /// file, so nothing here may widen what a job reaches. A grant is made only
    /// by root, with `sudo faber grant add`. A drop takes effect when root next
    /// renders the wall: at the next `sudo faber grant` change, or at once with
    /// `sudo faber grant apply`. Absent drops nothing.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drop_grants: Vec<String>,
    /// Docker only: extra environment for the runner container.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// What is wrong with a `macos-vm` profile's own keys, one sentence each. The
/// same grammar and ranges the supervisor protocol enforces on the wire
/// ([`crate::vm::proto`]), so a value that passes here cannot be refused there.
fn macos_vm_errors(p: &FileProfile) -> Vec<String> {
    use crate::vm::proto::{Cpus, ImageName, MemoryMib};
    let mut errs = Vec::new();
    let mut name = |key: &str, v: &Option<String>| match v.as_deref().map(str::trim) {
        None | Some("") => errs.push(format!("a macos-vm profile requires `{key}`")),
        Some(n) => {
            if ImageName::try_from(n).is_err() {
                errs.push(format!(
                    "`{key}` must be a name in the image store (1-64 of a-z, 0-9, `.`, `_`, `-`, \
                     not starting with a symbol), never a path"
                ));
            }
        }
    };
    name("image", &p.image);
    if let Some(c) = p.cpus {
        if Cpus::try_from(c).is_err() {
            errs.push("`cpus` must be 1 to 64".into());
        }
    }
    if let Some(m) = p.memory_mib {
        if MemoryMib::try_from(m).is_err() {
            errs.push("`memory_mib` must be 1024 to 262144".into());
        }
    }
    if !p.env.is_empty() {
        errs.push("`env` is not supported: a VM's job has no container to pass it into".into());
    }
    if p.dind.is_some() || p.dind_image.is_some() || p.cache_volume {
        errs.push("dind, dind_image and cache_volume belong to a docker profile".into());
    }
    errs
}

/// The error for a `backend = "native"` profile, `who` naming it as the other
/// per-profile errors do. One sentence for `check-config`, a settings save and
/// the boot that refuses the file (ADR-0052).
fn native_profile_error(who: &str) -> String {
    format!(
        "{who}: backend = \"native\" is removed (ADR-0026, ADR-0052): it ran job code on the \
         host itself. Set backend = \"macos-vm\" and keep its labels, so workflows do not change"
    )
}

/// `skip_serializing_if` for a key whose `false` is the same as its absence.
fn is_false(b: &bool) -> bool {
    !*b
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    /// Removed by ADR-0026, and a config error since ADR-0052. Parsed only so
    /// `validate_structural` can name the profile and say what to write instead;
    /// [`FileConfig::into_profiles`] refuses it.
    Native,
    Docker,
    /// A per-job macOS VM (ADR-0026).
    #[serde(rename = "macos-vm")]
    MacosVm,
}

impl BackendKind {
    /// A short, stable label for logs / the settings UI.
    pub fn as_str(self) -> &'static str {
        match self {
            BackendKind::Native => "native",
            BackendKind::Docker => "docker",
            BackendKind::MacosVm => "macos-vm",
        }
    }
}

/// vCPUs per macOS VM when a profile does not say (ADR-0026: about 4).
pub const DEFAULT_VM_CPUS: u8 = 4;
/// Memory per macOS VM in MiB when a profile does not say (ADR-0026: 6-8 GiB).
pub const DEFAULT_VM_MEMORY_MIB: u32 = 8192;

/// How faber decides how many runners to mint for the queued jobs it observes
/// (ADR-0004).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProvisioningMode {
    /// One runner per observed queued job, deduped only by *this* process's own
    /// in-memory claim set — faber's original behavior. Correct for a single
    /// instance; two instances watching the same repos would each mint a runner for
    /// the same job (GitHub still runs it once, but the surplus must be idle-reclaimed).
    #[default]
    PerJob,
    /// Mint only the shortfall between queued demand and the runners already
    /// registered on GitHub (idle, or a faber runner still connecting). Because the
    /// runner list is shared, this lets several faber instances split one repo pool
    /// with no extra infrastructure — the coordination point is GitHub itself.
    Demand,
}

impl ProvisioningMode {
    /// A short, stable label for logs / the dashboard.
    pub fn as_str(self) -> &'static str {
        match self {
            ProvisioningMode::PerJob => "per-job",
            ProvisioningMode::Demand => "demand",
        }
    }
}

// --- runner-image drift detection (ADR-0001) -------------------------------

/// How faber reacts to a detected runner-image ⇄ lockfile version mismatch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DriftMode {
    /// Detect but take no GitHub-facing action — an escape hatch for a known
    /// transitional bump window.
    Off,
    /// Post a `failure` commit status on the head SHA (the safe default).
    #[default]
    Surface,
    /// Cancel the run. Reserved for Phase 2; in Phase 1 it detects-and-records
    /// only (no cancel, and — being stronger than surface — posts no status).
    Gate,
}

impl DriftMode {
    /// A short, stable label for the audit log and dashboard.
    pub fn as_str(self) -> &'static str {
        match self {
            DriftMode::Off => "off",
            DriftMode::Surface => "surface",
            DriftMode::Gate => "gate",
        }
    }
}

/// faber's global `[playwright_drift]` default policy. Lives in faber's own
/// config; a watched repo may override the drift knobs (only) via [`RepoConfig`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DriftPolicy {
    /// Surfacing mode (default `surface`).
    #[serde(default)]
    pub mode: DriftMode,
    /// **Withdrawn** (ADR-0001 amendment, 2026-08-28). The catch-up PR this
    /// toggled was never built, and ADR-0008 removed the need for one: faber now
    /// heals a mismatch by building the image the job pins, so there is no image
    /// bump left for a PR to chase. Still parsed so an existing config doesn't
    /// error under `deny_unknown_fields`, but ignored — and never written back, so
    /// a file faber rewrites stops advertising a key faber ignores.
    #[serde(default, skip_serializing)]
    pub auto_pr: bool,
}

impl Default for DriftPolicy {
    fn default() -> Self {
        Self {
            mode: DriftMode::Surface,
            auto_pr: false,
        }
    }
}

// --- on-demand runner-image variants (ADR-0008) ----------------------------

/// Whether faber may build a per-version variant of a profile's base image when
/// a job's pinned toolchain version isn't the one the base bakes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ImageBuildMode {
    /// One static image per profile — faber's historical behavior, and the
    /// default. Drift is detected and surfaced (ADR-0001), never healed.
    #[default]
    Off,
    /// Resolve the image per job: build the variant the job's lockfile asks for
    /// when it doesn't exist yet, and keep it cached while it stays hot.
    OnDemand,
}

impl ImageBuildMode {
    /// A short, stable label for logs / the settings UI.
    pub fn as_str(self) -> &'static str {
        match self {
            ImageBuildMode::Off => "off",
            ImageBuildMode::OnDemand => "on-demand",
        }
    }

    /// Whether per-job image resolution runs at all.
    pub fn enabled(self) -> bool {
        matches!(self, ImageBuildMode::OnDemand)
    }
}

/// faber's `[runner_image]` policy (ADR-0008): whether to build per-version
/// runner-image variants on demand, and how long to keep them.
///
/// The whole section is the *operator's* switch. A watched repo can opt out of
/// being healed (`.faber/config.toml [playwright] heal = false`) but can never
/// opt in, nor say anything about how the image is built — the build runs on the
/// host daemon, outside the per-job sandbox that contains everything else.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerImagePolicy {
    /// Master switch. Default `off`: with it off the provisioning path is
    /// byte-for-byte what it was before ADR-0008, drift included.
    #[serde(default)]
    pub build: ImageBuildMode,
    /// How many most-recently-used variants stay warm. Beyond this the janitor
    /// evicts least-recently-used first. Default 3 — a bump window is one version
    /// leaving, one arriving, and the base's own.
    #[serde(default = "default_image_keep")]
    pub keep: usize,
    /// Evict a variant untouched for this long whatever its rank, so a version
    /// nobody needs any more doesn't hold disk merely by being one of few.
    /// Default 168 (a week); `0` disables the TTL and leaves only `keep`.
    #[serde(default = "default_image_idle_ttl_hours")]
    pub idle_ttl_hours: u64,
    /// Cap on a single variant build. Default 1200 (20 min): a browser set is a
    /// gigabyte-plus over whatever link this host has.
    #[serde(default = "default_image_build_timeout_secs")]
    pub build_timeout_secs: u64,
    /// An operator-owned derivation recipe, used instead of faber's own template.
    /// Its parent directory becomes the build context. Absent = faber writes and
    /// uses its own template, which is what keeps the recipe out of any reach a
    /// pull request has. Only root may write the file, every directory above it
    /// and every entry in its directory ([`crate::image::recipe_verdict`],
    /// ADR-0028): `check-config` and a save refuse anything else, and each
    /// variant build from it is refused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dockerfile: Option<String>,
}

fn default_image_keep() -> usize {
    3
}
fn default_image_idle_ttl_hours() -> u64 {
    168
}
fn default_image_build_timeout_secs() -> u64 {
    1200
}

impl Default for RunnerImagePolicy {
    fn default() -> Self {
        Self {
            build: ImageBuildMode::default(),
            keep: default_image_keep(),
            idle_ttl_hours: default_image_idle_ttl_hours(),
            build_timeout_secs: default_image_build_timeout_secs(),
            dockerfile: None,
        }
    }
}

// --- operator-owned needs allowlist (ADR-0010 §4) --------------------------

/// One entry of the operator-owned `[needs.<tool>]` allowlist (ADR-0010 §4):
/// the versions of `tool` this host will ever provision a runner for.
///
/// This is not redundant with the `faber.<tool>-<version>` label check ADR-0010
/// §3 already does against a job's own lockfile. §3 stops a PR from *asking*
/// for a version its lockfile does not pin — but a PR can legitimately *edit*
/// that lockfile, and §3 alone would then let a lockfile bump alone cause faber
/// to build whatever version landed in it, unbounded, on the operator's own
/// host. `[needs]` is the ceiling above that: an operator-only section a
/// watched repo can never reach (it lives in faber's own config, never in a
/// repo's `.faber/config.toml`), bounding disk, build time, and blast radius to
/// the versions the operator is willing to have CI cause to be built.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolNeeds {
    /// Either a comma-separated list of comparison constraints — every element
    /// starts with `>=`, `<=`, `>`, `<`, or `=`, ANDed together (a range) — or a
    /// comma-separated list of exact versions with no operator, matched against
    /// any one of them (an allowlist, ORed). Mixing the two forms has no sane
    /// interpretation and is refused by [`FileConfig::validate`], not guessed
    /// at. An empty (or blank/comma-only) string permits nothing — see
    /// [`FileConfig::needs_allow`].
    pub versions: String,
}

/// One `[services.<name>]` table (ADR-0046 §2): a service jobs on this host may
/// declare with a `faber.<name>` / `faber.<name>-<version>` label. Operator-only,
/// like `[needs]`: it lives in faber's own config, never in a repo's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceFile {
    /// The major versions a job may ask for, comma-separated (`"16, 17"`), for a
    /// service that has versions. Absent for one that has none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub versions: Option<String>,
    /// The image, the operator's choice and never the job's. `{version}` stands
    /// for the requested major version.
    pub image: String,
    /// Extensions installed into the shared server's template database.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extensions: Vec<String>,
    /// The container's `--memory` cap: reserved while a shared server runs, or
    /// added to the job's reservation for a per-job service.
    pub memory_mib: u64,
    /// How long a shared server may sit unused before faber stops it. Absent =
    /// 900 seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_stop_secs: Option<u64>,
    /// A per-job service's command: appended after the image, so it travels on
    /// an argv and never carries a secret (ADR-0046 A1).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// A per-job service's own environment. Passed name-only, the values in the
    /// Docker CLI's environment, so nothing here reaches an argv. A value may
    /// carry [`SERVICE_SECRET_PLACEHOLDER`] (ADR-0062).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// What the runner container gets for a job declaring this service. Absent,
    /// `redis` and `mailpit` get their built-in variables. A value may carry
    /// [`SERVICE_SECRET_PLACEHOLDER`], the same secret `env` gets (ADR-0062).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub job_env: BTreeMap<String, String>,
    /// Passed as `--health-cmd`; the job starts once the service is healthy. A
    /// service without one is waited for only if its image has a `HEALTHCHECK`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health_cmd: Option<String>,
}

/// The oldest Postgres major a `[services.postgres]` table may list: teardown's
/// `DROP DATABASE … WITH (FORCE)` needs 13 (ADR-0046 §10).
pub const POSTGRES_MIN_MAJOR: u64 = 13;

/// `[services.postgres] idle_stop_secs` when absent (ADR-0046 §7).
pub const SERVICE_IDLE_STOP_SECS: u64 = 900;

/// The placeholder a versioned service's `image` carries for the major version.
pub const SERVICE_VERSION_PLACEHOLDER: &str = "{version}";

/// The placeholder a per-job table's `env` and `job_env` values carry for the
/// credential faber generates for each job (ADR-0062). Never allowed in `args`
/// or `health_cmd`, which travel on an argv.
pub const SERVICE_SECRET_PLACEHOLDER: &str = "{secret}";

/// The built-in service names, reserved as `[needs]` tools whether or not a
/// table exists (ADR-0046 §10). Any `[services.<name>]` is reserved too.
pub const SERVICE_NAMES: [&str; 3] = ["postgres", "redis", "mailpit"];

/// Names a per-job `[services.<name>]` may not take besides `postgres`: faber
/// already puts them on a job's network (ADR-0046 A1).
const SERVICE_RESERVED: [&str; 1] = ["dind"];

/// `job_env` names faber sets itself (ADR-0046 A2), besides the prefixes in
/// [`SERVICE_RESERVED_PREFIXES`].
const SERVICE_RESERVED_ENV: [&str; 6] = [
    "DATABASE_URL",
    "PGHOST",
    "PGPORT",
    "PGUSER",
    "PGPASSWORD",
    "PGDATABASE",
];
const SERVICE_RESERVED_PREFIXES: [&str; 4] = ["RUNNER_", "GITHUB_", "ACTIONS_", "FABER_"];

/// `^[A-Z_][A-Z0-9_]*$`.
fn service_env_name_ok(n: &str) -> bool {
    let b = n.as_bytes();
    !b.is_empty()
        && (b[0].is_ascii_uppercase() || b[0] == b'_')
        && b.iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_')
}

/// A value the daemon and the runner can carry: no NUL, no line break.
fn service_env_value_ok(v: &str) -> bool {
    !v.bytes().any(|b| b == 0 || b == b'\n' || b == b'\r')
}

/// Parse a `[services.postgres] versions` string: a comma-separated list of
/// exact majors, each an integer of at least [`POSTGRES_MIN_MAJOR`], written
/// without a leading zero and listed once. Returns the majors as written, which
/// is what a label's version is compared against.
///
/// Not [`parse_needs_version`]: that one needs `major.minor`, and a Postgres
/// image is chosen by its major alone.
pub fn parse_service_versions(s: &str) -> Result<Vec<String>, String> {
    if s.trim().is_empty() {
        return Err("versions is empty — list at least one major, e.g. \"16, 17\"".into());
    }
    let mut out: Vec<String> = Vec::new();
    for token in s.split(',').map(str::trim) {
        let digits = !token.is_empty() && token.bytes().all(|b| b.is_ascii_digit());
        if !digits || (token.len() > 1 && token.starts_with('0')) {
            return Err(format!(
                "versions entry {token:?} is not a major version — write whole majors such as 16"
            ));
        }
        match token.parse::<u64>() {
            Ok(major) if major >= POSTGRES_MIN_MAJOR => {}
            _ => {
                return Err(format!(
                    "versions entry {token} is below {POSTGRES_MIN_MAJOR}, the oldest major \
                     faber can tear a job's databases down on (DROP DATABASE … WITH (FORCE))"
                ))
            }
        }
        if out.iter().any(|v| v == token) {
            return Err(format!("versions lists {token} twice"));
        }
        out.push(token.to_string());
    }
    Ok(out)
}

/// Whether `s` is an extension name faber will pass to `CREATE EXTENSION`:
/// `^[a-z0-9_]+$`, so it never needs quoting.
fn service_extension_ok(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

impl ServiceFile {
    /// What a job declaring this per-job service gets in its environment: the
    /// table's `job_env`, or the built-in variables of `redis` and `mailpit`
    /// when it states none (ADR-0046 A1).
    pub fn exported_job_env(&self, name: &str) -> BTreeMap<String, String> {
        if !self.job_env.is_empty() {
            return self.job_env.clone();
        }
        crate::services::builtin_job_env(name)
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// Everything wrong with this table as `[services.<name>]` (ADR-0046 §2,
    /// §10), each line prefixed with `services.<name>`. Empty means the table
    /// resolves into the policy.
    ///
    /// Postgres requires `versions` and its image requires `{version}`. Redis
    /// and Mailpit refuse `versions`, `extensions` and `idle_stop_secs`: each
    /// is one container per job, asked for by its bare label, with no template
    /// database and nothing left running to stop.
    pub fn errors(&self, name: &str) -> Vec<String> {
        let mut errs = Vec::new();
        let mut push = |e: String| errs.push(format!("services.{name}: {e}"));
        let versioned = name == crate::services::POSTGRES;
        if !versioned {
            if !crate::services::service_name_ok(name) {
                push(
                    "the name is the service's host name on a job's network: lowercase letters \
                     and digits, starting with a letter, at most 32 characters"
                        .into(),
                );
            } else if SERVICE_RESERVED.contains(&name) {
                push(format!(
                    "{name} is a name faber already puts on a job's network — pick another"
                ));
            }
        }
        let mut candidates: Vec<String> = Vec::new();
        if versioned {
            match self.versions.as_deref().map(parse_service_versions) {
                None => push(
                    "versions is required — the exact majors a job may ask for with \
                     faber.postgres-<major>, e.g. \"16, 17\""
                        .into(),
                ),
                Some(Err(e)) => push(e),
                Some(Ok(v)) => candidates = v,
            }
            if candidates.is_empty() {
                // Still check the image's shape when the list is unusable.
                candidates.push(POSTGRES_MIN_MAJOR.to_string());
            }
            if !self.image.contains(SERVICE_VERSION_PLACEHOLDER) {
                push(format!(
                    "image {:?} must contain {SERVICE_VERSION_PLACEHOLDER}, which stands for \
                     the major a job asks for",
                    self.image
                ));
            }
            for (key, set) in [
                ("args", !self.args.is_empty()),
                ("env", !self.env.is_empty()),
                ("job_env", !self.job_env.is_empty()),
                ("health_cmd", self.health_cmd.is_some()),
            ] {
                if set {
                    push(format!(
                        "{key} does not apply — postgres is the one shared server, and faber \
                         names its database and credentials"
                    ));
                }
            }
            for ext in &self.extensions {
                if !service_extension_ok(ext) {
                    push(format!(
                        "extensions entry {ext:?} must be lowercase letters, digits and _ only"
                    ));
                }
            }
            if self.idle_stop_secs == Some(0) {
                push(format!(
                    "idle_stop_secs must be at least 1 — omit it for {SERVICE_IDLE_STOP_SECS}"
                ));
            }
        } else {
            if self.versions.is_some() {
                push(format!(
                    "versions does not apply — {name} has one image, and a job asks for it as \
                     faber.{name}"
                ));
            }
            if self.image.contains(SERVICE_VERSION_PLACEHOLDER) {
                push(format!(
                    "image {:?} contains {SERVICE_VERSION_PLACEHOLDER}, but {name} has no \
                     versions to fill it from",
                    self.image
                ));
            }
            if !self.extensions.is_empty() {
                push("extensions belong to services.postgres".into());
            }
            if self.idle_stop_secs.is_some() {
                push(format!(
                    "idle_stop_secs belongs to services.postgres — a {name} container stops \
                     with its job"
                ));
            }
            candidates.push(String::new());
            if self.args.iter().any(|a| a.contains('\0')) {
                push("args entries must not contain a NUL".into());
            }
            if self
                .args
                .iter()
                .any(|a| a.contains(SERVICE_SECRET_PLACEHOLDER))
            {
                push(format!(
                    "args travel on an argv, so they cannot carry {SERVICE_SECRET_PLACEHOLDER} \
                     — pass the secret to the container through env"
                ));
            }
            for (table, vars) in [("env", &self.env), ("job_env", &self.job_env)] {
                for (k, v) in vars {
                    if !service_env_name_ok(k) {
                        push(format!("{table} name {k:?} must match [A-Z_][A-Z0-9_]*"));
                    }
                    // Both maps reach the container through the `docker` CLI
                    // child's environment, so a name that CLI reads would steer
                    // faber's own call, not the service.
                    if crate::docker::docker_cli_env_name(k) {
                        push(format!(
                            "{table} name {k} is read by the docker CLI faber runs (PATH, HOME, \
                             TMPDIR, LANG, DOCKER_*, BUILDKIT_*)"
                        ));
                    }
                    // The key only: a value may be a credential.
                    if !service_env_value_ok(v) {
                        push(format!(
                            "{table} {k} has a value with a NUL or a line break"
                        ));
                    }
                }
            }
            for k in self.job_env.keys() {
                if SERVICE_RESERVED_ENV.contains(&k.as_str())
                    || SERVICE_RESERVED_PREFIXES.iter().any(|p| k.starts_with(p))
                {
                    push(format!(
                        "job_env {k} is set by faber itself (DATABASE_URL, PG*, or anything \
                         under RUNNER_, GITHUB_, ACTIONS_, FABER_)"
                    ));
                }
            }
            if let Some(cmd) = &self.health_cmd {
                if cmd.trim().is_empty() || cmd.contains('\0') {
                    push("health_cmd must be a command, without a NUL".into());
                }
                if cmd.contains(SERVICE_SECRET_PLACEHOLDER) {
                    push(format!(
                        "health_cmd travels on an argv, so it cannot carry \
                         {SERVICE_SECRET_PLACEHOLDER} — check an unauthenticated endpoint"
                    ));
                }
            }
        }
        // Checked as Docker will see it, after each version is substituted. A
        // placeholder Redis or Mailpit cannot fill was reported above.
        let placeholder_left = !versioned && self.image.contains(SERVICE_VERSION_PLACEHOLDER);
        if !placeholder_left
            && !candidates.iter().all(|v| {
                crate::untrusted::image_ref(&self.image.replace(SERVICE_VERSION_PLACEHOLDER, v))
            })
        {
            push(format!("image {:?} is not an image reference", self.image));
        }
        if self.memory_mib == 0 {
            push("memory_mib must be greater than 0 — it is the container's --memory cap".into());
        }
        errs
    }
}

/// What boot says about the mirror settings it will not act on (docs/mirrors.md):
/// an invalid `[mirror]`, which keeps the mirror off; `mirror = true` on a
/// profile that cannot use it; and `mirror = true` with no `[mirror]` to point
/// it at. Warnings, never a refusal to boot (AGENTS.md). The last one is not a
/// `check-config` error either: leaving profiles opted in while the section is
/// removed is how an operator turns the mirror off in one edit.
pub fn mirror_warnings(cfg: &FileConfig) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(m) = &cfg.mirror {
        for e in m.errors() {
            out.push(format!("[mirror] is off: {e}"));
        }
    }
    for p in cfg.profiles.iter().filter(|p| p.mirror) {
        if p.backend != BackendKind::Docker {
            out.push(format!(
                "profile '{}': mirror = true is ignored — only a docker profile can use the mirror",
                p.name
            ));
        } else if cfg.mirror.is_none() {
            out.push(format!(
                "profile '{}': mirror = true does nothing without a [mirror] section",
                p.name
            ));
        }
    }
    out
}

/// The line boot and `faber check-config` give for `[services.*]` tables that
/// do not resolve, and are therefore left out of the policy: a job declaring
/// one is refused, as though the table were absent. A warning and never a
/// refusal to boot (AGENTS.md); `check-config` and a settings save refuse the
/// same tables as errors.
pub fn services_ignored_warning(cfg: &FileConfig) -> Option<String> {
    let errs = cfg.services_errors();
    if errs.is_empty() {
        return None;
    }
    Some(format!(
        "[services] tables ignored, so jobs declaring them are refused (ADR-0046): {}",
        errs.join("; ")
    ))
}

/// One line per service image faber may not pull (ADR-0046 §10): it names no
/// registry host and `[host] pull_unqualified_images = false`, so unless it is
/// already in the Linux VM's image store, every job declaring the service fails.
pub fn services_pull_warnings(cfg: &FileConfig) -> Vec<String> {
    let policy = cfg.services_policy();
    let setting = cfg.host.pull_unqualified_images;
    let mut images: Vec<(String, String)> = Vec::new();
    if let Some(p) = &policy.postgres {
        for v in &p.versions {
            images.push((
                format!("postgres-{v}"),
                p.image.replace(SERVICE_VERSION_PLACEHOLDER, v),
            ));
        }
    }
    for (name, p) in &policy.per_job {
        images.push((name.clone(), p.image.clone()));
    }
    images
        .into_iter()
        .filter(|(_, image)| !crate::profile_image::pull_allowed(image, setting))
        .map(|(name, image)| {
            format!(
                "service {name}: image {image} names no registry host and [host] \
                 pull_unqualified_images = false, so faber never pulls it — unless it is \
                 already in the Linux VM's image store, every job declaring {name} fails. \
                 Prefix it with its registry (docker.io/library/…, ghcr.io/…)"
            )
        })
        .collect()
}

/// The result of checking one `(tool, version)` pair against the operator's
/// `[needs]` allowlist (ADR-0010 §4). Never collapsed to a bool: a refusal here
/// is meant to be surfaced the way a drift mismatch is — with a legible reason —
/// never silently ignored, which would look like a match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NeedsVerdict {
    /// `version` satisfies `tool`'s configured `versions`.
    Allowed,
    /// No `[needs.<tool>]` entry exists for `tool` at all. Fail closed: an
    /// unconfigured tool is not provisionable, whatever version is asked for.
    UnknownTool,
    /// `tool` is configured, but `version` does not satisfy it: outside every
    /// range bound, absent from the exact list, unparseable itself, or checked
    /// against a `versions` string that itself doesn't parse (mixed forms, or
    /// empty). Carries the configured string verbatim so the caller can name it
    /// in an operator-facing refusal without re-reading the config.
    OutOfRange(String),
}

/// A parsed comparison operator from one range element of a `[needs.<tool>]`
/// `versions` string.
#[derive(Debug, Clone, Copy)]
enum NeedsCmp {
    Ge,
    Le,
    Gt,
    Lt,
    Eq,
}

/// Split a leading comparison operator off `token`, if it has one. Order
/// matters: the two-character operators must be tried before their
/// single-character prefixes, or `>=1.55` would be misread as `>` on `=1.55`.
fn strip_needs_cmp(token: &str) -> Option<(NeedsCmp, &str)> {
    if let Some(rest) = token.strip_prefix(">=") {
        Some((NeedsCmp::Ge, rest.trim()))
    } else if let Some(rest) = token.strip_prefix("<=") {
        Some((NeedsCmp::Le, rest.trim()))
    } else if let Some(rest) = token.strip_prefix('>') {
        Some((NeedsCmp::Gt, rest.trim()))
    } else if let Some(rest) = token.strip_prefix('<') {
        Some((NeedsCmp::Lt, rest.trim()))
    } else if let Some(rest) = token.strip_prefix('=') {
        Some((NeedsCmp::Eq, rest.trim()))
    } else {
        None
    }
}

/// The two shapes a `[needs.<tool>]` `versions` string may take (ADR-0010 §4),
/// classified from its comma-separated, trimmed, non-blank elements.
enum NeedsVersionForm<'a> {
    /// Every element carries a comparison operator — constraints AND together.
    Range(Vec<&'a str>),
    /// No element carries an operator — matches any one of them (OR).
    List(Vec<&'a str>),
    /// No elements at all: an empty string, or one that is blank or all commas.
    Empty,
    /// Some elements carry an operator and some don't. Not a form — a mistake,
    /// and [`FileConfig::validate`] says so rather than guessing which was meant.
    Mixed,
}

fn classify_needs_versions(versions: &str) -> NeedsVersionForm<'_> {
    let tokens: Vec<&str> = versions
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return NeedsVersionForm::Empty;
    }
    let (with_op, without_op): (Vec<&str>, Vec<&str>) = tokens
        .into_iter()
        .partition(|t| strip_needs_cmp(t).is_some());
    match (with_op.is_empty(), without_op.is_empty()) {
        (false, true) => NeedsVersionForm::Range(with_op),
        (true, false) => NeedsVersionForm::List(without_op),
        // Only one side empty is a clean form; all-empty can't happen (tokens
        // wasn't empty), so what's left is a genuine mix of both.
        _ => NeedsVersionForm::Mixed,
    }
}

/// Parse a bare `major.minor[.patch]` version into a numerically comparable
/// triple. Patch defaults to `0` when absent, so `"1.62"` and `"1.62.0"` compare
/// equal. Never a string compare — that gets `"1.9.0" < "1.10.0"` backwards
/// (`'9' > '1'` lexically) — and hand-rolled rather than a `semver` dependency,
/// which ADR-0010 §4 deliberately takes on none of: this only ever has to
/// understand faber's own numeric scheme, never pre-release tags or build
/// metadata.
fn parse_needs_version(s: &str) -> Option<(u64, u64, u64)> {
    let mut parts = s.trim().split('.');
    let major: u64 = parts.next()?.parse().ok()?;
    let minor: u64 = parts.next()?.parse().ok()?;
    let patch: u64 = match parts.next() {
        Some(p) => p.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() {
        // A fourth dotted component isn't a version this scheme represents —
        // unparseable, not silently truncated.
        return None;
    }
    Some((major, minor, patch))
}

/// Whether one range element holds for `requested`. An element whose operator
/// or version fails to parse reads as `false` rather than being dropped — a
/// silently-skipped upper bound would widen the operator's ceiling past what
/// they wrote, exactly what ADR-0010 §4 exists to prevent.
fn needs_range_bound_holds(token: &str, requested: (u64, u64, u64)) -> bool {
    let Some((cmp, ver)) = strip_needs_cmp(token) else {
        return false;
    };
    let Some(bound) = parse_needs_version(ver) else {
        return false;
    };
    match cmp {
        NeedsCmp::Ge => requested >= bound,
        NeedsCmp::Le => requested <= bound,
        NeedsCmp::Gt => requested > bound,
        NeedsCmp::Lt => requested < bound,
        NeedsCmp::Eq => requested == bound,
    }
}

/// Whether `requested` satisfies a `[needs.<tool>]` `versions` string.
/// `Empty` and `Mixed` both permit nothing: the former by design (see
/// [`ToolNeeds::versions`]), the latter because [`FileConfig::validate`] is
/// where a mixed string gets a chance to be fixed, not guessed at here. List
/// mode drops an unparseable *entry* — it simply can never match anything —
/// without invalidating the rest of the list; range mode cannot afford the same
/// leniency toward a bad *bound* (see [`needs_range_bound_holds`]).
fn needs_versions_allow(versions: &str, requested: (u64, u64, u64)) -> bool {
    match classify_needs_versions(versions) {
        NeedsVersionForm::Empty | NeedsVersionForm::Mixed => false,
        NeedsVersionForm::Range(bounds) => {
            bounds.iter().all(|b| needs_range_bound_holds(b, requested))
        }
        NeedsVersionForm::List(entries) => entries
            .iter()
            .any(|e| parse_needs_version(e) == Some(requested)),
    }
}

/// Whether a `[needs.<tool>]` `versions` change `old` -> `new` only loses
/// versions (ADR-0028, #238): both strings are the exact-list form and every
/// version `new` permits, `old` permits too. An unparseable entry in `new`
/// permits nothing (see [`needs_versions_allow`]), so it is no gain. Any other
/// pair, a range on either side above all, is `false`: comparing ranges by
/// containment is not worth the risk, and the widening check may over-report.
pub fn needs_list_only_narrows(old: &str, new: &str) -> bool {
    let (NeedsVersionForm::List(before), NeedsVersionForm::List(after)) =
        (classify_needs_versions(old), classify_needs_versions(new))
    else {
        return false;
    };
    after.iter().all(|e| match parse_needs_version(e) {
        None => true,
        Some(v) => before.iter().any(|o| parse_needs_version(o) == Some(v)),
    })
}

// --- auto-retry of faber-caused failures (ADR-0003) ------------------------

/// faber's `[retry]` policy: auto-re-run jobs that failed because faber's *own*
/// runner was ended under them (OOM / lifetime-cap / reclaim / freeze / a container
/// lost with its daemon), detected from faber's own audit record of how the
/// `faber-*` runner ended. See ADR-0003.
///
/// **Opt-in** — disabled by default, so faber stays a pure provisioner until a
/// deployment explicitly grants it the outward-facing action of re-running a
/// workflow. A genuine step failure is never re-run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryPolicy {
    /// Master switch. `false` (default) = detect nothing, re-run nothing.
    #[serde(default)]
    pub enabled: bool,
    /// The highest `run_attempt` auto-retry will *create*. `2` (default) = one
    /// automatic retry: a first-attempt faber-death is re-run once; a still-dead
    /// second attempt is left for a human. Read through [`RetryPolicy::attempt_cap`],
    /// which floors it at 2 so a stray `0`/`1` can't disable the first-attempt retry.
    #[serde(default = "default_retry_max_attempts")]
    pub max_attempts: u64,
    /// Only re-run runs whose last update is within this many seconds — so a
    /// freshly-started daemon retries *recent* faber-deaths, not the historical
    /// ones the failed-runs scan also returns. Default 1800 (30 min).
    #[serde(default = "default_retry_lookback_secs")]
    pub lookback_secs: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            enabled: false,
            max_attempts: default_retry_max_attempts(),
            lookback_secs: default_retry_lookback_secs(),
        }
    }
}

impl RetryPolicy {
    /// The lookback window as a `Duration`.
    pub fn lookback(&self) -> Duration {
        Duration::from_secs(self.lookback_secs)
    }

    /// The effective attempt cap, floored at 2 so a misconfigured `0`/`1` doesn't
    /// silently suppress the one automatic retry the feature exists to provide.
    pub fn attempt_cap(&self) -> u64 {
        self.max_attempts.max(2)
    }
}

// --- likely-wasted claims (ADR-0063) ----------------------------------------

/// What faber does when one waste signal fires (ADR-0063 §3). Ordered weakest to
/// strongest, so the action for several signals is the largest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WasteMode {
    /// No read for this signal: the default, and what an absent key means.
    #[default]
    Off,
    /// Read and decide, change nothing for the job: one `waste` audit line.
    Record,
    /// Move the claim to the waste tier, below every other (ADR-0063 §5).
    Demote,
    /// Release the claim and cancel its run (ADR-0063 §6).
    Skip,
}

impl WasteMode {
    /// The key's value as written, for the audit log.
    pub fn as_str(self) -> &'static str {
        match self {
            WasteMode::Off => "off",
            WasteMode::Record => "record",
            WasteMode::Demote => "demote",
            WasteMode::Skip => "skip",
        }
    }
}

/// `[waste]`: one mode per signal (ADR-0063 §2–3). Every key is off unless the
/// operator sets it, so a host that did not ask reads nothing and admits as before.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WastePolicy {
    /// The PR is closed.
    #[serde(default, skip_serializing_if = "is_waste_off")]
    pub closed: WasteMode,
    /// The PR's head moved past the run's `head_sha`.
    #[serde(default, skip_serializing_if = "is_waste_off")]
    pub superseded: WasteMode,
    /// The PR conflicts with its base (`mergeable` is `false`).
    #[serde(default, skip_serializing_if = "is_waste_off")]
    pub conflicted: WasteMode,
    /// Another job of the same run already failed. Never `skip` ([`Self::problems`]).
    #[serde(default, skip_serializing_if = "is_waste_off")]
    pub run_failed: WasteMode,
}

fn is_waste_off(mode: &WasteMode) -> bool {
    *mode == WasteMode::Off
}

impl WastePolicy {
    /// Every signal off: nothing is read.
    pub fn is_off(&self) -> bool {
        *self == Self::default()
    }

    /// Whether any signal read from the pull request is on.
    pub fn reads_pull(&self) -> bool {
        [self.closed, self.superseded, self.conflicted]
            .iter()
            .any(|m| *m != WasteMode::Off)
    }

    /// Whether any signal is set to `skip`, which cancels runs.
    pub fn skips(&self) -> bool {
        [
            self.closed,
            self.superseded,
            self.conflicted,
            self.run_failed,
        ]
        .contains(&WasteMode::Skip)
    }

    /// What the config check refuses. `skip` would cancel the whole run, and a
    /// failed run's other jobs may still report more failures (ADR-0063 §6.2).
    pub fn problems(&self) -> Vec<String> {
        let mut errs = Vec::new();
        if self.run_failed == WasteMode::Skip {
            errs.push(
                "waste.run_failed cannot be \"skip\": cancelling the run would hide failures its \
                 other jobs may still report — use \"demote\" or \"record\""
                    .into(),
            );
        }
        errs
    }
}

/// Host-level custody of the Docker daemon — ADR-0007.
///
/// faber is the Docker manager on this box, so keeping the daemon usable is its
/// job. These knobs exist because that responsibility is host-affecting and
/// reaches unattended follower hosts through the release channel: an operator
/// whose machine has something *else* owning Docker turns them off.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
pub struct HostPolicy {
    /// Restart a wedged daemon rather than only refusing to use it. Governs the
    /// machine's `docker` service only (a Linux host); faber's own Linux VM
    /// follows `revive_vms`. On by default: a faber that can diagnose a dead Docker
    /// and then only wait for a human is a monitor, not a manager (see ADR-0007).
    #[serde(default = "default_true")]
    pub revive_docker: bool,
    /// Restart a VM faber created when it wedges, by asking the VM supervisor to
    /// stop it and start it again (ADR-0026, ADR-0007 retargeted): a per-job macOS
    /// VM stuck before its runner console, once per job, never after `begin`. It is
    /// also the one switch for restarting faber's long-lived Linux VM when Docker
    /// goes unanswered (`revive_after_secs` and `revive_cooldown_secs` apply, and
    /// `revive_docker` does not). On by default: a `macos-vm` profile is new, so no
    /// existing host changes.
    #[serde(default = "default_true")]
    pub revive_vms: bool,
    /// How long the daemon must have been unresponsive before that counts as a
    /// wedge rather than a hiccup. Default 600 (10 min) — long enough that a
    /// daemon merely busy under a heavy cohort is never killed for it.
    #[serde(default = "default_revive_after_secs")]
    pub revive_after_secs: u64,
    /// Ignored since ADR-0026 (#69's fast path for Docker Desktop saying it is
    /// unable to start): faber no longer restarts Docker Desktop, so there is no
    /// such error to act on sooner. Still read, so a file that sets it boots, and
    /// warned about at every boot while it is set
    /// ([`HostPolicy::ignored_keys_warning`]). Never written back: a settings save
    /// drops it.
    #[serde(default, skip_serializing)]
    pub revive_unable_to_start_secs: Option<u64>,
    /// Minimum gap between two revival attempts. Default 1800 (30 min).
    ///
    /// Also how long Docker must stay answering after a restart before that
    /// restart counts as having worked, and the give-up count starts over
    /// (ADR-0024 §2). Without that second half the gap did not hold: a daemon on a
    /// full disk answers for a minute before it wedges again, and the first answer
    /// used to reset both.
    #[serde(default = "default_revive_cooldown_secs")]
    pub revive_cooldown_secs: u64,
    /// Reclaim CI-created Docker space when the volume backing Docker drops
    /// below this many GiB free. Default 25. `0` disables reclamation.
    #[serde(default = "default_disk_floor_gib")]
    pub disk_floor_gib: u64,
    /// Start no Docker job while the volume backing Docker has less than this
    /// many GiB free, and do not restart Docker onto it either — ADR-0024.
    ///
    /// Where [`HostPolicy::disk_floor_gib`] is the point reclamation *starts*,
    /// this is the point admission *stops*, so it may not sit above that one: a
    /// host would stop CI before it had tried to make room. `faber check-config`
    /// and the settings save reject that ([`FileConfig::validate`]); boot warns.
    ///
    /// Three states, read through [`HostPolicy::disk_admission`]. This tightens
    /// admission on hosts that update unattended (ADR-0006), so it copies the
    /// shape ADR-0013 built for `dind` (AGENTS.md): absent is *unstated* — no
    /// gate, exactly the behaviour before this key existed, but warned at every
    /// boot, shown as its own state on the dashboard, and written explicitly the
    /// first time the settings form is saved; `0` is off, stated; a positive
    /// number is the gate. Flipping the unstated default is a future,
    /// release-noted change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_admission_floor_gib: Option<u64>,
    /// The RAM, in GiB, that the Docker VM and CI must leave to the machine faber
    /// runs on — ADR-0021 §2. See [`crate::machine`].
    ///
    /// Once stated, Gate 1 plans within `machine RAM − this`, faber warns while
    /// the VM is larger than that, and admission — macOS VM jobs included — pauses
    /// while the machine reports memory pressure or its swap is growing
    /// (ADR-0032 §3). Above 0 it also measures Gate 1's reserve: the larger of
    /// `reserve_mib` and the 24-hour peak of what the VM uses beyond faber's own
    /// containers (§3, [`crate::cotenant`]). On macOS the VM never gives memory back, so its size is the
    /// machine's only memory reservation, and the warning is the fix.
    ///
    /// Tightens admission, so it copies the shape of
    /// [`HostPolicy::disk_admission_floor_gib`]: absent is *unstated* — exactly the
    /// behaviour before this key, warned at every boot with the measured split,
    /// shown as `unstated`, and written as `0` the first time the settings form is
    /// saved. `0` is stated and changes nothing, the pressure gate and the
    /// measured reserve included. A
    /// share not smaller than the machine is refused by `faber check-config` and
    /// the settings save. At boot it is too large to plan within, so planning
    /// ignores it with a warning, while the pressure and swap gates still apply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_memory_gib: Option<u64>,
    /// The cores CI must leave idle for the machine — ADR-0021 §2. Once stated,
    /// ADR-0019's CPU budget is built on `min(NCPU, machine cores − this)`, a
    /// real reservation even before the VM is resized. Three states, as
    /// [`HostPolicy::keep_memory_gib`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keep_cpus: Option<u64>,
    /// Ignored since ADR-0026 (formerly ADR-0016: compose project prefixes whose
    /// images were declared recreatable). Faber no longer reclaims from Docker
    /// Desktop. Kept so an existing config still parses; a non-empty value warns
    /// at boot and in `faber check-config` ([`recreatable_ignored_warning`]).
    #[serde(default)]
    pub recreatable_projects: Vec<String>,
    /// Ignored since ADR-0026, as [`HostPolicy::recreatable_projects`] (formerly
    /// ADR-0016: compose volume roles declared to hold cache, not data).
    #[serde(default)]
    pub recreatable_volumes: Vec<String>,
    /// The IPv4 block faber carves its per-job network subnets out of, so its
    /// sandboxes stop competing for the daemon's *shared* default address pools.
    ///
    /// A stock daemon offers 31 pool slots to the whole machine, and every tool
    /// on it draws from the same 31. When something else holds them, faber's
    /// provisions fail with `all predefined address pools have been fully
    /// subnetted` — a failure that says nothing about faber's own networks, which
    /// in the case that prompted this key were all live and correctly torn down.
    /// Declaring a block faber owns makes its capacity the operator's choice
    /// rather than a race, and it fixes the problem without faber forming an
    /// opinion about anybody else's network. See [`crate::netpool`] and
    /// ADR-0020.
    ///
    /// Pick a private range nothing on the host or its VPNs routes — the daemon
    /// will happily bridge a block that collides with something you reach over
    /// the network, and the symptom is a job that cannot resolve a service rather
    /// than an error here.
    ///
    /// Empty, the default, leaves allocation to Docker exactly as it was before
    /// this key existed. That is deliberate: this reaches unattended follower
    /// hosts through ADR-0006, and a host that never asked must be unchanged.
    #[serde(default)]
    pub network_pool: String,
    /// The prefix length of each per-job subnet carved out of
    /// [`HostPolicy::network_pool`]. Unread when no pool is declared.
    ///
    /// Default 24: 254 usable addresses, far more than a runner and a dind need,
    /// while still making a `/16` pool 256 concurrent jobs — comfortably past any
    /// runner cap a single host will carry.
    #[serde(default = "default_network_subnet_size")]
    pub network_subnet_size: u8,
    /// Delete each top-level entry of a cache volume (ADR-0005) that nothing has
    /// read, written or changed for this many days — ADR-0024 §4. See
    /// [`crate::cache`].
    ///
    /// Absent on both this and [`Self::cache_max_gib`], the default, leaves the
    /// policy to faber (ADR-0055): nothing is deleted inside a cache volume on a
    /// healthy disk, and under disk pressure the least recently used entries go.
    /// If either key is set, only the limits that are set apply, and `0` says
    /// "no limit" on that axis explicitly.
    ///
    /// The minimum is 2, not 1. The Docker VM mounts its disk `relatime`, so a
    /// read refreshes an entry's atime at most once a day, and a one-day window
    /// would expire entries a consumer reads every day. `faber check-config` and
    /// the settings save reject 1; a daemon that boots on one warns and treats it
    /// as off (AGENTS.md: reject where a human is looking, warn where they are
    /// not).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_max_age_days: Option<u64>,
    /// A cache volume holding more than this many GiB loses its least recently
    /// used top-level entries, oldest first, until it fits — ADR-0055. See
    /// [`crate::cache`].
    ///
    /// An age limit cannot bound a cache that is read on every run; this one can.
    /// `0` means no size limit; absent is the same unless [`Self::cache_max_age_days`]
    /// is also absent, in which case faber relieves disk pressure itself. An entry
    /// used in the last 2 hours is never deleted for size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_max_gib: Option<u64>,
    /// This faber instance's name, as it appears at the end of every runner name
    /// it mints and in its `faber-host-<instance>` label (#94): a lowercase letter,
    /// then up to 15 lowercase letters or digits.
    ///
    /// Empty, the default, derives one from a short hash of the hostname — stable
    /// and distinct per machine, and never the hostname itself, which every runner
    /// listing on GitHub would otherwise publish. Set it to make the instance
    /// readable at a glance when several faber hosts share one pool.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub instance: String,
    /// Whether faber may pull a profile image whose reference names no registry
    /// host (`owner/x`, `ubuntu:24.04`) when it is absent — #156. See
    /// [`crate::profile_image::origin`].
    ///
    /// A runner starts with `--pull=never`, so the only pull of a profile image
    /// is faber's own. A bare `owner/x` resolves to Docker Hub's `owner`
    /// namespace: when a local-only tag of that shape goes missing, a pull fetches
    /// whatever that namespace holds, which may be someone else's image.
    /// `false` never pulls such a reference — absent is missing at once — while a
    /// registry-qualified or digest-pinned one is pulled as before.
    ///
    /// Three states, because `false` withdraws a pull a host may rely on: absent
    /// is *unstated* — the pull goes ahead exactly as `docker run`'s implicit one
    /// did, but each profile whose image sits in a Docker Hub user namespace is
    /// warned about at every boot and shown on the dashboard, and the settings
    /// form writes `true` the first time it is saved; `true` accepts the pull,
    /// stated. An official-library reference (no `/`) is never warned about: no
    /// one can register a name in `library/`. Flipping the unstated default to
    /// `false` is a future, release-noted change.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_unqualified_images: Option<bool>,
    /// Whether faber holds off idle system sleep — issue #90. See
    /// [`crate::awake`] and [`SleepPolicy`].
    ///
    /// A host that idle-sleeps mid-job loses the job: GitHub fails it about ten
    /// minutes after its runner stops answering. `pmset -a sleep 0` avoids that by
    /// never sleeping; `while_busy` keeps the machine awake only while there is
    /// work, and `always` for as long as the daemon runs.
    ///
    /// Absent is `off`, so a host that never asked is unchanged by the release
    /// that added it (AGENTS.md). macOS only; elsewhere it is a no-op that boot
    /// warns about. Read through [`HostPolicy::sleep_policy`], which also folds in
    /// the old bool.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prevent_sleep: Option<SleepPolicy>,
    /// The key before [`HostPolicy::prevent_sleep`]: `true` is `while_busy`,
    /// `false` is `off`. Still read, so a file that sets it keeps its behaviour,
    /// and warned about at every boot while it is set
    /// ([`HostPolicy::deprecated_sleep_key_warning`]). Never written back: a
    /// settings save moves it to the new key ([`HostPolicy::migrate_sleep_key`]).
    /// Setting both is a config error.
    #[serde(default, skip_serializing)]
    pub prevent_sleep_while_busy: Option<bool>,
    /// Whether a job must ask for its Docker daemon with a `dind` label in its
    /// `runs-on` — ADR-0026 § Consequences: only jobs that ask for
    /// `[faber, dind]` get a daemon. See [`crate::dind`].
    ///
    /// Absent or `false` is the behaviour before the rule existed: every job on a
    /// profile that grants dind (`dind = true`, or unstated) gets a daemon,
    /// whatever its labels, and a job on a denying profile gets none, with no
    /// refusal. `true` is the rule: a job gets a daemon only when its `runs-on`
    /// includes `dind` *and* its profile grants one; a job without the label gets
    /// none on any profile, its runner's `DOCKER_HOST` points at a sentinel socket
    /// that names the fix, the runner that does get a daemon registers with the
    /// `dind` label, and a job that asks on a denying profile is refused rather
    /// than minted a runner.
    ///
    /// Three states, read through [`HostPolicy::dind_label_rule`]. `true` withdraws
    /// a daemon from every job that does not ask, which a host relies on today, so
    /// it copies the shape of [`HostPolicy::disk_admission_floor_gib`] (AGENTS.md):
    /// absent is *unstated* — the old behaviour, warned at every boot, shown as
    /// `unstated` on the dashboard, and written as `false` the first time the
    /// settings form is saved. The switch is scheduled, not indefinite: the cut's
    /// host sitting sets it `true` (after the watched workflows that run `docker`
    /// carry `dind`), and the key itself is removed after the cut, the rule then
    /// holding unconditionally.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dind_requires_label: Option<bool>,
}

fn default_true() -> bool {
    true
}
fn default_revive_after_secs() -> u64 {
    600
}
fn default_revive_cooldown_secs() -> u64 {
    1800
}
fn default_disk_floor_gib() -> u64 {
    25
}
fn default_network_subnet_size() -> u8 {
    24
}

impl Default for HostPolicy {
    fn default() -> Self {
        Self {
            revive_docker: default_true(),
            revive_vms: default_true(),
            revive_after_secs: default_revive_after_secs(),
            revive_unable_to_start_secs: None,
            revive_cooldown_secs: default_revive_cooldown_secs(),
            disk_floor_gib: default_disk_floor_gib(),
            disk_admission_floor_gib: None,
            keep_memory_gib: None,
            keep_cpus: None,
            recreatable_projects: Vec::new(),
            recreatable_volumes: Vec::new(),
            network_pool: String::new(),
            network_subnet_size: default_network_subnet_size(),
            cache_max_age_days: None,
            cache_max_gib: None,
            instance: String::new(),
            pull_unqualified_images: None,
            prevent_sleep: None,
            prevent_sleep_while_busy: None,
            dind_requires_label: None,
        }
    }
}

impl HostPolicy {
    /// How long Docker must have been unresponsive before faber restarts it, in
    /// ms.
    pub fn revive_after_ms(&self) -> u64 {
        self.revive_after_secs.saturating_mul(1000)
    }

    /// The boot warning for `[host]` keys this release reads and ignores, if any
    /// is set. Warned, never refused (AGENTS.md).
    pub fn ignored_keys_warning(&self) -> Option<String> {
        self.revive_unable_to_start_secs.map(|_| {
            "host.revive_unable_to_start_secs is ignored: faber no longer restarts Docker \
             Desktop (ADR-0026), so there is no \"unable to start\" to revive sooner. Remove \
             it from the file"
                .to_string()
        })
    }

    /// [`HostPolicy::prevent_sleep`] and the old bool, resolved. Both set is a
    /// config error ([`FileConfig::validate`]); the daemon, which must still boot,
    /// goes by the new key.
    pub fn sleep_policy(&self) -> SleepPolicy {
        resolve_sleep_policy(self.prevent_sleep, self.prevent_sleep_while_busy)
            .unwrap_or_else(|_| self.prevent_sleep.unwrap_or_default())
    }

    /// The boot warning for the old `prevent_sleep_while_busy` key, if it is set.
    pub fn deprecated_sleep_key_warning(&self) -> Option<String> {
        self.prevent_sleep_while_busy.map(|_| {
            "host.prevent_sleep_while_busy is deprecated: use host.prevent_sleep = \"off\" | \
             \"while_busy\" | \"always\" instead (true is \"while_busy\")"
                .to_string()
        })
    }

    /// Move the old bool to [`HostPolicy::prevent_sleep`], and state the key when
    /// neither is set, so the settings form shows what the daemon does and its
    /// first save writes it explicitly. Left alone when both are set: the form
    /// must not pick for the operator.
    pub fn migrate_sleep_key(&mut self) {
        if let Ok(mode) = resolve_sleep_policy(self.prevent_sleep, self.prevent_sleep_while_busy) {
            self.prevent_sleep = Some(mode);
            self.prevent_sleep_while_busy = None;
        }
    }

    /// [`HostPolicy::disk_admission_floor_gib`] resolved into its three states.
    pub fn disk_admission(&self) -> DiskAdmission {
        DiskAdmission::from_key(self.disk_admission_floor_gib)
    }

    /// [`HostPolicy::dind_requires_label`] resolved into its three states.
    pub fn dind_label_rule(&self) -> DindLabelRule {
        DindLabelRule::from_key(self.dind_requires_label)
    }

    /// `keep_memory_gib` and `keep_cpus`, as the machine's share (ADR-0021 §2).
    pub fn machine_share(&self) -> crate::machine::MachineShare {
        crate::machine::MachineShare {
            keep_memory_gib: self.keep_memory_gib,
            keep_cpus: self.keep_cpus,
        }
    }

    /// Why the two disk floors are in the wrong order, if they are: an admission
    /// floor above a non-zero reclamation floor stops CI before reclamation has
    /// started (ADR-0024 §1). A reclamation floor of `0` orders nothing — there
    /// is no reclamation for admission to wait behind.
    ///
    /// One sentence for both places that report it, so they cannot drift:
    /// [`FileConfig::validate`] rejects it where a human is looking, and boot only
    /// warns, since a file that arrived by unattended update must not stop the
    /// daemon (AGENTS.md).
    pub fn disk_floors_out_of_order(&self) -> Option<String> {
        let floor = self.disk_admission().floor_gib()?;
        let reclaim = self.disk_floor_gib;
        (reclaim != 0 && floor > reclaim).then(|| {
            format!(
                "host.disk_admission_floor_gib ({floor} GiB) is above host.disk_floor_gib \
                 ({reclaim} GiB): Docker admission would stop before disk reclamation had \
                 started — lower it to at most {reclaim}, or raise disk_floor_gib"
            )
        })
    }
}

/// `[host] disk_admission_floor_gib`, resolved (ADR-0024 §1).
///
/// Tightens admission, so per AGENTS.md an absent key keeps the old behaviour:
/// [`DiskAdmission::Unstated`] behaves as [`DiskAdmission::Off`], but is warned
/// about at boot and shown as its own word, so "an operator chose no gate" and
/// "nobody has said" stay distinguishable until the default flips in a
/// release-noted change. The same shape as [`CpuMode`] and the `dind` grant.
///
/// The file itself keeps a plain optional number, which is what an operator
/// writes; this is only how the code reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiskAdmission {
    /// Start no Docker job while the Docker volume has less than this many GiB
    /// free — counting what recent admissions have yet to write — and do not
    /// restart Docker onto it.
    Floor(u64),
    /// `0`: no gate, stated.
    Off,
    /// The key is absent. Behaves as `Off`.
    #[default]
    Unstated,
}

impl DiskAdmission {
    /// Read the key: absent is unstated, `0` is off, anything else a floor.
    pub fn from_key(gib: Option<u64>) -> Self {
        match gib {
            None => Self::Unstated,
            Some(0) => Self::Off,
            Some(gib) => Self::Floor(gib),
        }
    }

    /// The floor in force, in GiB — `None` unless the gate is on.
    pub fn floor_gib(self) -> Option<u64> {
        match self {
            Self::Floor(gib) => Some(gib),
            Self::Off | Self::Unstated => None,
        }
    }

    pub fn is_unstated(self) -> bool {
        matches!(self, Self::Unstated)
    }

    /// A short, stable word for logs and the dashboard: `on`, `off` or `unstated`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Floor(_) => "on",
            Self::Off => "off",
            Self::Unstated => "unstated",
        }
    }
}

/// `[host] prevent_sleep` — issue #90, see [`crate::awake`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SleepPolicy {
    /// faber never holds a sleep assertion. The default.
    #[default]
    Off,
    /// Held while any admitted runner is up, released after an idle grace.
    WhileBusy,
    /// Held from boot for the daemon's whole life, jobs or not.
    Always,
}

impl SleepPolicy {
    /// The config spelling, for logs and `/api/state`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::WhileBusy => "while_busy",
            Self::Always => "always",
        }
    }
}

/// Resolve `[host] prevent_sleep` and the old `prevent_sleep_while_busy` bool.
/// Neither set is `off`; the old bool alone maps `true` to `while_busy` and
/// `false` to `off`; both set is an error.
pub fn resolve_sleep_policy(
    new: Option<SleepPolicy>,
    old: Option<bool>,
) -> Result<SleepPolicy, String> {
    match (new, old) {
        (Some(_), Some(_)) => Err(
            "host.prevent_sleep and host.prevent_sleep_while_busy are both \
             set: keep host.prevent_sleep and remove the old key"
                .to_string(),
        ),
        (Some(mode), None) => Ok(mode),
        (None, Some(true)) => Ok(SleepPolicy::WhileBusy),
        (None, Some(false) | None) => Ok(SleepPolicy::Off),
    }
}

/// `[host] dind_requires_label`, resolved (ADR-0026 § Consequences).
///
/// Withdraws a daemon from jobs that do not ask for one, so per AGENTS.md an
/// absent key keeps the old behaviour: [`DindLabelRule::Unstated`] behaves as
/// [`DindLabelRule::Off`], but is warned about at boot and shown as its own word
/// until the cut's host sitting states it. The same shape as [`DiskAdmission`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DindLabelRule {
    /// `true`: a daemon only for a job whose `runs-on` includes `dind`, on a
    /// profile that grants one.
    On,
    /// `false`: every job on a granting profile gets a daemon, stated.
    Off,
    /// The key is absent. Behaves as `Off`.
    #[default]
    Unstated,
}

impl DindLabelRule {
    /// Read the key: absent is unstated.
    pub fn from_key(v: Option<bool>) -> Self {
        match v {
            None => Self::Unstated,
            Some(false) => Self::Off,
            Some(true) => Self::On,
        }
    }

    /// Whether a job must carry the `dind` label to get a daemon.
    pub fn requires_label(self) -> bool {
        matches!(self, Self::On)
    }

    pub fn is_unstated(self) -> bool {
        matches!(self, Self::Unstated)
    }

    /// A short, stable word for logs and the dashboard: `on`, `off` or `unstated`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
            Self::Unstated => "unstated",
        }
    }
}

/// The strict schema of a watched repo's in-repo `.faber/config.toml`.
///
/// `deny_unknown_fields` plus the deliberate *absence* of any trust / gate-
/// routing / backend / image field make this type **structurally incapable** of
/// weakening faber's own trust decisions: a PR-reachable config can only tune
/// drift surfacing, never widen what runs on the host. It is read at the job's
/// head ref and, per [`RepoConfig::resolve`], wins field-by-field over the global
/// [`DriftPolicy`]. See ADR-0001.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoConfig {
    /// The one governed toolchain in Phase 1: Playwright.
    #[serde(default)]
    pub playwright: Option<RepoPlaywright>,
}

/// The `[playwright]` table of a repo's `.faber/config.toml`. Every field is
/// optional so a repo overrides only what it means to; unset fields fall back to
/// the global default (or, for repo-only fields, their own default).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoPlaywright {
    /// Override the global surfacing mode.
    #[serde(default)]
    pub mode: Option<DriftMode>,
    /// **Withdrawn** — see [`DriftPolicy::auto_pr`]. Parsed so a repo that still
    /// ships the key keeps its *other* overrides (a malformed `.faber/config.toml`
    /// degrades to the global default, silently dropping `mode` and `heal` with
    /// it), but ignored.
    #[serde(default)]
    pub auto_pr: Option<bool>,
    /// Opt *out* of on-demand image healing (ADR-0008) for this repo, leaving it
    /// with plain detection. Default (absent) is `true` — a repo that bakes the
    /// opt-in label is healed wherever the operator enabled `[runner_image]`.
    /// There is deliberately no way to opt *in* from here: the build runs on the
    /// operator's host, so enabling it is the operator's call.
    #[serde(default)]
    pub heal: Option<bool>,
    /// **Withdrawn** — the Dockerfile an auto-PR would have bumped. Parsed and
    /// ignored for the same reason as [`RepoPlaywright::auto_pr`]. These
    /// coordinates presumed the runner image was built *in* the watched repo;
    /// it is built off-repo, so there was never a file here to bump.
    #[serde(default)]
    pub dockerfile: Option<String>,
    /// **Withdrawn** — the build-arg an auto-PR would have bumped. See
    /// [`RepoPlaywright::dockerfile`].
    #[serde(default)]
    pub arg: Option<String>,
    /// Lockfile ecosystem to read the pin from. Only `npm` is wired in Phase 1;
    /// any other value parses (so a repo can declare intent) but skips the check.
    #[serde(default)]
    pub ecosystem: Option<String>,
}

/// The effective drift policy for one repo: the global [`DriftPolicy`] with the
/// repo's in-repo override applied field-by-field (repo-wins).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedPolicy {
    pub mode: DriftMode,
    /// Whether this repo accepts on-demand healing (ADR-0008). Repo opt-out only;
    /// it means nothing unless the operator turned `[runner_image]` on.
    pub heal: bool,
    /// Lockfile ecosystem (default `npm`).
    pub ecosystem: String,
}

impl RepoConfig {
    /// Resolve the effective policy for one job: the repo's `[playwright]`
    /// overrides win over the global `[playwright_drift]` default, field by field.
    /// `repo` is `None` when the repo commits no `.faber/config.toml` (or it was
    /// unreadable — the caller degrades to the global default).
    pub fn resolve(global: &DriftPolicy, repo: Option<&RepoConfig>) -> ResolvedPolicy {
        let pw = repo.and_then(|r| r.playwright.as_ref());
        ResolvedPolicy {
            mode: pw.and_then(|p| p.mode).unwrap_or(global.mode),
            heal: pw.and_then(|p| p.heal).unwrap_or(true),
            ecosystem: pw
                .and_then(|p| p.ecosystem.clone())
                .unwrap_or_else(|| "npm".to_string()),
        }
    }
}

/// The platform facts [`dind_needs_wall`] judges a grant against, bundled so a
/// future change touches exactly one place instead of every call site.
///
/// `holds_credential` and `jobs_behind_wall` are fields rather than
/// hard-coded, because PR2 (ADR-0026) changes exactly one of them: once jobs
/// run inside faber's own VM, [`WallFacts::current`] starts returning
/// `jobs_behind_wall: true` and the predicate stops firing with no change to
/// any call site. `is_linux` is likewise a field (`cfg!(target_os = "linux")`
/// inside [`WallFacts::current`], an injected value in tests) rather than read
/// from `cfg!` inside the predicate, so it compiles — and is exercised — on
/// every OS. Before this existed, `dind_needs_wall(cfg!(target_os = "linux"),
/// true, false, grant)` was copy-pasted at every call site, and ADR-0026 would
/// have had to find and update each one by hand.
#[derive(Debug, Clone, Copy)]
pub struct WallFacts {
    pub is_linux: bool,
    pub holds_credential: bool,
    pub jobs_behind_wall: bool,
    /// Whether ADR-0051's session account exists here, asked only when a file
    /// turns macOS VMs on ([`FileConfig::validate`]).
    pub vm_session_account: fn() -> bool,
}

impl WallFacts {
    /// This host's real facts, today (PR1). Every host that claims jobs holds
    /// a GitHub credential by construction, and PR1 runs no jobs behind a wall
    /// of their own yet. PR2 (ADR-0026) changes `jobs_behind_wall` here, and
    /// only here, once jobs run inside faber's own VM.
    pub fn current() -> Self {
        Self {
            is_linux: cfg!(target_os = "linux"),
            holds_credential: true,
            jobs_behind_wall: false,
            vm_session_account: session_account_here,
        }
    }
}

/// Whether ADR-0051's session account exists on this host. Only a Mac has one.
fn session_account_here() -> bool {
    #[cfg(target_os = "macos")]
    {
        crate::setup::accounts::probe_session_account(
            crate::platform::layout::Os::HOST,
            &mut crate::setup::tools::SystemRunner,
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Whether AGENTS.md's Linux dind wall (ADR-0025 §"On Linux, dind waits for a
/// wall") must warn/reject for a profile with this Docker grant.
///
/// A granted dind is a privileged container on the host's own Docker Engine —
/// root on the machine (ADR-0013). On macOS that root is confined to Docker
/// Desktop's VM; on Linux there is no VM, so no part of faber's own account
/// split (ADR-0025) keeps it away from `_faber`'s files, or anything else on
/// the host. Fires for [`DindGrant::Granted`] **and** [`DindGrant::Unstated`],
/// via [`DindGrant::granted`] — an absent key behaves as granted (see
/// [`FileProfile::dind`]'s doc above), so a config that has never mentioned
/// `dind` on a Linux host is exactly as exposed as one that turned it on
/// explicitly. Never fires for [`DindGrant::Denied`]: no daemon ever starts.
pub fn dind_needs_wall(facts: WallFacts, grant: DindGrant) -> bool {
    facts.is_linux && facts.holds_credential && !facts.jobs_behind_wall && grant.granted()
}

/// ADR-0051: the VM layout keys this host has nothing for, one warning each, for
/// the boot and `faber check-config`. Never an error: the file is valid, and a
/// host it does not apply to runs as if the key were absent.
///
/// - `[macos_vm]` on Linux, which runs no macOS VMs.
/// - `[linux_vm] enabled = false` where `has_engine` says Docker is the host's own
///   (Linux, or a Mac not set up with `sudo faber setup`): there is no Linux VM of
///   faber's to turn off, and docker profiles keep running.
pub fn vm_layout_warnings(cfg: &FileConfig, is_linux: bool, has_engine: bool) -> Vec<String> {
    let mut out = Vec::new();
    if is_linux && !cfg.macos_vm.is_empty() {
        out.push(
            "[macos_vm] does not apply on this host: Linux runs no macOS VMs (ADR-0051), so \
             its keys do nothing here; remove the section"
                .to_string(),
        );
    }
    if !cfg.linux_vm.runs() && !has_engine {
        out.push(
            "[linux_vm] enabled = false does nothing on this host: its Docker is the host's \
             own, not faber's Linux VM, which only a Mac set up with `sudo faber setup` runs \
             (ADR-0051); docker profiles still run here"
                .to_string(),
        );
    }
    out
}

/// ADR-0051: the profiles this host's VM layout ([`VmLayout`]) starts no VM for,
/// dropped, not refused: jobs routed to them find no runner here, which leaves them
/// to another host, and the boot says so loudly rather than claiming jobs it cannot
/// run.
///
/// - `[linux_vm] enabled = false` drops the `docker` profiles, where `has_engine`
///   says Docker is faber's own Linux VM (a set-up Mac). Elsewhere Docker is the
///   host's, and the key does nothing.
/// - `[macos_vm] count = 0` drops the `macos-vm` profiles.
pub fn ignore_vm_layout(
    profiles: Vec<Profile>,
    layout: VmLayout,
    has_engine: bool,
) -> (Vec<Profile>, VmLayoutDrops) {
    let mut drops = VmLayoutDrops::default();
    let kept = profiles
        .into_iter()
        .filter(|p| match p.backend {
            Backend::Docker(_) if has_engine && !layout.linux_vm => {
                drops.docker.push(p.name.clone());
                false
            }
            Backend::MacosVm(_) if layout.macos_vms == 0 => {
                drops.macos.push(p.name.clone());
                false
            }
            _ => true,
        })
        .collect();
    (kept, drops)
}

/// The profiles [`ignore_vm_layout`] dropped, by name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VmLayoutDrops {
    pub docker: Vec<String>,
    pub macos: Vec<String>,
}

impl VmLayoutDrops {
    /// The boot warnings and dashboard banners for what was dropped, as
    /// `(notice kind, sentence)`: `linux_vm_off` and `macos_vms_off`.
    pub fn notices(&self) -> Vec<(&'static str, String)> {
        let mut out = Vec::new();
        if !self.docker.is_empty() {
            out.push((
                "linux_vm_off",
                format!(
                    "faber's Linux VM is off ([linux_vm] enabled = false, ADR-0051), so docker \
                     profile{} {} get{} no runner on this host. The VM's disk and the runner \
                     image in it are kept; remove the key to turn it back on.",
                    plural_s(self.docker.len()),
                    self.docker.join(", "),
                    if self.docker.len() == 1 { "s" } else { "" },
                ),
            ));
        }
        if !self.macos.is_empty() {
            out.push((
                "macos_vms_off",
                format!(
                    "macOS VMs are off ([macos_vm] count = 0, ADR-0051), so macos-vm profile{} \
                     {} get{} no runner on this host.",
                    plural_s(self.macos.len()),
                    self.macos.join(", "),
                    if self.macos.len() == 1 { "s" } else { "" },
                ),
            ));
        }
        out
    }
}

fn plural_s(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Why `[registry_auth] enabled = true` is refused, and what a booting daemon
/// says instead of acting on it. One sentence for both, so the save, the
/// `check-config` line and the boot log cannot drift apart.
pub const REGISTRY_AUTH_HELD: &str =
    "registry_auth is held: a credential mounted into a job can be read by the job's code, \
     so faber mounts none and pulls anonymously; it returns when faber pulls images on the \
     job's behalf. Set registry_auth.enabled = false or remove the section.";

/// The boot warning for `[registry_auth] enabled = true`. `None` when it is off.
///
/// [`FileConfig::validate`] refuses the switch, so a config that carries it came
/// from a hand edit or an earlier release. Such a daemon still boots (AGENTS.md:
/// never refuse a config the release did not write), resolves no credential,
/// fills and mounts no volume and sets no `DOCKER_CONFIG`, and says so at every
/// boot. The section is never carried into a profile, so this is the whole of
/// what a held section does.
pub fn registry_auth_held_warning(auth: &RegistryAuth) -> Option<String> {
    auth.enabled.then(|| REGISTRY_AUTH_HELD.to_string())
}

/// The boot warning, and `faber check-config`'s, for `[host] recreatable_projects`
/// / `recreatable_volumes` when either is set. `None` when both are empty.
///
/// The keys still parse, so a config that carries them boots as before, but they
/// do nothing: ADR-0016's reclamation named the operator's Docker Desktop compose
/// projects and volumes, and faber no longer touches Docker Desktop (ADR-0026).
/// A warning and never an error: an error would hold an unattended update back
/// (ADR-0006) over keys the operator can still remove by hand.
pub fn recreatable_ignored_warning(host: &HostPolicy) -> Option<String> {
    let set: Vec<&str> = [
        ("recreatable_projects", host.recreatable_projects.is_empty()),
        ("recreatable_volumes", host.recreatable_volumes.is_empty()),
    ]
    .into_iter()
    .filter(|(_, empty)| !empty)
    .map(|(key, _)| key)
    .collect();
    if set.is_empty() {
        return None;
    }
    Some(format!(
        "[host] {} ignored: faber no longer reclaims disk from Docker Desktop's compose \
         projects and volumes (ADR-0026 supersedes ADR-0016). The keys do nothing; remove \
         them from the config.",
        set.join(" and ")
    ))
}

impl FileConfig {
    /// The `[services.*]` tables, resolved into the policy the provisioning path
    /// reads (ADR-0046 §2).
    ///
    /// A table with any [`ServiceFile::errors`] is left out, so the service it
    /// would provide is refused as though the table were absent: closed, not
    /// guessed at. `check-config` and a settings save refuse such a table, and
    /// boot warns about it ([`services_ignored_warning`]), since the daemon
    /// itself never refuses to start over one.
    /// The `[mirror]` section, resolved: `None` when it is absent, which is the
    /// feature off, and also when it is invalid, which [`mirror_warnings`] says
    /// at boot (a daemon does not refuse to start on it, AGENTS.md).
    pub fn mirror_policy(&self) -> Option<crate::mirror::MirrorPolicy> {
        let m = self.mirror.as_ref()?;
        m.errors()
            .is_empty()
            .then_some(crate::mirror::MirrorPolicy {
                max_size_gib: m.max_size_gib,
                lfs: m.lfs.as_ref().map(|l| l.fill),
                npm: m.npm.as_ref().map(|n| n.fill),
            })
    }

    /// Everything wrong with the `[services.*]` tables: each table's own
    /// ([`ServiceFile::errors`]), then the ones across tables (ADR-0046 A2): two
    /// tables exporting the same `job_env` name would give a job declaring both
    /// whichever was written last, so each is refused. Built-in defaults count.
    /// A table with an error is left out of the policy.
    pub fn services_errors(&self) -> Vec<String> {
        self.services_errors_by_table()
            .into_values()
            .flatten()
            .collect()
    }

    /// [`Self::services_errors`], under the table each belongs to. A table
    /// with no entry is sound.
    fn services_errors_by_table(&self) -> BTreeMap<String, Vec<String>> {
        let mut errs: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut sound: Vec<&String> = Vec::new();
        for (name, f) in &self.services {
            let own = f.errors(name);
            if own.is_empty() {
                sound.push(name);
            } else {
                errs.insert(name.clone(), own);
            }
        }
        let mut exporters: BTreeMap<String, Vec<&String>> = BTreeMap::new();
        for name in sound {
            for var in self.services[name].exported_job_env(name).keys() {
                exporters.entry(var.clone()).or_default().push(name);
            }
        }
        for (var, tables) in exporters.iter().filter(|(_, t)| t.len() > 1) {
            for t in tables {
                let others: Vec<String> = tables
                    .iter()
                    .filter(|o| o != &t)
                    .map(|o| format!("services.{o}"))
                    .collect();
                errs.entry((*t).clone()).or_default().push(format!(
                    "services.{t}: job_env {var} is also exported by {} — a job declaring both \
                     would get whichever was written last",
                    others.join(", ")
                ));
            }
        }
        errs
    }

    pub fn services_policy(&self) -> crate::services::ServicesPolicy {
        use crate::services::{PerJobPolicy, PostgresPolicy, ServicesPolicy, POSTGRES};
        let mut policy = ServicesPolicy::default();
        let refused = self.services_errors_by_table();
        for (name, f) in &self.services {
            if refused.contains_key(name) {
                continue;
            }
            if name == POSTGRES {
                policy.postgres = Some(PostgresPolicy {
                    versions: f
                        .versions
                        .as_deref()
                        .and_then(|v| parse_service_versions(v).ok())
                        .unwrap_or_default(),
                    image: f.image.clone(),
                    extensions: f.extensions.clone(),
                    memory_mib: f.memory_mib,
                    idle_stop: Duration::from_secs(
                        f.idle_stop_secs.unwrap_or(SERVICE_IDLE_STOP_SECS),
                    ),
                });
            } else {
                policy.per_job.insert(
                    name.clone(),
                    PerJobPolicy {
                        image: f.image.clone(),
                        memory_mib: f.memory_mib,
                        args: f.args.clone(),
                        env: f.env.clone(),
                        job_env: f.exported_job_env(name),
                        health_cmd: f.health_cmd.clone(),
                    },
                );
            }
        }
        policy
    }

    /// The default config path: `~/Library/Application Support/faber/config.toml`.
    pub fn default_path() -> Option<PathBuf> {
        crate::platform::config_path()
    }

    /// Load the config. With an explicit `path`, a missing file is an error. With
    /// no path, the default location is tried and `Ok(None)` means "no config —
    /// fall back to CLI flags".
    pub fn load(explicit: Option<&Path>) -> anyhow::Result<Option<Self>> {
        let (path, required) = match explicit {
            Some(p) => (p.to_path_buf(), true),
            None => match Self::default_path() {
                Some(p) => (p, false),
                None => return Ok(None),
            },
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Self::load_from_str(&text, &path.display().to_string()).map(Some),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => Ok(None),
            Err(e) => Err(e).with_context(|| format!("reading config {}", path.display())),
        }
    }

    /// The most of a config [`Self::load_from_reader`] reads. A real config is
    /// a few KiB; a longer stream is refused, not buffered.
    pub const READ_CAP: usize = 1 << 20;

    /// Load a config from `reader`, parsed exactly as [`Self::load`] parses a
    /// file, reading at most [`Self::READ_CAP`] bytes. For `check-config
    /// --config -`: the process's own stdin, read through the descriptor it
    /// already holds, never by opening a path, so a process that has dropped
    /// to another account reads a pipe root created without a permission
    /// check. `source` names the stream in errors ("from stdin").
    pub fn load_from_reader(reader: impl std::io::Read, source: &str) -> anyhow::Result<Self> {
        use std::io::Read;
        let mut bytes = Vec::new();
        reader
            .take(Self::READ_CAP as u64 + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("reading config {source}"))?;
        if bytes.len() > Self::READ_CAP {
            anyhow::bail!(
                "config {source} is larger than 1 MiB, more than any faber config; refusing it"
            );
        }
        let text = String::from_utf8(bytes)
            .map_err(|_| anyhow::anyhow!("reading config {source}: it is not UTF-8"))?;
        Self::load_from_str(&text, source)
    }

    /// Parse `text` as a config: the one parse [`Self::load`] and
    /// [`Self::load_from_reader`] share. `source` names it in errors.
    pub fn load_from_str(text: &str, source: &str) -> anyhow::Result<Self> {
        let mut cfg: Self =
            toml::from_str(text).with_context(|| format!("parsing config {source}"))?;
        if let Some(m) = cfg.mirror.as_mut() {
            m.implicit = mirror_table_is_implicit(text);
        }
        Ok(cfg)
    }

    /// The path a given `--config` flag resolves to: the explicit path, else the
    /// default app-support location.
    pub fn resolve_path(explicit: Option<&Path>) -> Option<PathBuf> {
        match explicit {
            Some(p) => Some(p.to_path_buf()),
            None => Self::default_path(),
        }
    }

    /// Everything *structurally* wrong with this config, as operator-readable
    /// sentences: parsing and shape only — would this file let the daemon start
    /// and do its job. Empty means it is safe to write, in that narrow sense.
    ///
    /// Deliberately never rejects on policy — see [`Self::validate`], which adds
    /// that on top. This half is `pub` so a host-updater gate (`check-config
    /// --for-update`, a later slice) can confirm an incoming release's config
    /// will still *start*, without also holding it to a policy bar (such as
    /// AGENTS.md's Linux dind wall) that only widens over time and must never be
    /// the reason an unattended update bricks a running host (AGENTS.md's
    /// shipping-a-fix rule: a release must not withdraw a capability a host
    /// already relies on).
    ///
    /// This is also the guard on the settings API: the daemon reads its config
    /// exactly once, at startup, so an invalid file written from the UI would
    /// not fail until the *next* restart — long after whoever saved it could
    /// connect the two. Every check below is something that would make that
    /// restart fail, or silently produce a daemon that can't do its job.
    pub fn validate_structural(&self) -> Vec<String> {
        let mut errs = Vec::new();

        if self.topic.trim().is_empty() {
            errs.push("topic must not be empty — it is what opts a repo into faber".into());
        }
        if self.poll_interval_secs == 0 {
            errs.push("poll_interval_secs must be at least 1".into());
        }

        // The autoupdate knobs are only checked for shapes that would make the
        // updater unable to work at all. A wrong-but-valid repo is not something
        // this can catch — it surfaces as "no releases found" on the panel.
        if self.update.repo.split('/').count() != 2
            || self
                .update
                .repo
                .split('/')
                .any(|part| part.trim().is_empty())
        {
            errs.push("update.repo must be owner/repo".into());
        }
        if self.update.poll_interval_secs < 60 {
            errs.push("update.poll_interval_secs must be at least 60 — a tighter loop cannot notice a release any sooner than the host can install one".into());
        }
        if self.update.probation_secs < 10 {
            errs.push(
                "update.probation_secs must be at least 10 — a shorter probation clears the boot marker before a broken binary has had time to fail".into(),
            );
        }
        if self.update.maturity_secs < MIN_MATURITY_SECS {
            errs.push(format!(
                "update.maturity_secs must be at least {MIN_MATURITY_SECS} (one hour) — the wait is how a release the owner did not make gets caught before any host installs it (ADR-0031)"
            ));
        }
        if let Some(pin) = &self.update.pin {
            if crate::update::parse_version(pin).is_none() {
                errs.push(format!("update.pin '{pin}' is not a version like 0.2.0"));
            }
        }

        // The gate boundary can only ever narrow (`TrustPolicy::with_boundary`
        // intersects with the ceiling), so an out-of-range event is never a
        // security hole — it is a *typo*, and a silent one: `puhs` would simply
        // stop provisioning every push job with no clue why. Reject it here,
        // where the operator is looking at the file.
        for e in &self.allowed_events_unknown() {
            errs.push(format!(
                "gate.allowed_events: '{e}' is not an event faber provisions for — pick from {}",
                crate::gate::MAX_EVENTS.join(", ")
            ));
        }
        // Not an error: "provision nothing" is the most restrictive setting there
        // is, and refusing to save it would mean the one direction the operator
        // can always safely go is the one the validator blocks. It is warned about
        // at startup instead.
        //
        // An empty profile list is the one config that parses but cannot start:
        // `into_profiles` bails, so saving it would brick the next restart.
        if self.profiles.is_empty() {
            errs.push("at least one profile is required".into());
        }
        for p in &self.profiles {
            if p.git_mirror && p.backend != BackendKind::Docker {
                errs.push(format!(
                    "profile '{}': git_mirror needs a docker backend — only a container can \
                     mount the mirror (ADR-0035)",
                    p.name
                ));
            }
            if p.mirror && p.backend != BackendKind::Docker {
                errs.push(format!(
                    "profile '{}': mirror needs a docker backend — only a container can join \
                     the mirror's network (docs/mirrors.md)",
                    p.name
                ));
            }
            // Root refuses to render past a drop it cannot read (fail closed:
            // the operator meant to narrow), so a malformed one is refused here,
            // where they are looking, not at the next `sudo faber grant`.
            for spec in &p.drop_grants {
                if let Err(e) = crate::wall::grant_edit::parse_drop_spec(spec) {
                    errs.push(format!("profile '{}': drop_grants: {e}", p.name));
                }
            }
        }
        // ADR-0033 §2: an enum, not a free percentile. A typo here would run as
        // "max" with only a boot warning to say so, so reject it where the
        // operator is looking.
        if let Some(v) = self.autolimit.reserve.unknown() {
            errs.push(format!(
                "autolimit.reserve must be \"max\" or \"p75\", not {v:?} (ADR-0033)"
            ));
        }
        if !self.host.instance.is_empty() && !crate::instance::valid(&self.host.instance) {
            errs.push(format!(
                "host.instance {:?} must be a lowercase letter, then up to {} lowercase letters \
                 or digits — it ends every runner name this host mints",
                self.host.instance,
                crate::instance::MAX_LEN - 1
            ));
        }
        // Rejected rather than warned: the operator is looking at the file. A pool that does not
        // parse would otherwise be discovered as a *provisioning* failure on
        // every job, which is precisely the mystifying-error class this key
        // exists to end.
        if !self.host.network_pool.trim().is_empty() {
            if let Err(e) = crate::netpool::NetPool::declare(
                self.host.network_pool.trim(),
                self.host.network_subnet_size,
            ) {
                errs.push(format!("host.network_pool: {e}"));
            }
        }
        // ADR-0024 §1: admission stopping above the point reclamation starts would
        // park CI on a host faber had not yet tried to clear. Rejected here, where
        // the operator is looking; boot only warns about the same file.
        if let Some(e) = self.host.disk_floors_out_of_order() {
            errs.push(e);
        }
        // ADR-0026: the engine's size, in the range its `start` must carry.
        errs.extend(self.linux_vm.errors());
        // ADR-0051: the macOS VM layout's own ranges. That no profile's VM outgrows
        // it is policy, checked beside the session account (`vm_layout_host_errors`).
        errs.extend(self.macos_vm.errors());
        // The mirror's size limit. A boot on 0 warns and keeps the mirror off
        // (`mirror_policy`); refusing it is for where a human is looking.
        if let Some(m) = &self.mirror {
            errs.extend(m.errors());
        }
        // ADR-0024 §4. A boot on this warns and ages nothing (see
        // `crate::cache::boot_warning`); refusing it is for where a human is
        // looking, like the checks above.
        if let Some(days) = self.host.cache_max_age_days {
            if days > 0 && days < crate::cache::MIN_MAX_AGE_DAYS {
                errs.push(format!(
                    "host.cache_max_age_days must be 0 (no age limit) or at least {} — the Docker VM \
                     mounts its disk relatime, so a read refreshes an entry's atime at most \
                     once a day and {days} would delete entries still read every day",
                    crate::cache::MIN_MAX_AGE_DAYS
                ));
            }
        }
        // ADR-0055. `cache_max_gib` is a `u64`: 0 (no limit) and every positive
        // value mean what they say, so there is nothing to refuse.
        let mut seen_names: Vec<&str> = Vec::new();
        let mut seen_labels: Vec<Vec<String>> = Vec::new();
        for (i, p) in self.profiles.iter().enumerate() {
            let who = if p.name.trim().is_empty() {
                format!("profile #{}", i + 1)
            } else {
                format!("profile '{}'", p.name)
            };
            if p.name.trim().is_empty() {
                errs.push(format!("{who}: name must not be empty"));
            } else if seen_names.contains(&p.name.as_str()) {
                errs.push(format!("{who}: duplicate profile name"));
            } else {
                seen_names.push(&p.name);
            }

            if p.backend == BackendKind::Native {
                errs.push(native_profile_error(&who));
            }
            if p.labels.is_empty() {
                errs.push(format!("{who}: at least one label is required"));
            }
            if p.labels.iter().any(|l| l.trim().is_empty()) {
                errs.push(format!("{who}: labels must not be blank"));
            }
            // Routing takes the first profile whose labels a job matches, so an
            // exact duplicate set makes this profile permanently unreachable.
            let mut sorted = p.labels.clone();
            sorted.sort();
            if seen_labels.contains(&sorted) {
                errs.push(format!(
                    "{who}: same label set as an earlier profile — it would never be selected"
                ));
            } else {
                seen_labels.push(sorted);
            }

            if matches!(p.backend, BackendKind::Docker)
                && p.image.as_deref().map(str::trim).unwrap_or("").is_empty()
            {
                errs.push(format!("{who}: a docker profile requires an image"));
            }
            if p.env.keys().any(|k| k.trim().is_empty()) {
                errs.push(format!("{who}: env names must not be blank"));
            }
            if matches!(p.backend, BackendKind::MacosVm) {
                errs.extend(
                    macos_vm_errors(p)
                        .into_iter()
                        .map(|e| format!("{who}: {e}")),
                );
            } else if p.cpus.is_some() || p.memory_mib.is_some() {
                errs.push(format!(
                    "{who}: cpus and memory_mib belong to a macos-vm profile"
                ));
            }
        }

        let a = &self.autolimit;
        if a.margin < 1.0 {
            errs.push("autolimit.margin must be at least 1.0 (it is a safety multiplier)".into());
        }
        if a.window == 0 {
            errs.push("autolimit.window must be at least 1".into());
        }
        if a.native_concurrency == 0 {
            errs.push("autolimit.native_concurrency must be at least 1".into());
        }
        if a.profiling_cap_mib == 0 {
            errs.push("autolimit.profiling_cap_mib must be greater than 0".into());
        }
        if a.headroom_mib == 0 {
            errs.push("autolimit.headroom_mib must be greater than 0".into());
        }
        if !(a.escalation_frac > 0.0 && a.escalation_frac <= 1.0) {
            errs.push("autolimit.escalation_frac must be between 0 and 1".into());
        }
        for (i, p) in a.priority_repos.iter().enumerate() {
            let p = p.trim();
            if p.split_once('/')
                .is_none_or(|(o, r)| o.is_empty() || r.is_empty() || r.contains('/'))
            {
                errs.push(format!(
                    "autolimit.priority_repos[{i}] must be written owner/repo"
                ));
            }
        }
        if a.probe_image.trim().is_empty() {
            errs.push("autolimit.probe_image must not be empty".into());
        }
        // Zero was once how an operator stopped admission, and it reads as a
        // healthy host that simply starts nothing. Stopping admission is a hold
        // now, which says why (issue #66) — so point there rather than at "off",
        // which is the opposite of what someone typing 0 wants.
        if a.hard_ceiling == HardCeiling::Fixed(0) {
            errs.push(
                "autolimit.hard_ceiling must be at least 1 (\"off\" removes the ceiling) — to stop \
                 Docker admission, hold it from the dashboard or POST /api/hold"
                    .into(),
            );
        }
        // ADR-0019's knobs are checked in every `cpu` mode, so a file that is valid
        // while CPU admission is off stays valid the day it is turned on. Written as
        // negated ranges so a NaN fails them too; infinities are refused because the
        // budget and the `--cpus` quota are derived from these.
        if !(a.cpu_overcommit.is_finite() && a.cpu_overcommit > 0.0) {
            errs.push("autolimit.cpu_overcommit must be greater than 0".into());
        }
        if !(a.reserve_cpus.is_finite() && a.reserve_cpus >= 0.0) {
            errs.push("autolimit.reserve_cpus must be 0 or more".into());
        }
        if !(a.profiling_cpus.is_finite() && a.profiling_cpus > 0.0) {
            errs.push("autolimit.profiling_cpus must be greater than 0".into());
        }
        if !(a.cpu_pause_pressure > 0.0 && a.cpu_pause_pressure <= 100.0) {
            errs.push("autolimit.cpu_pause_pressure must be above 0 and at most 100 (%)".into());
        }
        if !(a.cpu_resume_pressure >= 0.0 && a.cpu_resume_pressure < a.cpu_pause_pressure) {
            errs.push(
                "autolimit.cpu_resume_pressure must be at least 0 and below cpu_pause_pressure \
                 (the gap is what stops admission flapping)"
                    .into(),
            );
        }
        if self.retry.enabled && self.retry.lookback_secs == 0 {
            errs.push("retry.lookback_secs must be greater than 0 while retry is enabled".into());
        }
        // A `versions` string mixing range operators and exact versions has no
        // sane interpretation (ADR-0010 §4) — refused here rather than guessed
        // at by `needs_allow`, which can only fail the whole entry closed.
        // Sorted so the reported order doesn't depend on HashMap iteration.
        let mut needs_tools: Vec<&String> = self.needs.keys().collect();
        needs_tools.sort();
        for tool in needs_tools {
            let versions = &self.needs[tool].versions;
            if matches!(classify_needs_versions(versions), NeedsVersionForm::Mixed) {
                errs.push(format!(
                    "needs.{tool}: versions '{versions}' mixes range operators (>=, <=, >, <, =) \
                     with exact versions — use one form or the other"
                ));
            }
        }
        // ADR-0046 §2, §10. Boot leaves a bad table out of the policy and warns
        // ([`services_ignored_warning`]); here a human is looking.
        errs.extend(self.services_errors());

        errs
    }

    /// Everything wrong with this config: [`Self::validate_structural`] plus
    /// policy — checks that reject a config which would still *start*, because
    /// AGENTS.md's shipping-a-fix rule and this crate's own policy bar (today,
    /// the Linux dind wall) say it should not run this way. Used by both
    /// `check-config` and the settings-save API, so a hand-edited file and a
    /// UI-driven save are held to the exact same bar.
    pub fn validate(&self) -> Vec<String> {
        self.validate_with_facts(WallFacts::current())
    }

    /// [`Self::validate`], with [`WallFacts`] injected instead of read from the
    /// real compile target.
    ///
    /// Same shape `crate::platform` already uses for its own environment seam
    /// (issue #47's lesson generalized: keep the ambient fact a function reads
    /// out of the function under test, not just `std::env` vars) — it is what
    /// lets the Linux dind-wall policy below be exercised from a test built for
    /// any host OS, including this crate's own macOS CI leg.
    fn validate_with_facts(&self, facts: WallFacts) -> Vec<String> {
        let mut errs = self.validate_structural();
        // Policy, not structure: `[registry_auth]` is held, and a daemon that
        // boots with it on runs, mounts nothing and warns
        // ([`registry_auth_held_warning`]). Refused here, where a human is
        // looking, so neither `check-config` nor a settings save can turn it on.
        if self.registry_auth.enabled {
            errs.push(REGISTRY_AUTH_HELD.to_string());
        }
        // Policy, not structure: the updater's gate must not hold a release back
        // over it (AGENTS.md). Whatever the switch says: turning it on once the
        // section returns must not be the first time the name is looked at
        // (issue #191).
        if let Some(problem) = crate::docker::password_env_problem(&self.registry_auth.password_env)
        {
            errs.push(problem);
        }
        // Policy, not structure: the daemon boots on the new key alone.
        if let Err(e) =
            resolve_sleep_policy(self.host.prevent_sleep, self.host.prevent_sleep_while_busy)
        {
            errs.push(e);
        }
        // Policy, not structure: a name outside this set still starts (boot
        // only warns, `check-config --for-update` never asks), but a save or a
        // human-run `check-config` refuses it. A profile's name lands in the
        // `sudo faber config set profile.<name>.…` commands the UIs show.
        for p in &self.profiles {
            if !p.name.trim().is_empty() && !profile_name_ok(&p.name) {
                errs.push(profile_name_error(&p.name));
            }
        }
        // A list entry the settings form would refuse (`FieldSpec::item`) is
        // not judged here: an existing one must never block a save or a
        // `check-config` (which warns about it). The settings save refuses the
        // entries it adds (`crate::settings::new_item_errors`).
        // Policy, not structure: a service is declared with a tool's label
        // shape, and `[services.<name>]` decides it (ADR-0046 §10).
        // `needs::evaluate` skips these names, so an entry does nothing and the
        // daemon runs fine with one; an unattended update must not be held back
        // over it, but a save or a human-run `check-config` refuses it.
        let mut reserved: Vec<&String> = self
            .needs
            .keys()
            .filter(|t| SERVICE_NAMES.contains(&t.as_str()) || self.services.contains_key(*t))
            .collect();
        reserved.sort();
        for tool in reserved {
            errs.push(format!(
                "needs.{tool}: {tool} is a service, not a tool — declare it under \
                 [services.{tool}] (ADR-0046) and remove it from [needs]"
            ));
        }
        // Policy, not structure (ADR-0028, #238): an operator recipe only root
        // could have written. Refused here, where a human is looking; the boot
        // only warns, and each variant build from it is refused.
        if let Some(problem) = crate::image::recipe_problem(&self.runner_image) {
            errs.push(problem);
        }
        for p in &self.profiles {
            if !matches!(p.backend, BackendKind::Docker) {
                continue;
            }
            let grant = match p.dind {
                Some(true) => DindGrant::Granted,
                Some(false) => DindGrant::Denied,
                None => DindGrant::Unstated,
            };
            if dind_needs_wall(facts, grant) {
                let who = if p.name.trim().is_empty() {
                    "this profile".to_string()
                } else {
                    format!("profile '{}'", p.name)
                };
                errs.push(format!(
                    "{who}: Docker access on Linux has no account wall around it yet \
                     (ADR-0025) — a granted (or unstated) dind is root on this whole machine, \
                     not merely inside a VM as on macOS. Set dind = false, or run this host's \
                     jobs behind a wall once ADR-0026 ships. This will become a hard refusal in \
                     a later, release-noted version; check-config and the settings form reject \
                     it now so you see it while looking at the file."
                ));
            }
        }
        // Policy, not structure: a mirror that carries faber's token, beside a
        // profile whose jobs are root on the engine that holds it. The boot
        // leaves both mirrors off and says so instead (`mirror_beside_dind`).
        if let Some(problem) = crate::mirror_beside_dind::config_error(self) {
            errs.push(problem);
        }
        errs.extend(self.vm_layout_host_errors(facts.is_linux, facts.vm_session_account));
        // Policy too (ADR-0063 §6.2): a boot on such a file demotes instead.
        errs.extend(self.waste.problems());
        errs
    }

    /// ADR-0051's host rules for the VM layout. Policy, not structure: an
    /// unattended update is never held back over them, and a boot on such a file
    /// only finds that macOS VM jobs wait or run in a VM the share undercounts.
    ///
    /// - No `macos-vm` profile gives its VM more than `[macos_vm] memory_mib`, what
    ///   the share counts for each. Checked only once the key is written, so a file
    ///   that predates it passes as before.
    /// - Turning macOS VMs on needs the session account, which only `sudo faber
    ///   setup` creates. `session_account` is asked only when the file turns them
    ///   on, so a host that never does is never probed. Linux has no macOS VMs, so
    ///   the key does not apply there: [`vm_layout_warnings`] says so instead.
    fn vm_layout_host_errors(&self, is_linux: bool, session_account: fn() -> bool) -> Vec<String> {
        let mut errs = Vec::new();
        if let Some(charge) = self.macos_vm.memory_mib {
            for p in &self.profiles {
                let mib = p.memory_mib.unwrap_or(DEFAULT_VM_MEMORY_MIB);
                if matches!(p.backend, BackendKind::MacosVm) && mib > charge {
                    errs.push(format!(
                        "profile '{}': memory_mib {mib} is more than macos_vm.memory_mib \
                         {charge}, what the machine share counts for each macOS VM",
                        p.name
                    ));
                }
            }
        }
        if self.macos_vm.turned_on() && !is_linux && !session_account() {
            errs.push(format!(
                "macos_vm.count turns macOS VMs on, and they run in the login session of the \
                 account {} (ADR-0051), which does not exist on this host yet — run `sudo \
                 faber setup` to create it, or set macos_vm.count = 0",
                crate::platform::layout::FABER_MAC_ACCOUNT
            ));
        }
        errs
    }

    /// Render this config as the TOML faber would read back, with a header noting
    /// that the file is UI-managed.
    pub fn to_toml(&self) -> anyhow::Result<String> {
        let body = toml::to_string_pretty(self).context("serializing config")?;
        Ok(format!("{TOML_HEADER}{body}"))
    }

    /// Write this config to `path`, keeping the previous contents next to it as
    /// `<name>.bak`.
    ///
    /// A rewrite cannot preserve comments or hand-formatting (serde round-trips
    /// data, not layout), so the displaced file is kept verbatim — one undo step
    /// for anyone who had annotated theirs.
    ///
    /// The write is atomic: a temporary file with a name of its own is created
    /// exclusively beside `path` (never one left behind or planted there), with
    /// the permission bits of the file it replaces (`0644` for a first file; on
    /// Unix only), written, synced, renamed over `path`, and the directory synced (best effort). A
    /// crash mid-write leaves the old config intact rather than a truncated one
    /// the daemon would refuse to start from. The file is owned by whoever
    /// writes it: the daemon's account for a settings save, since the daemon
    /// does the writing for every caller, root included.
    pub fn save_to(&self, path: &Path) -> anyhow::Result<()> {
        self.validated()?;
        Self::write_text(path, &self.to_toml()?)
    }

    /// [`Self::save_to`], writing `text` verbatim instead of re-serialising:
    /// the raw editor's save (`POST /api/settings/raw`), which keeps the
    /// operator's comments and layout. `self` is what the caller judged, so
    /// `text` is parsed again here and refused unless it is `self`: what is
    /// validated and written is then one config, not two the caller hoped
    /// agree.
    pub(crate) fn save_text_to(&self, path: &Path, text: &str) -> anyhow::Result<()> {
        let parsed = Self::load_from_str(text, "to be written")?;
        let as_json = |c: &Self| serde_json::to_value(c).context("comparing configs");
        if as_json(&parsed)? != as_json(self)? {
            anyhow::bail!(
                "refusing to write a config whose text does not parse to the config judged"
            );
        }
        parsed.validated()?;
        Self::write_text(path, text)
    }

    /// `Ok` when [`Self::validate`] finds nothing; the refusal a write gives.
    fn validated(&self) -> anyhow::Result<()> {
        let errs = self.validate();
        if !errs.is_empty() {
            anyhow::bail!("refusing to write an invalid config: {}", errs.join("; "));
        }
        Ok(())
    }

    /// The write behind [`Self::save_to`] and [`Self::save_text_to`], once the
    /// config has been judged: `.bak`, then the atomic temp-and-rename.
    fn write_text(path: &Path, text: &str) -> anyhow::Result<()> {
        use std::io::Write;
        // `create_new` is O_CREAT|O_EXCL: it neither reuses nor follows
        // anything already at the temporary path.
        let dir = match path.parent() {
            Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
            _ => PathBuf::from("."),
        };
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        // The permission bits only: setuid, setgid and sticky never carry over.
        let mode: Option<u32> = match std::fs::metadata(path) {
            Ok(meta) => {
                let backup = path.with_extension("toml.bak");
                std::fs::copy(path, &backup)
                    .with_context(|| format!("backing up to {}", backup.display()))?;
                unix_mode(&meta)
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };

        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "config.toml".to_string());
        let tmp = dir.join(format!(
            ".{name}.tmp.{}.{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let written = (|| -> anyhow::Result<()> {
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            let mode = mode.unwrap_or(0o644);
            #[cfg(unix)]
            std::os::unix::fs::OpenOptionsExt::mode(&mut opts, mode);
            let mut file = opts
                .open(&tmp)
                .with_context(|| format!("creating {}", tmp.display()))?;
            // The umask narrowed the mode `open` was given; state it exactly.
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(mode))
                    .with_context(|| format!("setting the mode of {}", tmp.display()))?;
            }
            #[cfg(not(unix))]
            let _ = mode;
            file.write_all(text.as_bytes())
                .with_context(|| format!("writing {}", tmp.display()))?;
            file.sync_all()
                .with_context(|| format!("syncing {}", tmp.display()))?;
            std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
            Ok(())
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&tmp);
            return written;
        }
        // The rename is durable once the directory is: best effort, since a
        // directory that cannot be opened for sync was still written.
        if let Ok(d) = std::fs::File::open(&dir) {
            let _ = d.sync_all();
        }
        Ok(())
    }

    /// Events in `[gate] allowed_events` that are outside [`crate::gate::MAX_EVENTS`].
    ///
    /// Its own method so [`Self::validate`] and the startup log read the same
    /// answer rather than each deciding what "unknown" means.
    fn allowed_events_unknown(&self) -> Vec<String> {
        self.gate
            .allowed_events
            .iter()
            .filter(|e| !crate::gate::MAX_EVENTS.contains(&e.as_str()))
            .cloned()
            .collect()
    }

    /// Build the runtime routing profiles. `runner_version` is the
    /// `actions/runner` version a macOS VM job runs when it pins none (resolved
    /// once at startup). The actor allow/deny lists are shared across all
    /// profiles. A native profile is an error (ADR-0052), never built.
    pub fn into_profiles(&self, runner_version: &str) -> anyhow::Result<Vec<Profile>> {
        if self.profiles.is_empty() {
            anyhow::bail!("config has no [[profile]] entries");
        }
        // Warned at every boot, never fatal: `validate` refuses such a name on a
        // save and in `check-config`, where a human is looking.
        for p in &self.profiles {
            if !p.name.trim().is_empty() && !profile_name_ok(&p.name) {
                tracing::warn!(
                    "{}; rename it before the next settings save",
                    profile_name_error(&p.name)
                );
            }
        }
        // Warned, not fatal. `faber check-config` and the settings page both
        // *reject* these, so a human editing the file is told at once; a daemon
        // that arrived by unattended update (ADR-0006) must not refuse to boot
        // over a file it did not choose — a dead daemon is worse than a narrowed
        // one, and the rollback would flap against it.
        // Resolved once for the whole host: every profile's job networks come out
        // of the same address space. Warned rather than fatal for the reason just
        // above — `validate` rejects a bad block where a human is looking, and a
        // follower host that inherited one must fall back to Docker's own pools
        // (the behaviour before this key existed) rather than refuse to boot.
        let network_pool = match self.host.network_pool.trim() {
            "" => None,
            decl => match crate::netpool::NetPool::declare(decl, self.host.network_subnet_size) {
                Ok(p) => {
                    tracing::info!(
                        "host: job networks come from {p} ({} concurrent slots)",
                        p.slots()
                    );
                    Some(p)
                }
                Err(e) => {
                    tracing::warn!(
                        "host.network_pool: {e} — falling back to Docker's default address \
                         pools. Run `faber check-config` to fix the file."
                    );
                    None
                }
            },
        };
        if self.update.maturity_secs < MIN_MATURITY_SECS {
            tracing::warn!(
                "update.maturity_secs = {} is under the {MIN_MATURITY_SECS} s floor — using the {} s \
                 default instead. Run `faber check-config` to fix the file.",
                self.update.maturity_secs,
                self.update.effective_maturity_secs()
            );
        }
        let unknown = self.allowed_events_unknown();
        if !unknown.is_empty() {
            tracing::warn!(
                "gate.allowed_events lists {} which faber never provisions for — ignoring \
                 them. Valid events: {}. Run `faber check-config` to fix the file.",
                unknown.join(", "),
                crate::gate::MAX_EVENTS.join(", ")
            );
        }
        if self
            .gate
            .allowed_events
            .iter()
            .all(|e| !crate::gate::MAX_EVENTS.contains(&e.as_str()))
        {
            tracing::warn!(
                "gate.allowed_events admits no event faber provisions for — every job will be \
                 refused. This is a valid (maximally restrictive) setting; if it wasn't meant, \
                 remove the key to restore faber's default boundary."
            );
        }
        if self.gate.allow_public_repos {
            // A relaxation of the trust boundary, on the record at every boot: the
            // point of moving the boundary into config was that it stops being
            // something you can only learn by reading the source.
            tracing::warn!(
                "gate.allow_public_repos is on — this host services PUBLIC repos. Fork PRs are \
                 still refused and the event ceiling is unchanged, but anyone can queue jobs \
                 against a watched public repo."
            );
        }
        if let Some(warning) = job_token_dispatches_warning(&self.gate) {
            tracing::warn!("{warning}");
        }
        self.profiles
            .iter()
            .map(|p| -> anyhow::Result<Option<Profile>> {
                // The boundary is host-wide, not per-profile: a job that reaches
                // any profile reaches this host, and letting one profile run
                // `pull_request` while another refuses it would mean the answer to
                // "what will this machine run?" depended on label routing.
                let policy = TrustPolicy::new(
                    self.trusted_actors.clone(),
                    self.denied_actors.clone(),
                    p.labels.clone(),
                )
                .with_boundary(
                    self.gate.allowed_events.clone(),
                    self.gate.allow_public_repos,
                )
                .with_on_bypass(self.gate.on_bypass)
                .with_job_token_dispatches(self.gate.allow_job_token_dispatches);
                let backend = match p.backend {
                    BackendKind::Native => {
                        // Fatal, like a docker profile without an image: the
                        // backend is gone, and ADR-0052 chose an error over
                        // ADR-0026's ignored-with-a-warning.
                        anyhow::bail!(native_profile_error(&format!("profile '{}'", p.name)));
                    }
                    BackendKind::MacosVm => match MacosVmConfig::from_profile(p, runner_version) {
                        Ok(mut cfg) => {
                            // Top-level, like the docker backend's: one lifetime cap
                            // for every runner this host starts.
                            if let Some(secs) = self.runner_timeout_secs {
                                cfg.timeouts.job = Duration::from_secs(secs);
                            }
                            cfg.revive =
                                crate::vm::revive::RevivePolicy::from_switch(self.host.revive_vms);
                            Backend::MacosVm(cfg)
                        }
                        Err(e) => {
                            // Warned, not fatal (AGENTS.md): `check-config` and the
                            // settings save reject this where a human is looking.
                            // The profile then has no runner on this host.
                            tracing::warn!(
                                "profile '{}' is not a usable macos-vm profile ({e}) — \
                                 ignoring it. Run `faber check-config` to fix the file.",
                                p.name
                            );
                            return Ok(None);
                        }
                    },
                    BackendKind::Docker => {
                        let image = p.image.clone().with_context(|| {
                            format!("docker profile '{}' requires an `image`", p.name)
                        })?;
                        let mut cfg = DockerConfig::new(image);
                        // Which jobs a grant reaches depends on `[host]
                        // dind_requires_label` (ADR-0026): with the rule on, only
                        // those whose runs-on includes `dind`; off or unstated,
                        // every job on the profile, as before the rule existed.
                        let label_rule = self.host.dind_label_rule().requires_label();
                        let which_jobs = if label_rule {
                            "jobs whose runs-on includes `dind` get one, its other jobs none \
                             (ADR-0026)"
                        } else {
                            "every job on it gets one ([host] dind_requires_label is not \
                             true, ADR-0026)"
                        };
                        cfg.dind = match p.dind {
                            Some(true) => {
                                // Once per boot: which jobs the grant reaches is a
                                // host setting (ADR-0026), which an operator who
                                // wrote `true` may not expect.
                                tracing::info!(
                                    "profile '{}' grants a Docker daemon: {which_jobs}",
                                    p.name
                                );
                                DindGrant::Granted
                            }
                            Some(false) => DindGrant::Denied,
                            None => {
                                // Loud, and on every boot rather than once: this is
                                // the widest privilege faber hands out and nobody
                                // has said whether this profile should have it. The
                                // warning is the whole reason `Unstated` is a state
                                // rather than a silent `true` (issue #43).
                                tracing::warn!(
                                    "profile '{}' does not say whether its jobs get a Docker \
                                     daemon, so {which_jobs} — a --privileged container and an \
                                     unauthenticated daemon on the job network, which is host \
                                     root. Set `dind = false` on the profile if its jobs never \
                                     run `docker` (most don't), or `dind = true` to state the \
                                     grant.",
                                    p.name
                                );
                                DindGrant::Unstated
                            }
                        };
                        // ADR-0025 §"On Linux, dind waits for a wall": a granted
                        // (or unstated) dind on Linux is root on the whole machine
                        // with nothing protecting faber's own files from it, per
                        // today's `WallFacts::current()`. Loud, on every boot,
                        // same shape as the Unstated warning just above.
                        if dind_needs_wall(WallFacts::current(), cfg.dind) {
                            tracing::warn!(
                                "profile '{}': Docker access on Linux has no account wall \
                                 around it yet — a granted (or unstated) dind is root on this \
                                 whole machine, not merely inside a VM as on macOS. Set \
                                 `dind = false` if this profile's jobs never run `docker`, or \
                                 run this host's jobs behind a wall once ADR-0026 ships. This is \
                                 only a warning today; a later, release-noted version will \
                                 refuse it outright.",
                                p.name
                            );
                        }
                        if let Some(d) = &p.dind_image {
                            cfg.dind_image = d.clone();
                        }
                        cfg.env = p.env.clone();
                        cfg.cache_volume = p.cache_volume;
                        cfg.git_mirror = p.git_mirror;
                        cfg.mirror = p.mirror;
                        if let Some(secs) = self.runner_timeout_secs {
                            cfg.runner_timeout = Duration::from_secs(secs);
                        }
                        // `[registry_auth]` is held and deliberately not carried:
                        // nothing a job reaches holds it (see [`RegistryAuth`]).
                        //
                        // Host-level, not per-profile: the address space belongs
                        // to the machine, not to a routing profile.
                        cfg.network_pool = network_pool;
                        Backend::Docker(cfg)
                    }
                };
                Ok(Some(Profile {
                    name: p.name.clone(),
                    labels: p.labels.clone(),
                    backend,
                    policy,
                }))
            })
            .filter_map(Result::transpose)
            .collect()
    }

    /// Whether `tool` may be provisioned at `version` — the operator's ceiling
    /// from `[needs.<tool>]` (ADR-0010 §4).
    ///
    /// ADR-0010 §3 already stops a PR from *asking* (via its
    /// `faber.<tool>-<version>` label) for a version its own lockfile doesn't
    /// pin — but a PR can legitimately *edit* that lockfile, and §3 alone would
    /// then let a lockfile bump alone cause faber to build whatever version
    /// landed in it, unbounded, on the operator's own host. This is the ceiling
    /// above that: `[needs]` lives only in faber's own config, which no watched
    /// repo can reach, so a PR's lockfile bump can widen what *that job* asks
    /// for but never what the *host* is willing to build — bounding disk, build
    /// time, and blast radius to versions the operator chose in the one file
    /// only the operator writes.
    ///
    /// Fails closed in every direction: a tool with no `[needs.<tool>]` entry at
    /// all is [`NeedsVerdict::UnknownTool`], and a `version` that fails to
    /// parse, or fails the configured constraint, or is checked against a
    /// malformed `versions` string (mixed forms — [`FileConfig::validate`]
    /// catches this before a config reaches disk, but a hand-edited file can
    /// still carry it) is [`NeedsVerdict::OutOfRange`]. Nothing here is ever
    /// silently allowed.
    pub fn needs_allow(&self, tool: &str, version: &str) -> NeedsVerdict {
        needs_allow_in(&self.needs, tool, version)
    }
}

/// [`FileConfig::needs_allow`] over a bare allowlist map, for a holder that has
/// the map but no `FileConfig` — [`crate::image::VariantCache`], which applies
/// the ceiling at the one place a build can start.
pub fn needs_allow_in(
    needs: &HashMap<String, ToolNeeds>,
    tool: &str,
    version: &str,
) -> NeedsVerdict {
    let Some(entry) = needs.get(tool) else {
        return NeedsVerdict::UnknownTool;
    };
    let allowed = match parse_needs_version(version) {
        Some(requested) => needs_versions_allow(&entry.versions, requested),
        None => false,
    };
    if allowed {
        NeedsVerdict::Allowed
    } else {
        NeedsVerdict::OutOfRange(entry.versions.clone())
    }
}

/// Machine-local runtime overrides for the live-tunable autolimiter settings, as
/// edited through the control panel's quick tuning controls, plus operator-pinned
/// per-class caps.
///
/// Kept in a separate JSON file, never in `config.toml`, because they are a
/// different *kind* of setting: `config.toml` is the deployment's intent (and may
/// be shared or checked in), while these are one machine's live tweak, applied
/// without a restart. The settings page, which edits the intent, deliberately
/// clears the tweak when it saves — otherwise the file and the running daemon
/// could disagree forever with nothing on screen admitting it.
///
/// Overrides always live at the default app-support path, independent of
/// `--config`, since they are machine-local tuning.
///
/// (Learned — as opposed to pinned — caps are *not* stored here; they are rebuilt
/// on startup from the audit log, the authoritative substrate.)
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Overrides {
    /// The `native_concurrency` knob. Kept and read back, but it limits nothing
    /// since ADR-0052 removed the native jobs it counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_concurrency: Option<usize>,
    /// Held-back VM memory (`budget = MemTotal − reserve`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reserve_mib: Option<u64>,
    /// Gate 2 slack.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub headroom_mib: Option<u64>,
    /// Learned-cap safety margin over observed peak.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub margin: Option<f64>,
    /// Legacy numeric mirror of [`Overrides::ceiling`]: the *effective* limit
    /// (`auto` writes its resolved number, `off` writes nothing) — or `0` while
    /// admission is held, so a rollback to a binary from before holds keeps it
    /// held. Kept because an older binary reads only this key, and a file it could
    /// not parse would be discarded whole — pins included — on a rollback.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hard_ceiling: Option<usize>,
    /// The ceiling *setting* (`"auto"`, `"off"` or a number). Wins over
    /// `hard_ceiling` when present; see [`Overrides::hard_ceiling_setting`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ceiling: Option<HardCeiling>,
    /// Operator-pinned per-class caps the learner won't touch, keyed by class-key.
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub pinned_caps: std::collections::HashMap<String, Limits>,
    /// The operator's hold on new Docker admissions (issue #66), kept across
    /// restarts until it is released. An older binary ignores the key, which is
    /// why the legacy `hard_ceiling` mirror carries a zero alongside it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hold: Option<Hold>,
    /// Per-repo holds on minting (ADR-0030), kept across restarts until the
    /// operator clears them: a restart must not lift a stop that a trust bypass
    /// put there. An older binary ignores the key.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub repo_holds: Vec<RepoHold>,
}

/// A named stop on minting for one repo (ADR-0030), in ADR-0022's shape: what it
/// is about, why, who put it there and since when.
///
/// Set by faber itself when a trust-class gate bypass is enforced, because a
/// workflow that routes untrusted work to faber's labels does not fix itself.
/// Only the operator clears it. Claims for the repo are still taken, so the held
/// work stays visible; each parks and says it is held.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoHold {
    /// `owner/repo`.
    pub repo: String,
    pub reason: String,
    /// Who set it — faber's gate enforcement, today.
    pub by: String,
    pub since_ms: u64,
}

impl RepoHold {
    pub fn since_utc(&self) -> String {
        rfc3339_utc(self.since_ms)
    }
}

/// The reason a hold migrated from a zero ceiling is given — see
/// [`Overrides::migrate_zero_ceiling`].
pub const ZERO_CEILING_HOLD_REASON: &str =
    "hard_ceiling = 0 in overrides.json (set before faber had holds)";

/// An operator's stop on new Docker admissions, with the reason they gave and
/// when they gave it (issue #66).
///
/// A hold is a decision, not a number, and it carries its reason wherever it is
/// shown: the dashboard, the tray, every claim it parks, the boot log. What it
/// replaces is `hard_ceiling = 0`, which said nothing at all — it survived
/// restarts while the dashboard read healthy, every claim sat parked with no
/// reason given, and the stall was diagnosed as something else entirely.
///
/// It stops *new* Docker admissions only. Running and adopted containers finish,
/// and claims are still taken — they park and say
/// they are held. Released only by the operator: `POST /api/unhold`, or Release
/// in the dashboard or the tray.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Hold {
    /// Why admission is held, in the operator's words.
    pub reason: String,
    /// When admission was first held (epoch ms). Re-holding with a new reason
    /// keeps it, so "held since" goes on meaning when admission stopped.
    pub since_ms: u64,
}

impl Hold {
    /// `since_ms` as RFC 3339 UTC to the second (`2026-01-15T12:00:00Z`), for
    /// the log lines that name a hold.
    pub fn since_utc(&self) -> String {
        rfc3339_utc(self.since_ms)
    }
}

/// Epoch milliseconds as RFC 3339 UTC to the second, with no date crate: Howard
/// Hinnant's `civil_from_days`, the inverse of the `days_from_civil` that
/// [`crate::retry::parse_rfc3339_to_ms`] reads GitHub's timestamps with.
pub(crate) fn rfc3339_utc(ms: u64) -> String {
    let secs = ms / 1000;
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // Shifted so the era starts on 0000-03-01; `days` is never negative, so
    // unsigned arithmetic holds throughout.
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11], March-based
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

impl Overrides {
    /// The overridden ceiling setting, if any: `ceiling` when this binary wrote the
    /// file, else the legacy numeric `hard_ceiling` an older one did.
    pub fn hard_ceiling_setting(&self) -> Option<HardCeiling> {
        self.ceiling.or(self.hard_ceiling.map(HardCeiling::Fixed))
    }

    /// Read a zero ceiling as the hold it stood for (issue #66). Run at boot,
    /// before the overrides are resolved, so a zero never reaches the limiter.
    ///
    /// Before holds existed, a zero ceiling was the only way to stop Docker
    /// admission, and the live API took one — so a host can boot with it here.
    /// Taken as a number it stops admission with no reason given and nothing to
    /// mark it as deliberate. So both ceiling keys are cleared, letting the
    /// config file's own ceiling apply, and unless a hold is already recorded the
    /// zero becomes one, dated `written_ms` (the file's mtime: the best record
    /// there is of when it was set).
    ///
    /// What that host relied on is kept: admission stays stopped (AGENTS.md — a
    /// release must not withdraw a capability a host relies on). Nothing is
    /// written back; the next save from the control panel records the hold.
    ///
    /// This binary's own held writes pass through untouched: they keep the true
    /// setting in `ceiling` and the zero only in the legacy mirror. A zero mirror
    /// with no `ceiling` beside it — a settings save while held — is cleared the
    /// same way, which is what that save meant: the file's ceiling applies.
    ///
    /// Returns whether anything changed.
    pub fn migrate_zero_ceiling(&mut self, written_ms: u64) -> bool {
        if self.hard_ceiling_setting() != Some(HardCeiling::Fixed(0)) {
            return false;
        }
        self.ceiling = None;
        self.hard_ceiling = None;
        self.hold.get_or_insert_with(|| Hold {
            reason: ZERO_CEILING_HOLD_REASON.to_string(),
            since_ms: written_ms,
        });
        true
    }

    /// When the overrides file was last written (epoch ms), if that can be read.
    pub fn written_ms() -> Option<u64> {
        let modified = std::fs::metadata(Self::default_path()?)
            .ok()?
            .modified()
            .ok()?;
        let since_epoch = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
        Some(since_epoch.as_millis() as u64)
    }

    /// `~/Library/Application Support/faber/overrides.json`.
    pub fn default_path() -> Option<PathBuf> {
        crate::platform::overrides_path()
    }

    /// Load overrides, or an empty set if the file is missing or unreadable —
    /// runtime tuning is best-effort and must never block daemon startup.
    pub fn load() -> Self {
        let Some(path) = Self::default_path() else {
            return Self::default();
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => Self::default(),
        }
    }

    /// Persist overrides, creating the app-support directory if needed. Atomic,
    /// and serialized with every other writer by [`OVERRIDES_LOCK`].
    pub fn save(&self) -> anyhow::Result<()> {
        let path = Self::default_path().context("no HOME to locate the faber overrides file")?;
        let _guard = overrides_lock();
        self.save_at(&path)
    }

    /// Replace the whole file with what `build` returns, built *under* the lock —
    /// so a writer composing the file from live state cannot interleave with
    /// another's load-modify-save and write back fields that writer just changed.
    pub fn replace(build: impl FnOnce() -> Overrides) -> anyhow::Result<()> {
        let path = Self::default_path().context("no HOME to locate the faber overrides file")?;
        let _guard = overrides_lock();
        build().save_at(&path)
    }

    /// Load, change and save the file as one step under the lock. A file that
    /// exists but does not parse is an error and is left untouched: saving
    /// defaults over it would throw away whatever the operator had in it.
    pub fn update(change: impl FnOnce(&mut Overrides)) -> anyhow::Result<()> {
        let path = Self::default_path().context("no HOME to locate the faber overrides file")?;
        let _guard = overrides_lock();
        Self::update_at(&path, change)
    }

    fn update_at(path: &Path, change: impl FnOnce(&mut Overrides)) -> anyhow::Result<()> {
        let mut o = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str::<Overrides>(&text).with_context(|| {
                format!("{} does not parse; leaving it as it is", path.display())
            })?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Overrides::default(),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        change(&mut o);
        o.save_at(path)
    }

    /// Write to a temporary file beside `path`, then rename it over `path`: a
    /// reader never sees half a file, and a crash mid-write leaves the old one.
    fn save_at(&self, path: &Path) -> anyhow::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
        std::fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("renaming onto {}", path.display()))?;
        Ok(())
    }
}

/// The permission bits `FileConfig::save_to` copies from the file it replaces
/// (`& 0o777`: setuid, setgid and sticky never carry over); `None` off Unix.
#[cfg(unix)]
fn unix_mode(meta: &std::fs::Metadata) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    Some(meta.permissions().mode() & 0o777)
}

#[cfg(not(unix))]
fn unix_mode(_meta: &std::fs::Metadata) -> Option<u32> {
    None
}

/// Whether `name` is a profile name a settings save takes: ASCII letters,
/// digits, `.`, `_` and `-`.
pub fn profile_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
}

fn profile_name_error(name: &str) -> String {
    format!(
        "profile {name:?}: a name may hold only letters, digits, '.', '_' and '-' \
         (it is part of the sudo commands faber shows)"
    )
}

/// Serializes every write of the overrides file in this process. Several writers
/// share it — the panel's tuning, pins and hold, the settings save, and the repo
/// holds a gate bypass sets (ADR-0030) — and a load-modify-save racing a whole-file
/// write would lose one of them.
static OVERRIDES_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn overrides_lock() -> std::sync::MutexGuard<'static, ()> {
    OVERRIDES_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
        topic = "selfhosted-ci"
        concurrency = 4
        poll_interval_secs = 15
        denied_actors = ["dependabot[bot]"]

        [[profile]]
        name = "macos"
        labels = ["self-hosted", "macOS", "ARM64"]
        backend = "macos-vm"
        image = "macos-27"

        [[profile]]
        name = "linux-docker"
        labels = ["self-hosted", "docker"]
        backend = "docker"
        image = "ghcr.io/owner/my-runner:latest"
        env = { PLAYWRIGHT_BROWSERS_PATH = "/opt/ms-playwright" }
    "#;

    #[test]
    fn parses_and_builds_profiles() {
        let cfg: FileConfig = toml::from_str(SAMPLE).unwrap();
        assert_eq!(cfg.topic, "selfhosted-ci");
        assert_eq!(cfg.concurrency, 4);
        assert_eq!(cfg.poll_interval_secs, 15);

        let profiles = cfg.into_profiles("2.330.0").unwrap();
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].backend.kind(), "macos-vm");
        match &profiles[1].backend {
            Backend::Docker(d) => {
                assert_eq!(d.image, "ghcr.io/owner/my-runner:latest");
                assert_eq!(
                    d.env.get("PLAYWRIGHT_BROWSERS_PATH").map(String::as_str),
                    Some("/opt/ms-playwright")
                );
                assert_eq!(d.dind_image, "docker:dind");
                // Absent from SAMPLE — the cache must stay opt-in, so a config
                // that says nothing about it gets no cross-job volume.
                assert!(!d.cache_volume);
            }
            _ => panic!("expected docker backend"),
        }
    }

    #[test]
    fn cache_volume_opt_in_reaches_the_docker_config() {
        let cfg: FileConfig = toml::from_str(
            r#"
            [[profile]]
            name = "linux-docker"
            labels = ["faber", "docker"]
            backend = "docker"
            image = "ghcr.io/owner/my-runner:latest"
            cache_volume = true
        "#,
        )
        .unwrap();
        match &cfg.into_profiles("2.330.0").unwrap()[0].backend {
            Backend::Docker(d) => assert!(d.cache_volume),
            _ => panic!("expected docker backend"),
        }
    }

    /// One docker profile with `extra` appended, for the `git_mirror` cases.
    fn docker_profile_with(extra: &str) -> FileConfig {
        toml::from_str(&format!(
            r#"
            [[profile]]
            name = "linux-docker"
            labels = ["faber", "docker"]
            backend = "docker"
            image = "ghcr.io/owner/my-runner:latest"
            dind = false
            {extra}
        "#
        ))
        .unwrap()
    }

    #[test]
    fn git_mirror_is_off_unless_set_true_and_false_is_never_written() {
        // ADR-0035 §8: absent and `false` are the same, and both are today's
        // behaviour.
        for (extra, want) in [
            ("", false),
            ("git_mirror = false", false),
            ("git_mirror = true", true),
        ] {
            let cfg = docker_profile_with(extra);
            assert_eq!(cfg.profiles[0].git_mirror, want, "{extra:?}");
            match &cfg.clone().into_profiles("2.330.0").unwrap()[0].backend {
                Backend::Docker(d) => assert_eq!(d.git_mirror, want, "{extra:?}"),
                _ => panic!("expected docker backend"),
            }
            assert!(cfg.validate().is_empty(), "{extra:?}: {:?}", cfg.validate());
            // A config that never set it saves without it: byte-identical.
            let saved = toml::to_string(&cfg).unwrap();
            assert_eq!(saved.contains("git_mirror"), want, "{extra:?}: {saved}");
        }
    }

    #[test]
    fn the_profile_mirror_key_is_off_unless_set_true_and_false_is_never_written() {
        for (extra, want) in [
            ("", false),
            ("mirror = false", false),
            ("mirror = true", true),
        ] {
            let cfg = docker_profile_with(extra);
            assert_eq!(cfg.profiles[0].mirror, want, "{extra:?}");
            match &cfg.clone().into_profiles("2.330.0").unwrap()[0].backend {
                Backend::Docker(d) => assert_eq!(d.mirror, want, "{extra:?}"),
                _ => panic!("expected docker backend"),
            }
            assert!(cfg.validate().is_empty(), "{extra:?}: {:?}", cfg.validate());
            let saved = toml::to_string(&cfg).unwrap();
            let profile = &saved[saved.find("[[profile]]").unwrap()..];
            assert_eq!(
                profile.lines().any(|l| l.starts_with("mirror")),
                want,
                "{extra:?}: {saved}"
            );
        }
    }

    #[test]
    fn the_waste_section_is_off_unless_set_and_refuses_skipping_a_failed_run() {
        // ADR-0063 §3: absent is off, and an all-off section is not written back.
        let cfg: FileConfig = toml::from_str("").unwrap();
        assert!(cfg.waste.is_off());
        assert!(!toml::to_string(&cfg).unwrap().contains("[waste]"));

        let mut cfg = docker_profile_with("");
        cfg.waste = toml::from_str("conflicted = \"demote\"\nclosed = \"skip\"\n").unwrap();
        assert_eq!(cfg.waste.conflicted, WasteMode::Demote);
        assert_eq!(cfg.waste.closed, WasteMode::Skip);
        assert_eq!(cfg.waste.superseded, WasteMode::Off);
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
        let saved = toml::to_string(&cfg).unwrap();
        assert!(saved.contains("conflicted = \"demote\""), "{saved}");
        assert!(!saved.contains("superseded"), "{saved}");

        assert!(toml::from_str::<FileConfig>("[waste]\nconflicted = \"maybe\"\n").is_err());
        assert!(toml::from_str::<FileConfig>("[waste]\ndraft = \"demote\"\n").is_err());

        cfg.waste = toml::from_str("run_failed = \"skip\"\n").unwrap();
        let errs = cfg.validate();
        assert!(
            errs.len() == 1 && errs[0].contains("waste.run_failed"),
            "{errs:?}"
        );
        // Policy, not structure: an update is never held back over it.
        assert!(cfg.validate_structural().is_empty());
    }

    #[test]
    fn an_absent_mirror_section_is_off_and_is_not_written_back() {
        let cfg = docker_profile_with("mirror = true");
        assert!(cfg.mirror.is_none());
        assert!(cfg.mirror_policy().is_none());
        assert!(!toml::to_string(&cfg).unwrap().contains("[mirror]"));
        // Opted in with nothing to point at: a boot warning, not an error, so the
        // section can be removed alone to turn the mirror off.
        assert!(cfg.validate().is_empty());
        let w = mirror_warnings(&cfg);
        assert!(
            w.len() == 1 && w[0].contains("without a [mirror] section"),
            "{w:?}"
        );
    }

    #[test]
    fn the_mirror_section_defaults_its_size_and_refuses_zero() {
        let mut cfg: FileConfig = toml::from_str(
            r#"
            [mirror]

            [[profile]]
            name = "linux-docker"
            labels = ["faber", "docker"]
            backend = "docker"
            image = "ghcr.io/owner/my-runner:latest"
            dind = false
            mirror = true
        "#,
        )
        .unwrap();
        assert_eq!(
            cfg.mirror,
            Some(MirrorFile {
                max_size_gib: DEFAULT_MIRROR_MAX_SIZE_GIB,
                ..MirrorFile::default()
            })
        );
        assert_eq!(
            cfg.mirror_policy(),
            Some(crate::mirror::MirrorPolicy {
                max_size_gib: 20,
                lfs: None,
                npm: None,
            })
        );
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
        assert!(mirror_warnings(&cfg).is_empty());
        let saved = toml::to_string(&cfg).unwrap();
        assert!(
            saved.contains("[mirror]") && saved.contains("max_size_gib = 20"),
            "{saved}"
        );
        assert!(
            saved.find("[mirror]") < saved.find("[[profile]]"),
            "a table after the profiles would land inside the last one: {saved}"
        );

        cfg.mirror = Some(MirrorFile {
            max_size_gib: 0,
            ..MirrorFile::default()
        });
        assert!(cfg
            .validate_structural()
            .iter()
            .any(|e| e.contains("mirror.max_size_gib must be above 0")));
        // At boot it is a warning and the mirror stays off, never a refusal.
        assert!(cfg.mirror_policy().is_none());
        assert!(mirror_warnings(&cfg)
            .iter()
            .any(|w| w.starts_with("[mirror] is off")));
        assert!(cfg.clone().into_profiles("2.330.0").is_ok());

        let unknown: Result<FileConfig, _> = toml::from_str("[mirror]\nmax_size = 5\n");
        assert!(unknown.is_err(), "an unknown [mirror] key is a parse error");
    }

    #[test]
    fn mirror_lfs_takes_relaxed_or_off_and_refuses_what_this_release_lacks() {
        let load = |extra: &str| FileConfig::load_from_str(&format!("[mirror]\n{extra}"), "test");
        let cfg = load("[mirror.lfs]\n").unwrap();
        assert_eq!(
            cfg.mirror_policy().unwrap().lfs,
            Some(faber_mirror::Fill::Relaxed),
            "fill defaults to relaxed"
        );
        let cfg = load("[mirror.lfs]\nfill = \"off\"\n").unwrap();
        assert_eq!(
            cfg.mirror_policy().unwrap().lfs,
            Some(faber_mirror::Fill::Off)
        );
        let saved = toml::to_string(&cfg).unwrap();
        assert!(
            saved.contains("[mirror.lfs]") && saved.contains("fill = \"off\""),
            "{saved}"
        );
        assert_eq!(load("").unwrap().mirror_policy().unwrap().lfs, None);

        for (extra, says) in [
            ("[mirror.lfs]\nfill = \"strict\"\n", "reserved"),
            (
                "[mirror.lfs]\nwarm_on_push = true\n",
                "warm_on_push is not in this release",
            ),
            (
                "[mirror.lfs]\nwarm_on_push = false\n",
                "warm_on_push is not in this release",
            ),
            ("[mirror.lfs]\nfill = \"eager\"\n", "\"relaxed\" or \"off\""),
            ("[mirror.lfs]\nupload = true\n", "unknown field"),
        ] {
            let err = format!("{:#}", load(extra).unwrap_err());
            assert!(err.contains(says), "{extra:?}: {err}");
        }
    }

    #[test]
    fn mirror_npm_takes_relaxed_or_off_and_refuses_what_this_release_lacks() {
        let load = |extra: &str| FileConfig::load_from_str(&format!("[mirror]\n{extra}"), "test");
        let cfg = load("[mirror.npm]\n").unwrap();
        let policy = cfg.mirror_policy().unwrap();
        assert_eq!(
            policy.npm,
            Some(faber_mirror::Fill::Relaxed),
            "fill defaults to relaxed"
        );
        assert_eq!(policy.lfs, None, "npm alone leaves LFS off");
        let cfg = load("[mirror.npm]\nfill = \"off\"\n[mirror.lfs]\n").unwrap();
        let policy = cfg.mirror_policy().unwrap();
        assert_eq!(policy.npm, Some(faber_mirror::Fill::Off));
        assert_eq!(policy.lfs, Some(faber_mirror::Fill::Relaxed));
        let saved = toml::to_string(&cfg).unwrap();
        assert!(
            saved.contains("[mirror.npm]") && saved.contains("fill = \"off\""),
            "{saved}"
        );
        assert_eq!(load("").unwrap().mirror_policy().unwrap().npm, None);

        for (extra, says) in [
            ("[mirror.npm]\nfill = \"strict\"\n", "reserved"),
            ("[mirror.npm]\nfill = \"eager\"\n", "\"relaxed\" or \"off\""),
            ("[mirror.npm]\nupload = true\n", "unknown field"),
            (
                "[mirror.npm]\nregistry = \"https://registry.example\"\n",
                "unknown field",
            ),
            ("[mirror.npm]\nwarm_on_push = true\n", "unknown field"),
        ] {
            let err = format!("{:#}", load(extra).unwrap_err());
            assert!(err.contains(says), "{extra:?}: {err}");
        }
    }

    #[test]
    fn a_settings_save_keeps_mirror_npm_as_it_is_and_never_makes_one() {
        // The settings form starts from the whole config as JSON and posts it
        // back; `[mirror]` has no field of its own (`HAND_EDITED_SECTIONS`).
        let round = |cfg: &FileConfig| -> FileConfig {
            serde_json::from_value(serde_json::to_value(cfg).unwrap()).unwrap()
        };
        let cfg =
            FileConfig::load_from_str("[mirror]\n[mirror.npm]\nfill = \"off\"\n", "test").unwrap();
        let saved = round(&cfg);
        assert_eq!(
            saved.mirror.as_ref().unwrap().npm,
            Some(MirrorNpmFile {
                fill: faber_mirror::Fill::Off
            })
        );
        assert!(saved
            .to_toml()
            .unwrap()
            .contains("[mirror.npm]\nfill = \"off\""));
        let none = round(&FileConfig::load_from_str("[mirror]\n", "test").unwrap());
        assert_eq!(none.mirror.as_ref().unwrap().npm, None);
        assert!(!none.to_toml().unwrap().contains("mirror.npm"));
        // A JSON body naming a reserved fill is refused like the file.
        let mut v = serde_json::to_value(&cfg).unwrap();
        v["mirror"]["npm"]["fill"] = "strict".into();
        assert!(serde_json::from_value::<FileConfig>(v).is_err());
    }

    #[test]
    fn mirror_npm_without_a_mirror_section_is_refused_and_keeps_the_mirror_off() {
        let cfg = FileConfig::load_from_str("[mirror.npm]\nfill = \"relaxed\"\n", "test").unwrap();
        assert!(cfg.mirror.as_ref().unwrap().implicit);
        assert!(cfg
            .validate_structural()
            .iter()
            .any(|e| e.contains("need a [mirror] section")));
        // At boot: a warning and the mirror off, never a refusal to start.
        assert!(cfg.mirror_policy().is_none());
        assert!(mirror_warnings(&cfg)
            .iter()
            .any(|w| w.starts_with("[mirror] is off")));
        let cfg = FileConfig::load_from_str("[mirror.npm]\n[mirror]\n", "test").unwrap();
        assert_eq!(
            cfg.mirror_policy().unwrap().npm,
            Some(faber_mirror::Fill::Relaxed),
            "order does not matter"
        );
    }

    #[test]
    fn mirror_lfs_without_a_mirror_section_is_refused_and_keeps_the_mirror_off() {
        let text = r#"
            [mirror.lfs]
            fill = "relaxed"
        "#;
        let cfg = FileConfig::load_from_str(text, "test").unwrap();
        assert!(cfg.mirror.as_ref().unwrap().implicit);
        assert!(cfg
            .validate_structural()
            .iter()
            .any(|e| e.contains("[mirror.lfs] and [mirror.npm] need a [mirror] section")));
        // At boot: a warning and the mirror off, never a refusal to start.
        assert!(cfg.mirror_policy().is_none());
        assert!(mirror_warnings(&cfg)
            .iter()
            .any(|w| w.starts_with("[mirror] is off")));

        // Written out, the same table is fine.
        let cfg = FileConfig::load_from_str("[mirror]\n[mirror.lfs]\n", "test").unwrap();
        assert!(!cfg.mirror.as_ref().unwrap().implicit);
        assert!(cfg.mirror_policy().is_some());
        let cfg = FileConfig::load_from_str("[mirror.lfs]\n[mirror]\nmax_size_gib = 5\n", "test")
            .unwrap();
        assert!(cfg.mirror_policy().is_some(), "order does not matter");
    }

    #[test]
    fn mirror_on_a_macos_vm_profile_is_refused_where_a_human_looks() {
        let cfg: FileConfig = toml::from_str(
            r#"
            [mirror]
            [[profile]]
            name = "mac"
            labels = ["faber", "macOS"]
            backend = "macos-vm"
            image = "macos-27"
            mirror = true
        "#,
        )
        .unwrap();
        assert!(cfg
            .validate()
            .iter()
            .any(|e| e.contains("mirror needs a docker backend")));
        assert!(mirror_warnings(&cfg)
            .iter()
            .any(|w| w.contains("only a docker profile")));
        // ...and only warned about at boot, never fatal.
        assert!(cfg.into_profiles("2.330.0").is_ok());
        let vm = vm_profile("mirror = true");
        assert!(vm
            .validate_structural()
            .iter()
            .any(|e| e.contains("mirror needs a docker backend")));
    }

    #[test]
    fn drop_grants_is_absent_unless_set_and_a_malformed_one_is_refused() {
        // Absent narrows nothing, and is not written back.
        let cfg = docker_profile_with("");
        assert!(cfg.profiles[0].drop_grants.is_empty());
        assert!(!toml::to_string(&cfg).unwrap().contains("drop_grants"));

        let cfg = docker_profile_with(
            r#"drop_grants = ["192.0.2.5/32 5432/tcp", "192.0.2.0/24 22/udp"]"#,
        );
        assert_eq!(cfg.profiles[0].drop_grants.len(), 2);
        assert!(
            cfg.validate_structural().is_empty(),
            "{:?}",
            cfg.validate_structural()
        );
        let saved = toml::to_string(&cfg).unwrap();
        assert!(saved.contains("drop_grants"), "{saved}");
        let back: FileConfig = toml::from_str(&saved).unwrap();
        assert_eq!(back.profiles[0].drop_grants, cfg.profiles[0].drop_grants);

        for bad in [
            "db.example.com 5432/tcp",
            "192.0.2.5 5432/tcp",
            "192.0.2.5/32",
            "192.0.2.5/32 5432/TCP",
            "192.0.2.5/32 0/tcp",
        ] {
            let cfg = docker_profile_with(&format!("drop_grants = [{bad:?}]"));
            let errs = cfg.validate_structural();
            assert!(
                errs.iter()
                    .any(|e| e.contains("drop_grants") && e.contains("linux-docker")),
                "{bad}: {errs:?}"
            );
            assert!(!cfg.validate().is_empty(), "{bad}");
        }
    }

    #[test]
    fn git_mirror_on_a_macos_vm_profile_is_refused_where_a_human_looks() {
        let cfg: FileConfig = toml::from_str(
            r#"
            [[profile]]
            name = "mac"
            labels = ["faber", "macOS"]
            backend = "macos-vm"
            image = "macos-27"
            git_mirror = true
        "#,
        )
        .unwrap();
        assert!(cfg.validate().iter().any(|e| e.contains("git_mirror")));
        // ...and only warned about at boot, never fatal.
        assert!(cfg.into_profiles("2.330.0").is_ok());
    }

    #[test]
    fn a_declared_network_pool_reaches_the_docker_config() {
        let cfg: FileConfig = toml::from_str(
            r#"
            [host]
            network_pool = "10.63.0.0/16"
            network_subnet_size = 24

            [[profile]]
            name = "linux-docker"
            labels = ["faber", "docker"]
            backend = "docker"
            image = "ghcr.io/owner/my-runner:latest"
        "#,
        )
        .unwrap();
        match &cfg.into_profiles("2.330.0").unwrap()[0].backend {
            Backend::Docker(d) => {
                let p = d.network_pool.expect("pool reaches the backend");
                assert_eq!(p.slots(), 256);
            }
            _ => panic!("expected docker backend"),
        }
    }

    /// The contract that lets this ship to unattended follower hosts (ADR-0006):
    /// a config that never mentions the key keeps Docker's own allocation.
    #[test]
    fn no_network_pool_key_leaves_allocation_to_docker() {
        let cfg: FileConfig = toml::from_str(
            r#"
            [[profile]]
            name = "linux-docker"
            labels = ["faber", "docker"]
            backend = "docker"
            image = "ghcr.io/owner/my-runner:latest"
        "#,
        )
        .unwrap();
        match &cfg.into_profiles("2.330.0").unwrap()[0].backend {
            Backend::Docker(d) => assert!(d.network_pool.is_none()),
            _ => panic!("expected docker backend"),
        }
    }

    /// Rejected where a human is looking. A block that cannot be parsed would
    /// otherwise surface as a provisioning failure on every single job, which is
    /// the mystifying-error class this key exists to end.
    #[test]
    fn validate_rejects_an_unusable_network_pool() {
        for (pool, size) in [
            ("not-a-cidr", 24),
            ("10.63.0.0/24", 16), // subnet wider than the pool holding it
            ("10.0.0.0/8", 30),   // four million slots
        ] {
            let cfg: FileConfig = toml::from_str(&format!(
                r#"
                [host]
                network_pool = "{pool}"
                network_subnet_size = {size}

                [[profile]]
                name = "linux-docker"
                labels = ["faber", "docker"]
                backend = "docker"
                image = "ghcr.io/owner/my-runner:latest"
            "#
            ))
            .unwrap();
            let errs = cfg.validate();
            assert!(
                errs.iter().any(|e| e.starts_with("host.network_pool:")),
                "{pool} /{size} should be rejected, got {errs:?}"
            );
        }
    }

    /// ...but a daemon that inherited one through an unattended update must still
    /// boot, falling back to what it did before the key existed.
    #[test]
    fn an_unusable_network_pool_degrades_rather_than_bricking_the_daemon() {
        let cfg: FileConfig = toml::from_str(
            r#"
            [host]
            network_pool = "not-a-cidr"

            [[profile]]
            name = "linux-docker"
            labels = ["faber", "docker"]
            backend = "docker"
            image = "ghcr.io/owner/my-runner:latest"
        "#,
        )
        .unwrap();
        let profiles = cfg.into_profiles("2.330.0").expect("still boots");
        match &profiles[0].backend {
            Backend::Docker(d) => assert!(d.network_pool.is_none()),
            _ => panic!("expected docker backend"),
        }
    }

    // --- the disk admission floor (ADR-0024 §1) -----------------------------

    fn with_host(host: &str) -> FileConfig {
        toml::from_str(&format!(
            r#"
            [host]
            {host}

            [[profile]]
            name = "linux-docker"
            labels = ["faber", "docker"]
            backend = "docker"
            image = "ghcr.io/owner/my-runner:latest"
            # Explicit denial, not this section's concern: an Unstated grant
            # would trip the Linux dind-wall policy (ADR-0025 §8) on the
            # `ubuntu-latest` CI leg and nowhere else, which is exactly the
            # "impossible locally" trap AGENTS.md warns about — see
            # `dind_needs_wall`.
            dind = false
        "#
        ))
        .unwrap()
    }

    #[test]
    fn the_sleep_key_resolves_every_combination() {
        use SleepPolicy::*;
        let ok = |n, o| resolve_sleep_policy(n, o);
        assert_eq!(ok(None, None), Ok(Off));
        assert_eq!(ok(None, Some(false)), Ok(Off));
        assert_eq!(ok(None, Some(true)), Ok(WhileBusy));
        for m in [Off, WhileBusy, Always] {
            assert_eq!(ok(Some(m), None), Ok(m));
            assert!(ok(Some(m), Some(true)).is_err());
            assert!(ok(Some(m), Some(false)).is_err());
        }
    }

    #[test]
    fn the_sleep_key_parses_warns_migrates_and_conflicts() {
        assert_eq!(with_host("").host.sleep_policy(), SleepPolicy::Off);
        let new = with_host(r#"prevent_sleep = "always""#);
        assert_eq!(new.host.sleep_policy(), SleepPolicy::Always);
        assert!(new.host.deprecated_sleep_key_warning().is_none());
        assert!(toml::from_str::<FileConfig>("[host]\nprevent_sleep = \"sometimes\"").is_err());

        let mut old = with_host("prevent_sleep_while_busy = true");
        assert_eq!(old.host.sleep_policy(), SleepPolicy::WhileBusy);
        let warning = old.host.deprecated_sleep_key_warning().unwrap();
        assert!(warning.contains("host.prevent_sleep"), "{warning}");
        assert!(old.validate().iter().all(|e| !e.contains("prevent_sleep")));
        // A save writes the new key and never the old one.
        old.host.migrate_sleep_key();
        assert_eq!(old.host.prevent_sleep, Some(SleepPolicy::WhileBusy));
        let text = toml::to_string(&old).unwrap();
        assert!(text.contains("prevent_sleep = \"while_busy\""), "{text}");
        assert!(!text.contains("prevent_sleep_while_busy"), "{text}");

        let both = with_host("prevent_sleep = \"off\"\nprevent_sleep_while_busy = true");
        assert!(both.validate().iter().any(|e| e.contains("both set")));
        // The daemon still boots, on the new key; the form leaves it alone.
        assert_eq!(both.host.sleep_policy(), SleepPolicy::Off);
        let mut migrated = both.clone();
        migrated.host.migrate_sleep_key();
        assert_eq!(migrated.host.prevent_sleep_while_busy, Some(true));
    }

    #[test]
    fn revive_unable_to_start_secs_still_parses_is_ignored_and_never_written_back() {
        let cfg = with_host("revive_unable_to_start_secs = 60");
        assert_eq!(cfg.host.revive_unable_to_start_secs, Some(60));
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
        let warning = cfg.host.ignored_keys_warning().unwrap();
        assert!(warning.contains("is ignored"), "{warning}");
        assert_eq!(
            cfg.host.revive_after_ms(),
            cfg.host.revive_after_secs * 1000
        );
        assert!(!toml::to_string(&cfg)
            .unwrap()
            .contains("revive_unable_to_start_secs"));
        assert_eq!(with_host("").host.ignored_keys_warning(), None);
    }

    #[test]
    fn the_linux_vm_section_takes_defaults_and_is_checked_where_a_human_looks() {
        let cfg = with_host("");
        assert_eq!(cfg.linux_vm, LinuxVmFile::default());
        assert!(cfg.validate().is_empty());
        let sized: FileConfig =
            toml::from_str("[linux_vm]\ncpus = 8\nmemory_mib = 20480\n").unwrap();
        assert_eq!(
            (sized.linux_vm.cpus, sized.linux_vm.memory_mib),
            (Some(8), Some(20480))
        );
        assert!(sized.linux_vm.errors().is_empty());
        assert!(toml::from_str::<FileConfig>("[linux_vm]\nmemory = 1\n").is_err());
        let bad = LinuxVmFile {
            cpus: Some(65),
            memory_mib: Some(512),
            disk_max_gib: Some(63),
            ..Default::default()
        };
        assert_eq!(
            bad.errors(),
            [
                "linux_vm.cpus must be 1 to 64",
                "linux_vm.memory_mib must be 1024 to 262144",
                "linux_vm.disk_max_gib must be 64 to 1024 (the disk's built size turns growth \
                 off)"
            ]
        );
        for ok in [64, 128, 1024] {
            let f = LinuxVmFile {
                disk_max_gib: Some(ok),
                ..Default::default()
            };
            assert!(f.errors().is_empty(), "{ok}");
        }
        let too_big = LinuxVmFile {
            disk_max_gib: Some(1025),
            ..Default::default()
        };
        assert_eq!(too_big.errors().len(), 1);
        let mut c = cfg.clone();
        c.linux_vm = bad;
        assert_eq!(c.validate().len(), 3, "{:?}", c.validate());
        // Absent keys are not written back.
        assert!(!toml::to_string(&cfg).unwrap().contains("memory_mib"));
    }

    #[test]
    fn the_disk_admission_floor_has_three_states_and_absent_is_unstated() {
        // Absent must stay distinguishable from a stated 0 — the whole point of
        // the unstated state is that a host nobody configured says so at boot.
        let unstated = with_host("");
        assert_eq!(unstated.host.disk_admission(), DiskAdmission::Unstated);
        assert_eq!(unstated.host.disk_admission().floor_gib(), None);
        let off = with_host("disk_admission_floor_gib = 0");
        assert_eq!(off.host.disk_admission(), DiskAdmission::Off);
        assert_eq!(off.host.disk_admission().floor_gib(), None);
        let on = with_host("disk_admission_floor_gib = 10");
        assert_eq!(on.host.disk_admission(), DiskAdmission::Floor(10));
        assert_eq!(on.host.disk_admission().floor_gib(), Some(10));
        assert_eq!(
            [unstated, off, on].map(|c| c.host.disk_admission().as_str()),
            ["unstated", "off", "on"]
        );
    }

    #[test]
    fn a_stated_disk_admission_floor_survives_the_round_trip_and_an_unstated_one_stays_absent() {
        // The settings form states the key the first time it saves (it posts 0 for
        // a blank field), so a stated 0 must be written and read back as off —
        // while an absent key must not be written as anything at all.
        let posted: FileConfig = serde_json::from_str(
            r#"{"host": {"disk_admission_floor_gib": 0},
                "profile": [{"name": "n", "labels": ["faber"], "backend": "macos-vm", "image": "macos-27"}]}"#,
        )
        .unwrap();
        let text = posted.to_toml().unwrap();
        assert!(text.contains("disk_admission_floor_gib = 0"), "{text}");
        let reread: FileConfig = toml::from_str(&text).unwrap();
        assert_eq!(reread.host.disk_admission(), DiskAdmission::Off);

        let text = with_host("").to_toml().unwrap();
        assert!(!text.contains("disk_admission_floor_gib"), "{text}");
        let reread: FileConfig = toml::from_str(&text).unwrap();
        assert_eq!(reread.host.disk_admission(), DiskAdmission::Unstated);
    }

    #[test]
    fn the_dind_label_rule_has_three_states_and_absent_stays_absent() {
        // ADR-0026: absent keeps every job on a granting profile its daemon and
        // must stay distinguishable from the `false` the settings form writes on
        // first save.
        let unstated = with_host("");
        assert_eq!(unstated.host.dind_requires_label, None);
        assert_eq!(unstated.host.dind_label_rule(), DindLabelRule::Unstated);
        assert!(unstated.host.dind_label_rule().is_unstated());
        assert!(!unstated.host.dind_label_rule().requires_label());
        let text = unstated.to_toml().unwrap();
        assert!(!text.contains("dind_requires_label"), "{text}");
        for (line, want, rule) in [
            ("dind_requires_label = true", Some(true), DindLabelRule::On),
            (
                "dind_requires_label = false",
                Some(false),
                DindLabelRule::Off,
            ),
        ] {
            let c = with_host(line);
            assert_eq!(c.host.dind_requires_label, want, "{line}");
            assert_eq!(c.host.dind_label_rule(), rule, "{line}");
            assert_eq!(rule.requires_label(), rule == DindLabelRule::On);
            let text = c.to_toml().unwrap();
            assert!(text.contains(line), "{text}");
            let reread: FileConfig = toml::from_str(&text).unwrap();
            assert_eq!(reread.host.dind_label_rule(), rule, "{line}");
            assert!(c.validate().is_empty(), "{:?}", c.validate());
        }
        assert_eq!(
            [
                DindLabelRule::On,
                DindLabelRule::Off,
                DindLabelRule::Unstated
            ]
            .map(|r| r.as_str()),
            ["on", "off", "unstated"]
        );
    }

    #[test]
    fn pull_unqualified_images_has_three_states_and_absent_stays_absent() {
        // #156: absent keeps the implicit Docker Hub pull and must stay
        // distinguishable from the `true` the settings form writes on first save.
        let unstated = with_host("");
        assert_eq!(unstated.host.pull_unqualified_images, None);
        let text = unstated.to_toml().unwrap();
        assert!(!text.contains("pull_unqualified_images"), "{text}");
        for (line, want) in [
            ("pull_unqualified_images = true", Some(true)),
            ("pull_unqualified_images = false", Some(false)),
        ] {
            let c = with_host(line);
            assert_eq!(c.host.pull_unqualified_images, want, "{line}");
            let reread: FileConfig = toml::from_str(&c.to_toml().unwrap()).unwrap();
            assert_eq!(reread.host.pull_unqualified_images, want, "{line}");
            assert!(c.validate().is_empty(), "{:?}", c.validate());
        }
    }

    #[test]
    fn the_machine_share_reads_absent_as_unstated_and_survives_the_round_trip() {
        // ADR-0021's absent-key rule: unstated stays distinguishable from a stated
        // 0, which the settings form writes on its first save.
        let unstated = with_host("");
        assert_eq!(unstated.host.machine_share().as_str(), "unstated");
        assert!(!unstated.host.machine_share().yields_to_pressure());
        let text = unstated.to_toml().unwrap();
        assert!(!text.contains("keep_"), "{text}");

        let stated = with_host("keep_memory_gib = 0\nkeep_cpus = 0");
        let text = stated.to_toml().unwrap();
        assert!(text.contains("keep_memory_gib = 0"), "{text}");
        assert!(text.contains("keep_cpus = 0"), "{text}");
        let reread: FileConfig = toml::from_str(&text).unwrap();
        assert_eq!(reread.host.machine_share().as_str(), "declared");
        assert!(!reread.host.machine_share().yields_to_pressure());

        let partial = with_host("keep_memory_gib = 16");
        assert_eq!(partial.host.machine_share().as_str(), "partial");
        assert!(partial.host.machine_share().yields_to_pressure());
        assert!(partial.validate().is_empty(), "{:?}", partial.validate());
    }

    fn with_update(update: &str) -> FileConfig {
        toml::from_str(&format!(
            r#"
            [update]
            {update}

            [[profile]]
            name = "linux-docker"
            labels = ["faber", "docker"]
            backend = "docker"
            image = "ghcr.io/owner/my-runner:latest"
            dind = false
        "#
        ))
        .unwrap()
    }

    #[test]
    fn maturity_secs_is_24h_when_absent_and_refused_under_an_hour() {
        // ADR-0031: absent means the default wait, never zero.
        let absent = with_update("enabled = true");
        assert_eq!(absent.update.maturity_secs, 86_400);
        assert!(absent.validate().is_empty(), "{:?}", absent.validate());

        let errs = with_update("maturity_secs = 600").validate();
        assert!(
            errs.iter()
                .any(|e| e.starts_with("update.maturity_secs must be at least 3600")),
            "{errs:?}"
        );
        assert!(with_update("maturity_secs = 0").validate().len() == 1);
        assert!(with_update("maturity_secs = 3600").validate().is_empty());
    }

    #[test]
    fn validate_rejects_an_admission_floor_above_the_reclamation_floor() {
        // Admission would stop before reclamation had started, so faber would park
        // CI on a disk it had not yet tried to clear.
        let above = with_host("disk_floor_gib = 25\ndisk_admission_floor_gib = 30");
        let errs = above.validate();
        assert!(
            errs.iter()
                .any(|e| e.starts_with("host.disk_admission_floor_gib (30 GiB) is above")),
            "{errs:?}"
        );
        // …and it is the same sentence boot warns with.
        assert_eq!(
            above.host.disk_floors_out_of_order().as_deref(),
            errs.iter()
                .find(|e| e.starts_with("host.disk_admission_floor_gib"))
                .map(String::as_str)
        );

        for fine in [
            // Equal: admission stops exactly where reclamation starts.
            "disk_floor_gib = 25\ndisk_admission_floor_gib = 25",
            "disk_floor_gib = 25\ndisk_admission_floor_gib = 10",
            // No reclamation at all, so nothing for admission to wait behind.
            "disk_floor_gib = 0\ndisk_admission_floor_gib = 30",
            // Off and unstated have no floor to be out of order.
            "disk_floor_gib = 5\ndisk_admission_floor_gib = 0",
            "disk_floor_gib = 5",
        ] {
            let cfg = with_host(fine);
            assert!(cfg.validate().is_empty(), "{fine:?}: {:?}", cfg.validate());
            assert_eq!(cfg.host.disk_floors_out_of_order(), None, "{fine:?}");
        }
    }

    #[test]
    fn an_out_of_order_disk_floor_still_boots() {
        // Rejected by `validate`, but a file that arrived by unattended update must
        // not stop the daemon: boot warns about it instead (AGENTS.md).
        let cfg = with_host("disk_floor_gib = 25\ndisk_admission_floor_gib = 30");
        assert!(cfg.into_profiles("2.330.0").is_ok());
    }

    #[test]
    fn registry_auth_is_held_and_carries_no_secret() {
        let toml_with = |section: &str| -> FileConfig {
            toml::from_str(&format!(
                r#"
            {section}

            [[profile]]
            name = "linux-docker"
            labels = ["faber", "docker"]
            backend = "docker"
            image = "ghcr.io/owner/my-runner:latest"
            dind = false
        "#
            ))
            .unwrap()
        };
        // Absent and switched off both validate: a held section that is off is
        // just a section, and every existing config file keeps saving.
        for section in [
            "",
            "[registry_auth]\nenabled = false\nusername = \"someone\"",
        ] {
            let cfg = toml_with(section);
            assert!(cfg.validate().is_empty(), "{section}: {:?}", cfg.validate());
            assert_eq!(registry_auth_held_warning(&cfg.registry_auth), None);
        }

        // Switched on, it is refused where a human is looking — but not by the
        // structural gate an unattended update consults (AGENTS.md) — and a
        // daemon that boots with it anyway resolves its profiles, carries none
        // of the section into them, and warns.
        let cfg = toml_with(
            "[registry_auth]\nenabled = true\nusername = \"someone\"\n\
             password_env = \"FABER_REGISTRY_MY_SECRET\"",
        );
        let errs = cfg.validate();
        assert_eq!(
            errs,
            [REGISTRY_AUTH_HELD.to_string()],
            "one sentence, no noise"
        );
        assert!(
            errs[0].contains("can be read by the job's code"),
            "{errs:?}"
        );
        assert!(cfg.validate_structural().is_empty());
        let warning = registry_auth_held_warning(&cfg.registry_auth).expect("warns at boot");
        assert!(warning.contains("held"), "{warning}");
        match &cfg.into_profiles("2.330.0").unwrap()[0].backend {
            Backend::Docker(d) => {
                let shown = format!("{d:?}");
                assert!(!shown.contains("FABER_REGISTRY_MY_SECRET"), "{shown}");
                assert!(!shown.contains("registry"), "{shown}");
            }
            _ => panic!("expected docker backend"),
        }

        // The config file has no way to *hold* a secret, which is what keeps one
        // out of the settings-page rewrite and the dashboard's config view.
        assert!(
            toml::from_str::<FileConfig>("[registry_auth]\npassword = \"hunter2\"\n").is_err(),
            "a `password` key must be rejected, not quietly persisted"
        );
    }

    #[test]
    fn runner_timeout_secs_overrides_the_docker_default() {
        let cfg: FileConfig = toml::from_str(
            r#"
            runner_timeout_secs = 5400
            [[profile]]
            name = "d"
            labels = ["self-hosted", "docker"]
            backend = "docker"
            image = "ghcr.io/owner/my-runner:latest"
            "#,
        )
        .unwrap();
        match &cfg.into_profiles("2.330.0").unwrap()[0].backend {
            Backend::Docker(d) => assert_eq!(d.runner_timeout, Duration::from_secs(5400)),
            _ => panic!("expected docker backend"),
        }
    }

    #[test]
    fn absent_runner_timeout_uses_the_docker_default() {
        // Omitted ⇒ the built-in cap, so the leak backstop is on without opt-in.
        let cfg: FileConfig = toml::from_str(
            r#"
            [[profile]]
            name = "d"
            labels = ["self-hosted", "docker"]
            backend = "docker"
            image = "ghcr.io/owner/my-runner:latest"
            "#,
        )
        .unwrap();
        match &cfg.into_profiles("2.330.0").unwrap()[0].backend {
            Backend::Docker(d) => {
                assert_eq!(d.runner_timeout, crate::docker::DEFAULT_RUNNER_TIMEOUT)
            }
            _ => panic!("expected docker backend"),
        }
    }

    #[test]
    fn defaults_apply_when_omitted() {
        let cfg: FileConfig = toml::from_str(
            r#"
            [[profile]]
            name = "n"
            labels = ["self-hosted", "macOS", "ARM64"]
            backend = "macos-vm"
            image = "macos-27"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.topic, DEFAULT_TOPIC);
        assert_eq!(cfg.concurrency, DEFAULT_CONCURRENCY);
        assert_eq!(cfg.poll_interval_secs, DEFAULT_POLL_SECS);
        assert_eq!(cfg.sweep_interval_secs, DEFAULT_SWEEP_SECS);
        assert_eq!(cfg.denied_actors, vec!["dependabot[bot]".to_string()]);
    }

    #[test]
    fn docker_profile_without_image_is_rejected() {
        let cfg: FileConfig = toml::from_str(
            r#"
            [[profile]]
            name = "bad"
            labels = ["self-hosted", "docker"]
            backend = "docker"
            "#,
        )
        .unwrap();
        assert!(cfg.into_profiles("2.330.0").is_err());
    }

    #[test]
    fn unknown_key_is_rejected() {
        // deny_unknown_fields catches typos in a hand-edited file.
        assert!(toml::from_str::<FileConfig>("concurrencyy = 3").is_err());
    }

    // --- writing the config back (the settings page) -----------------------

    /// A config exercising every shape the writer has to get right: options set,
    /// a table, and an array of tables.
    fn full() -> FileConfig {
        FileConfig {
            topic: "faber".into(),
            runner_timeout_secs: Some(7200),
            provisioning: ProvisioningMode::Demand,
            trusted_actors: vec!["someone".into()],
            playwright_drift: DriftPolicy {
                mode: DriftMode::Gate,
                ..DriftPolicy::default()
            },
            retry: RetryPolicy {
                enabled: true,
                ..RetryPolicy::default()
            },
            autolimit: AutoLimitFile {
                hard_ceiling: HardCeiling::Fixed(4),
                ..AutoLimitFile::default()
            },
            profiles: vec![
                FileProfile {
                    name: "macos".into(),
                    labels: vec!["faber".into(), "macOS".into()],
                    backend: BackendKind::MacosVm,
                    image: Some("macos-27".into()),
                    cpus: None,
                    memory_mib: None,
                    dind: None,
                    dind_image: None,
                    cache_volume: false,
                    git_mirror: false,
                    mirror: false,
                    drop_grants: Vec::new(),
                    env: BTreeMap::new(),
                },
                FileProfile {
                    name: "linux-docker".into(),
                    labels: vec!["faber".into(), "docker".into()],
                    backend: BackendKind::Docker,
                    image: Some("ghcr.io/owner/my-runner:latest".into()),
                    cpus: None,
                    memory_mib: None,
                    // `Some(false)`, not `Some(true)`: still exercises the `Some`
                    // round-trip shape this fixture exists for, but a `Granted`
                    // (or the TOML fixtures' `Unstated`) grant would trip the
                    // Linux dind-wall policy (ADR-0025 §8) on the `ubuntu-latest`
                    // CI leg only — exactly the "impossible locally" trap
                    // AGENTS.md warns about. Tests that care about a specific
                    // grant set `dind` explicitly.
                    dind: Some(false),
                    dind_image: Some("docker:dind".into()),
                    cache_volume: true,
                    git_mirror: true,
                    mirror: false,
                    drop_grants: vec!["192.0.2.5/32 5432/tcp".into()],
                    env: BTreeMap::from([("PLAYWRIGHT_BROWSERS_PATH".into(), "/opt/pw".into())]),
                },
            ],
            ..FileConfig::default()
        }
    }

    #[test]
    fn written_toml_reads_back_identically() {
        // The whole settings feature rests on this: what the UI saves is what the
        // next daemon start parses. TOML is order-sensitive (a value written after
        // a table header lands *inside* that table), so this also pins the struct
        // field order.
        let before = full();
        let text = before.to_toml().unwrap();
        let after: FileConfig = toml::from_str(&text)
            .unwrap_or_else(|e| panic!("re-parsing our own output failed: {e}\n---\n{text}"));
        assert_eq!(
            serde_json::to_value(&before).unwrap(),
            serde_json::to_value(&after).unwrap()
        );
    }

    #[test]
    fn a_float_that_lands_on_a_whole_number_survives_the_round_trip() {
        // The sharp edge under `margin = 1`: TOML has no int→float coercion, so a
        // float written bare as `1` would be a config faber then refuses to parse
        // — a file the settings page wrote and the next start rejects.
        let mut cfg = full();
        cfg.autolimit.margin = 1.0;
        cfg.autolimit.escalation_frac = 1.0;
        let text = cfg.to_toml().unwrap();
        assert!(
            text.contains("margin = 1.0"),
            "float must keep its point:\n{text}"
        );
        assert!(toml::from_str::<FileConfig>(&text).is_ok());
    }

    #[test]
    fn written_toml_carries_the_managed_file_header() {
        assert!(full().to_toml().unwrap().starts_with("# faber config"));
    }

    #[test]
    fn the_superseded_concurrency_key_is_not_written_back() {
        let mut cfg = full();
        cfg.concurrency = 9;
        assert!(!cfg.to_toml().unwrap().contains("concurrency = 9"));
    }

    #[test]
    fn a_valid_config_has_nothing_to_report() {
        assert!(full().validate().is_empty(), "{:?}", full().validate());
    }

    #[test]
    fn validate_rejects_a_password_env_outside_the_faber_namespace() {
        let mut cfg = FileConfig::default();
        let ours = |c: &FileConfig| {
            c.validate()
                .into_iter()
                .filter(|e| e.contains("registry_auth") && e != REGISTRY_AUTH_HELD)
                .collect::<Vec<_>>()
        };
        assert!(ours(&cfg).is_empty(), "{:?}", ours(&cfg));
        for enabled in [false, true] {
            // The updater's gate must never see it.
            cfg.registry_auth.password_env = "GITHUB_TOKEN".into();
            assert!(
                !cfg.validate_structural()
                    .iter()
                    .any(|e| e.contains("registry_auth.password_env")),
                "structural gate must not hold updates back"
            );
            cfg.registry_auth.enabled = enabled;
            cfg.registry_auth.username = "someone".into();
            cfg.registry_auth.password_env = "GITHUB_TOKEN".into();
            let errs = ours(&cfg);
            assert!(
                errs.iter().any(|e| e.contains("FABER_REGISTRY_")),
                "enabled={enabled}: {errs:?}"
            );
            cfg.registry_auth.password_env = "FABER_REGISTRY_OK".into();
            assert!(ours(&cfg).is_empty(), "{:?}", ours(&cfg));
        }
    }

    #[test]
    fn validate_rejects_a_config_that_could_not_start() {
        // Each of these parses fine but breaks the daemon on its next start, which
        // is exactly the failure the settings page must not be able to write.
        type Case = (&'static str, Box<dyn Fn(&mut FileConfig)>);
        let cases: Vec<Case> = vec![
            (
                "empty topic",
                Box::new(|c: &mut FileConfig| c.topic.clear()),
            ),
            (
                "zero poll interval",
                Box::new(|c: &mut FileConfig| c.poll_interval_secs = 0),
            ),
            (
                "no profiles",
                Box::new(|c: &mut FileConfig| c.profiles.clear()),
            ),
            (
                "docker profile without an image",
                Box::new(|c: &mut FileConfig| c.profiles[1].image = None),
            ),
            (
                "blank profile name",
                Box::new(|c: &mut FileConfig| c.profiles[0].name.clear()),
            ),
            (
                "duplicate profile name",
                Box::new(|c: &mut FileConfig| c.profiles[1].name = c.profiles[0].name.clone()),
            ),
            (
                "unreachable duplicate label set",
                Box::new(|c: &mut FileConfig| c.profiles[1].labels = c.profiles[0].labels.clone()),
            ),
            (
                "labelless profile",
                Box::new(|c: &mut FileConfig| c.profiles[0].labels.clear()),
            ),
            (
                "margin below 1",
                Box::new(|c: &mut FileConfig| c.autolimit.margin = 0.9),
            ),
            (
                "zero native concurrency",
                Box::new(|c: &mut FileConfig| c.autolimit.native_concurrency = 0),
            ),
            (
                "zero headroom",
                Box::new(|c: &mut FileConfig| c.autolimit.headroom_mib = 0),
            ),
            (
                "escalation fraction above 1",
                Box::new(|c: &mut FileConfig| c.autolimit.escalation_frac = 1.5),
            ),
            (
                "zero learning window",
                Box::new(|c: &mut FileConfig| c.autolimit.window = 0),
            ),
            (
                "a hard ceiling of zero",
                Box::new(|c: &mut FileConfig| c.autolimit.hard_ceiling = HardCeiling::Fixed(0)),
            ),
            (
                "a zero cpu overcommit",
                Box::new(|c: &mut FileConfig| c.autolimit.cpu_overcommit = 0.0),
            ),
            (
                "a NaN cpu overcommit",
                Box::new(|c: &mut FileConfig| c.autolimit.cpu_overcommit = f64::NAN),
            ),
            (
                "a negative cpu reserve",
                Box::new(|c: &mut FileConfig| c.autolimit.reserve_cpus = -1.0),
            ),
            (
                "a zero profiling cpu share",
                Box::new(|c: &mut FileConfig| c.autolimit.profiling_cpus = 0.0),
            ),
            (
                "an infinite profiling cpu share",
                Box::new(|c: &mut FileConfig| c.autolimit.profiling_cpus = f64::INFINITY),
            ),
            (
                "a zero cpu pause pressure",
                Box::new(|c: &mut FileConfig| c.autolimit.cpu_pause_pressure = 0.0),
            ),
            (
                "a cpu pause pressure above 100%",
                Box::new(|c: &mut FileConfig| c.autolimit.cpu_pause_pressure = 150.0),
            ),
            (
                "a cpu resume pressure equal to the pause pressure",
                Box::new(|c: &mut FileConfig| {
                    c.autolimit.cpu_resume_pressure = c.autolimit.cpu_pause_pressure
                }),
            ),
            (
                "a negative cpu resume pressure",
                Box::new(|c: &mut FileConfig| c.autolimit.cpu_resume_pressure = -1.0),
            ),
            (
                "retry enabled with no lookback",
                Box::new(|c: &mut FileConfig| c.retry.lookback_secs = 0),
            ),
            (
                "needs.playwright mixes range and list forms",
                Box::new(|c: &mut FileConfig| {
                    c.needs.insert(
                        "playwright".into(),
                        ToolNeeds {
                            versions: ">=1.55, 1.61.1".into(),
                        },
                    );
                }),
            ),
        ];
        for (what, break_it) in cases {
            let mut cfg = full();
            break_it(&mut cfg);
            assert!(!cfg.validate().is_empty(), "validate() accepted {what}");
        }
    }

    #[test]
    fn save_to_writes_and_keeps_the_displaced_file() {
        let dir = std::env::temp_dir().join(format!("faber-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");

        // First save creates the file (and the directory), with no backup to make.
        full().save_to(&path).unwrap();
        assert!(!path.with_extension("toml.bak").exists());

        // The second keeps the first verbatim — the one undo step for a config
        // whose comments the rewrite just dropped.
        let first = std::fs::read_to_string(&path).unwrap();
        let mut next = full();
        next.topic = "changed".into();
        next.save_to(&path).unwrap();
        assert_eq!(
            std::fs::read_to_string(path.with_extension("toml.bak")).unwrap(),
            first
        );
        assert_eq!(
            FileConfig::load(Some(&path)).unwrap().unwrap().topic,
            "changed"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A profile name outside `[A-Za-z0-9._-]` is refused by `validate` (the
    /// save and `check-config`), never by the structural check an unattended
    /// update runs, so a host already running one still starts.
    #[test]
    fn a_profile_name_that_is_not_one_shell_word_is_refused_on_save_only() {
        for good in ["linux-docker", "a.b_c-1", "X"] {
            assert!(profile_name_ok(good), "{good}");
        }
        for bad in ["x.dind true; sudo sh -c 'id'; :", "a b", "é", "a/b", "$x"] {
            assert!(!profile_name_ok(bad), "{bad}");
            let mut cfg = full();
            cfg.profiles[0].name = bad.into();
            assert!(
                cfg.validate()
                    .iter()
                    .any(|e| e.contains("may hold only letters")),
                "{bad}: {:?}",
                cfg.validate()
            );
            assert!(
                !cfg.validate_structural()
                    .iter()
                    .any(|e| e.contains("may hold only")),
                "{bad}"
            );
            assert!(
                cfg.into_profiles("2.330.0").is_ok(),
                "{bad}: boot only warns"
            );
        }
    }

    /// A save keeps the mode of the file it replaces, a first file is 0644
    /// whatever the umask, and no temporary file is left beside it.
    #[cfg(unix)]
    #[test]
    fn save_to_keeps_the_mode_and_leaves_no_temp_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("faber-cfg-mode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o7777;

        full().save_to(&path).unwrap();
        assert_eq!(mode(&path), 0o644);

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut next = full();
        next.topic = "changed".into();
        next.save_to(&path).unwrap();
        assert_eq!(mode(&path), 0o600, "the replaced file's mode is kept");

        // Setuid, setgid and sticky never carry over.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o7640)).unwrap();
        full().save_to(&path).unwrap();
        assert_eq!(mode(&path), 0o640, "only the permission bits are kept");
        next.save_to(&path).unwrap();
        assert_eq!(
            FileConfig::load(Some(&path)).unwrap().unwrap().topic,
            "changed"
        );
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        assert_eq!(left, ["config.toml", "config.toml.bak"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `check-config --config -` reads stdin through
    /// [`FileConfig::load_from_reader`]: the same parse as a file's, and the
    /// same result.
    #[test]
    fn a_config_read_from_a_stream_parses_as_the_file_does() {
        let dir = std::env::temp_dir().join(format!("faber-cfg-stream-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, SAMPLE).unwrap();
        let from_file = FileConfig::load(Some(&path)).unwrap().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let from_stream = FileConfig::load_from_reader(SAMPLE.as_bytes(), "from stdin").unwrap();
        assert_eq!(format!("{from_stream:?}"), format!("{from_file:?}"));

        // A parse error names the stream, as a file's names the file.
        let err = FileConfig::load_from_reader(&b"topic = [1"[..], "from stdin").unwrap_err();
        assert!(
            format!("{err:#}").contains("parsing config from stdin"),
            "{err:#}"
        );
    }

    /// A stream is read to at most 1 MiB: exactly that much parses, and one
    /// byte more is refused rather than buffered.
    #[test]
    fn a_config_stream_is_capped_at_1_mib() {
        // The refusal says 1 MiB, and so does setup's, which shares the cap.
        assert_eq!(FileConfig::READ_CAP, 1 << 20);
        let mut text = b"topic = \"faber\"\n".to_vec();
        text.resize(FileConfig::READ_CAP, b'#');
        let cfg = FileConfig::load_from_reader(&text[..], "from stdin").unwrap();
        assert_eq!(cfg.topic, "faber");
        text.push(b'#');
        let err = FileConfig::load_from_reader(&text[..], "from stdin").unwrap_err();
        assert!(err.to_string().contains("larger than 1 MiB"), "{err}");
    }

    #[test]
    fn a_settings_form_post_deserializes_and_writes() {
        // The shape the dashboard's Settings tab posts: JSON, `null` for every unset
        // option, and no `concurrency` key. The form is the only writer of this
        // file now, so its exact wire shape has to land as a config faber accepts.
        // `auto_pr` is kept here deliberately, from a form version that predates its
        // withdrawal: a browser holding the old Settings JS can POST it at a new
        // binary, and that POST must still write a config rather than 400.
        let posted = r#"{
          "topic": "faber-ci",
          "poll_interval_secs": 20,
          "sweep_interval_secs": 900,
          "runner_timeout_secs": null,
          "provisioning": "demand",
          "trusted_actors": ["alice", "bob"],
          "denied_actors": ["dependabot[bot]"],
          "autolimit": {"reserve_mib": 5120, "headroom_mib": 1536, "band_mib": 1024,
                        "profiling_cap_mib": 4096, "margin": 1.4, "escalation_frac": 0.5,
                        "window": 20, "native_concurrency": 2, "probe_image": "busybox",
                        "hard_ceiling": 4},
          "playwright_drift": {"mode": "gate", "auto_pr": false},
          "retry": {"enabled": true, "max_attempts": 2, "lookback_secs": 1800},
          "profile": [
            {"name": "macos", "labels": ["faber", "macOS", "ARM64"], "backend": "macos-vm",
             "image": "macos-27", "dind_image": null, "cache_volume": false, "env": {}},
            {"name": "linux-docker", "labels": ["faber", "docker"], "backend": "docker",
             "image": "ghcr.io/owner/my-runner:latest", "dind": false, "dind_image": null,
             "cache_volume": false, "env": {"FOO": "bar"}}
          ]
        }"#;
        let cfg: FileConfig = serde_json::from_str(posted).unwrap();
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
        assert_eq!(cfg.runner_timeout_secs, None, "null must mean unset");
        assert_eq!(cfg.provisioning, ProvisioningMode::Demand);
        assert_eq!(cfg.autolimit.hard_ceiling, HardCeiling::Fixed(4));
        assert_eq!(
            cfg.concurrency, DEFAULT_CONCURRENCY,
            "the form omits the superseded key; the default fills in"
        );

        // …and the TOML it produces is a config the daemon parses back the same.
        let reread: FileConfig = toml::from_str(&cfg.to_toml().unwrap()).unwrap();
        assert_eq!(
            serde_json::to_value(&cfg).unwrap(),
            serde_json::to_value(&reread).unwrap()
        );
        assert_eq!(reread.into_profiles("2.330.0").unwrap().len(), 2);
    }

    #[test]
    fn save_to_refuses_an_invalid_config() {
        let dir = std::env::temp_dir().join(format!("faber-cfg-bad-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("config.toml");
        let mut cfg = full();
        cfg.profiles.clear();
        assert!(cfg.save_to(&path).is_err());
        assert!(!path.exists(), "an invalid config must not reach disk");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- provisioning strategy (ADR-0004) ----------------------------------

    #[test]
    fn provisioning_defaults_to_per_job() {
        // Absent key ⇒ single-instance behavior, so an existing config is untouched.
        let cfg: FileConfig = toml::from_str(
            r#"
            [[profile]]
            name = "n"
            labels = ["self-hosted"]
            backend = "macos-vm"
            image = "macos-27"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.provisioning, ProvisioningMode::PerJob);
    }

    #[test]
    fn provisioning_parses_demand() {
        let cfg: FileConfig = toml::from_str(
            r#"
            provisioning = "demand"
            [[profile]]
            name = "n"
            labels = ["self-hosted"]
            backend = "macos-vm"
            image = "macos-27"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.provisioning, ProvisioningMode::Demand);
    }

    #[test]
    fn provisioning_rejects_an_unknown_value() {
        // A typo'd strategy must fail loudly, not silently fall back.
        assert!(toml::from_str::<FileConfig>(
            r#"
            provisioning = "elastic"
            [[profile]]
            name = "n"
            labels = ["self-hosted"]
            backend = "macos-vm"
            image = "macos-27"
            "#,
        )
        .is_err());
    }

    // --- drift policy (ADR-0001) -------------------------------------------

    #[test]
    fn global_drift_policy_defaults_to_surface() {
        // Absent `[playwright_drift]` yields the safe default.
        let cfg: FileConfig = toml::from_str(
            r#"
            [[profile]]
            name = "n"
            labels = ["self-hosted"]
            backend = "macos-vm"
            image = "macos-27"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.playwright_drift.mode, DriftMode::Surface);
    }

    #[test]
    fn global_drift_policy_parses_overrides() {
        let cfg: FileConfig = toml::from_str(
            r#"
            [playwright_drift]
            mode = "gate"

            [[profile]]
            name = "n"
            labels = ["self-hosted"]
            backend = "macos-vm"
            image = "macos-27"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.playwright_drift.mode, DriftMode::Gate);
    }

    #[test]
    fn a_host_config_still_carrying_the_withdrawn_auto_pr_starts_and_stops_advertising_it() {
        // `DriftPolicy` is `deny_unknown_fields` and `FileConfig::load` propagates
        // a parse error, which `run` takes with `?` — so deleting the key outright
        // would abort startup on any host whose config still sets it. Under
        // ADR-0006 that reaches unattended followers on the next release, so the
        // key stays tolerated...
        let cfg: FileConfig = toml::from_str(
            r#"
            [playwright_drift]
            mode = "surface"
            auto_pr = true

            [[profile]]
            name = "n"
            labels = ["self-hosted"]
            backend = "macos-vm"
            image = "macos-27"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.playwright_drift.mode, DriftMode::Surface);

        // ...but a config faber rewrites must stop advertising a key faber
        // ignores, exactly as `concurrency` does.
        let back = toml::to_string(&cfg.playwright_drift).unwrap();
        assert!(
            !back.contains("auto_pr"),
            "withdrawn key written back: {back}"
        );
    }

    #[test]
    fn a_repo_still_shipping_the_withdrawn_auto_pr_keys_keeps_its_live_overrides() {
        // The catch-up-PR coordinates are withdrawn, but the schema is strict:
        // dropping them outright would make an adopting repo's whole
        // `.faber/config.toml` malformed, and a malformed one degrades to the
        // global default — silently taking `mode` and `heal` down with it. So the
        // dead keys stay parseable, and the live ones must survive beside them.
        let rc: RepoConfig = toml::from_str(
            r#"
            [playwright]
            mode = "surface"
            auto_pr = true
            dockerfile = "ci/runner/Dockerfile"
            arg = "PLAYWRIGHT_VERSION"
            heal = false
            ecosystem = "npm"
            "#,
        )
        .unwrap();
        let pw = rc.playwright.as_ref().unwrap();
        assert_eq!(pw.mode, Some(DriftMode::Surface));
        assert_eq!(pw.heal, Some(false));
        assert_eq!(pw.ecosystem.as_deref(), Some("npm"));

        // ...and the withdrawn keys reach nothing: the resolved policy has no
        // place left to put them.
        let resolved = RepoConfig::resolve(&DriftPolicy::default(), Some(&rc));
        assert_eq!(resolved.mode, DriftMode::Surface);
        assert!(!resolved.heal);
    }

    #[test]
    fn resolve_is_repo_wins_field_by_field() {
        let global = DriftPolicy {
            mode: DriftMode::Off,
            ..DriftPolicy::default()
        };
        // Repo overrides mode only; heal falls back to its own default.
        let rc: RepoConfig = toml::from_str(
            r#"
            [playwright]
            mode = "surface"
            ecosystem = "npm"
            "#,
        )
        .unwrap();
        let resolved = RepoConfig::resolve(&global, Some(&rc));
        assert_eq!(resolved.mode, DriftMode::Surface, "repo wins on mode");
        assert!(resolved.heal, "silence means healable");
        assert_eq!(resolved.ecosystem, "npm");
    }

    #[test]
    fn resolve_with_no_repo_config_is_the_global_default() {
        let global = DriftPolicy {
            mode: DriftMode::Gate,
            ..DriftPolicy::default()
        };
        let resolved = RepoConfig::resolve(&global, None);
        assert_eq!(resolved.mode, DriftMode::Gate);
        assert_eq!(resolved.ecosystem, "npm", "ecosystem defaults to npm");
    }

    // --- the trust boundary (issue #42) ------------------------------------

    #[test]
    fn no_gate_section_is_fabers_historical_boundary() {
        // The compatibility contract: this binary reaches installed hosts through
        // an unattended update (ADR-0006), so a config that says nothing about the
        // gate must enforce exactly what faber shipped before it was configurable.
        let cfg: FileConfig = toml::from_str(
            r#"
[[profile]]
name = "n"
labels = ["faber"]
backend = "macos-vm"
image = "macos-27"
"#,
        )
        .unwrap();
        assert_eq!(cfg.gate, GatePolicy::default());
        let b = cfg.into_profiles("2.335.1").unwrap()[0].policy.boundary();
        assert_eq!(b.allowed_events, crate::gate::DEFAULT_EVENTS.to_vec());
        assert!(!b
            .allowed_events
            .contains(&"repository_dispatch".to_string()));
        assert!(!b.allow_public_repos);
    }

    #[test]
    fn an_absent_on_bypass_records_and_a_stated_one_reaches_every_profile() {
        // ADR-0030: hosts rely on bypasses running today, so a config that says
        // nothing must not start enforcing on an unattended update.
        let base = r#"
[[profile]]
name = "n"
labels = ["faber"]
backend = "macos-vm"
image = "macos-27"
"#;
        let narrowed = format!("[gate]\nallowed_events = [\"push\"]\n{base}");
        for text in [base, narrowed.as_str()] {
            let cfg: FileConfig = toml::from_str(text).unwrap();
            assert_eq!(cfg.gate.on_bypass, crate::gate::OnBypass::Record);
            let p = &cfg.into_profiles("2.335.1").unwrap()[0].policy;
            assert_eq!(p.boundary().on_bypass, crate::gate::OnBypass::Record);
        }
        let enforcing = format!("[gate]\non_bypass = \"enforce\"\n{base}");
        let cfg: FileConfig = toml::from_str(&enforcing).unwrap();
        let p = &cfg.into_profiles("2.335.1").unwrap()[0].policy;
        assert_eq!(p.on_bypass(), crate::gate::OnBypass::Enforce);
        // A misspelt value is an error, not a silent `record`.
        let typo = format!("[gate]\non_bypass = \"enforcing\"\n{base}");
        assert!(toml::from_str::<FileConfig>(&typo).is_err());
    }

    #[test]
    fn job_token_dispatches_are_off_unless_stated_and_warned_about_when_on() {
        // ADR-0059 §3: refused on every host unless the operator turns them back
        // on, and loud while they are on.
        let base = r#"
[[profile]]
name = "n"
labels = ["faber"]
backend = "macos-vm"
image = "macos-27"
"#;
        let cfg: FileConfig = toml::from_str(base).unwrap();
        assert!(!cfg.gate.allow_job_token_dispatches);
        assert_eq!(job_token_dispatches_warning(&cfg.gate), None);
        let p = &cfg.into_profiles("2.335.1").unwrap()[0].policy;
        assert!(!p.boundary().allow_job_token_dispatches);

        let on = format!("[gate]\nallow_job_token_dispatches = true\n{base}");
        let cfg: FileConfig = toml::from_str(&on).unwrap();
        let warning = job_token_dispatches_warning(&cfg.gate).expect("warned while on");
        assert!(warning.contains("github-actions[bot]"), "{warning}");
        for profile in cfg.into_profiles("2.335.1").unwrap() {
            assert!(profile.policy.boundary().allow_job_token_dispatches);
        }
    }

    #[test]
    fn repository_dispatch_is_opt_in_via_allowed_events() {
        let base = r#"
[[profile]]
name = "n"
labels = ["faber"]
backend = "macos-vm"
image = "macos-27"
"#;
        let job = || crate::gate::JobContext {
            event: "repository_dispatch".to_string(),
            actor: "owner".to_string(),
            triggering_actor: None,
            base_repo: "owner/demo".to_string(),
            private: true,
            head_repo: None,
            code: crate::gate::Code::Own,
            labels: vec!["faber".to_string()],
            status: "queued".to_string(),
        };
        // No [gate]: refused, exactly as before.
        let cfg: FileConfig = toml::from_str(base).unwrap();
        let p = &cfg.into_profiles("2.335.1").unwrap()[0].policy;
        assert_eq!(
            crate::gate::evaluate(&job(), p),
            crate::gate::Verdict::Refuse(crate::gate::RefuseReason::DisallowedEvent(
                "repository_dispatch".to_string()
            ))
        );
        // Listed: loads without error and the boundary includes it.
        let listed =
            format!("[gate]\nallowed_events = [\"push\", \"repository_dispatch\"]\n{base}");
        let cfg: FileConfig = toml::from_str(&listed).unwrap();
        assert!(cfg.allowed_events_unknown().is_empty());
        let p = &cfg.into_profiles("2.335.1").unwrap()[0].policy;
        assert_eq!(
            p.boundary().allowed_events,
            vec!["push".to_string(), "repository_dispatch".to_string()]
        );
        assert_eq!(
            crate::gate::evaluate(&job(), p),
            crate::gate::Verdict::Provision
        );
    }

    #[test]
    fn the_gate_section_narrows_every_profile_alike() {
        // Host-wide, not per-profile: "what will this machine run?" must not
        // depend on which label set a job happened to match.
        let cfg: FileConfig = toml::from_str(
            r#"
[gate]
allowed_events = ["push"]
allow_public_repos = true

[[profile]]
name = "a"
labels = ["faber", "macOS"]
backend = "macos-vm"
image = "macos-27"

[[profile]]
name = "b"
labels = ["faber", "docker"]
backend = "docker"
image = "ghcr.io/owner/my-runner:latest"
dind = false
"#,
        )
        .unwrap();
        for p in cfg.into_profiles("2.335.1").unwrap() {
            let b = p.policy.boundary();
            assert_eq!(b.allowed_events, vec!["push".to_string()], "{}", p.name);
            assert!(b.allow_public_repos, "{}", p.name);
        }
    }

    #[test]
    fn allow_fork_prs_is_a_startup_error_not_a_silent_no_op() {
        // The key an operator reaches for first, and the one that must not exist
        // while job code reaches a privileged Docker daemon (issue #43). Without
        // `deny_unknown_fields` this would parse, be ignored, and leave a host
        // looking like it had accepted the setting.
        let err = toml::from_str::<FileConfig>(
            r#"
[gate]
allow_fork_prs = true

[[profile]]
name = "n"
labels = ["faber"]
backend = "macos-vm"
image = "macos-27"
"#,
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("allow_fork_prs"),
            "the error must name the key: {err}"
        );
    }

    #[test]
    fn an_event_outside_the_ceiling_is_a_validation_error() {
        // Two different mistakes, one check. `pull_request_target` is someone
        // trying to widen the boundary — impossible at runtime, but it must not
        // look accepted. `puhs` is a typo that would otherwise silently stop
        // provisioning every push job.
        for bad in ["pull_request_target", "puhs"] {
            let mut cfg = full();
            cfg.gate.allowed_events = vec!["push".to_string(), bad.to_string()];
            let errs = cfg.validate();
            assert!(
                errs.iter().any(|e| e.contains(bad)),
                "validate() accepted {bad}: {errs:?}"
            );
            // And even if it reached the runtime, the gate would drop it.
            let b = cfg.into_profiles("2.335.1").unwrap()[0].policy.boundary();
            assert_eq!(b.allowed_events, vec!["push".to_string()]);
        }
    }

    #[test]
    fn an_empty_event_list_is_valid_because_it_is_the_strictest_setting() {
        // Refusing to save "provision nothing" would mean the one direction an
        // operator can always safely go is the one the validator blocks.
        let mut cfg = full();
        cfg.gate.allowed_events = Vec::new();
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
    }

    #[test]
    fn the_boundary_survives_a_config_round_trip() {
        // The settings page reads, edits and rewrites the file, and this section is
        // the one where a dropped key silently widens what the host will run.
        let mut cfg = full();
        cfg.gate.allowed_events = vec!["push".to_string(), "schedule".to_string()];
        cfg.gate.allow_public_repos = true;
        let text = cfg.to_toml().unwrap();
        let back: FileConfig = toml::from_str(&text).unwrap();
        assert_eq!(back.gate, cfg.gate);
    }

    #[test]
    fn the_gate_table_survives_the_needs_table_before_it() {
        // TOML ordering is a real hazard in this file (see `FileConfig`'s note on
        // `profiles`): a table header swallows every key written after it, so a new
        // section landing between `[needs.*]` and `[[profile]]` has to be proven,
        // not assumed.
        let mut cfg = full();
        cfg.needs.insert(
            "playwright".into(),
            ToolNeeds {
                versions: ">=1.55, <1.70".into(),
            },
        );
        cfg.gate.allowed_events = vec!["push".into()];
        cfg.gate.allow_public_repos = true;
        let text = cfg.to_toml().unwrap();
        let back: FileConfig = toml::from_str(&text).unwrap();
        assert_eq!(back.gate, cfg.gate, "in:\n{text}");
        assert_eq!(back.needs.len(), 1, "in:\n{text}");
        assert_eq!(back.profiles.len(), cfg.profiles.len(), "in:\n{text}");
    }

    // --- the daemon grant (issue #43) --------------------------------------

    #[test]
    fn a_profile_says_whether_its_jobs_get_a_daemon() {
        let toml = r#"
[[profile]]
name = "no-docker"
labels = ["faber", "linux"]
backend = "docker"
image = "ghcr.io/owner/my-runner:latest"
dind = false

[[profile]]
name = "docker"
labels = ["faber", "docker"]
backend = "docker"
image = "ghcr.io/owner/my-runner:latest"
dind = true

[[profile]]
name = "legacy"
labels = ["faber", "old"]
backend = "docker"
image = "ghcr.io/owner/my-runner:latest"
"#;
        let cfg: FileConfig = toml::from_str(toml).unwrap();
        let grants: Vec<DindGrant> = cfg
            .into_profiles("2.335.1")
            .unwrap()
            .iter()
            .map(|p| match &p.backend {
                Backend::Docker(d) => d.dind,
                Backend::MacosVm(_) => {
                    unreachable!("all three are docker")
                }
            })
            .collect();
        assert_eq!(
            grants,
            vec![DindGrant::Denied, DindGrant::Granted, DindGrant::Unstated]
        );
    }

    #[test]
    fn an_omitted_grant_is_not_a_denial() {
        // The upgrade contract. A profile written before the key existed keeps its
        // daemon, because this binary arrives on existing hosts unattended
        // (ADR-0006) and silently withdrawing a capability their workflows use
        // would break running CI with nobody watching. It is loud, not silent —
        // `into_profiles` warns, and it reports as `unstated` everywhere.
        let cfg: FileConfig = toml::from_str(
            r#"
[[profile]]
name = "legacy"
labels = ["faber", "docker"]
backend = "docker"
image = "ghcr.io/owner/my-runner:latest"
"#,
        )
        .unwrap();
        assert_eq!(cfg.profiles[0].dind, None);
        let profiles = cfg.into_profiles("2.335.1").unwrap();
        match &profiles[0].backend {
            Backend::Docker(d) => {
                assert_eq!(d.dind, DindGrant::Unstated);
                assert!(d.dind.granted(), "an unstated grant still grants");
            }
            Backend::MacosVm(_) => unreachable!(),
        }
    }

    #[test]
    fn the_grant_survives_a_config_round_trip() {
        // The settings page reads, edits and rewrites the file. All three states
        // have to come back out as they went in — in particular `false` must not
        // round-trip to absent, which would silently re-grant the daemon.
        for (written, expect) in [(Some(true), "dind = true"), (Some(false), "dind = false")] {
            let mut cfg = full();
            cfg.profiles[0].dind = written;
            let text = cfg.to_toml().unwrap();
            assert!(text.contains(expect), "{expect} missing from:\n{text}");
            let back: FileConfig = toml::from_str(&text).unwrap();
            assert_eq!(back.profiles[0].dind, written);
        }
        let mut cfg = full();
        cfg.profiles[0].dind = None;
        let text = cfg.to_toml().unwrap();
        let back: FileConfig = toml::from_str(&text).unwrap();
        assert_eq!(back.profiles[0].dind, None);
    }

    // --- the Linux dind wall (AGENTS.md/ADR-0025 §"On Linux, dind waits for a
    //     wall") -------------------------------------------------------------

    #[test]
    fn dind_needs_wall_truth_table() {
        use DindGrant::*;
        fn facts(is_linux: bool, holds_credential: bool, jobs_behind_wall: bool) -> WallFacts {
            WallFacts {
                is_linux,
                holds_credential,
                jobs_behind_wall,
                vm_session_account: || false,
            }
        }
        // (is_linux, holds_credential, jobs_behind_wall, grant) -> expected
        let cases: &[(bool, bool, bool, DindGrant, bool)] = &[
            // The one case PR1 actually produces on a Linux host: the wall this
            // predicate exists for.
            (true, true, false, Granted, true),
            // Absent means granted (config.rs's own upgrade-compatibility rule,
            // see `FileProfile::dind`'s doc) — exactly as exposed as `Granted`.
            (true, true, false, Unstated, true),
            // Denied starts no daemon at all, on any platform.
            (true, true, false, Denied, false),
            (false, true, false, Denied, false),
            // macOS: the account split (ADR-0025) confines a granted dind's root
            // to Docker Desktop's VM, so the wall does not apply there.
            (false, true, false, Granted, false),
            (false, true, false, Unstated, false),
            // PR2 (ADR-0026): once jobs run behind a wall of their own, Linux
            // stops needing this one too, with no change to the predicate itself.
            (true, true, true, Granted, false),
            (true, true, true, Unstated, false),
            // No credential to protect — hypothetical today (PR1 always passes
            // `true`), but the predicate must still say so honestly.
            (true, false, false, Granted, false),
            (true, false, false, Unstated, false),
        ];
        for &(is_linux, holds_credential, jobs_behind_wall, grant, expected) in cases {
            assert_eq!(
                dind_needs_wall(facts(is_linux, holds_credential, jobs_behind_wall), grant),
                expected,
                "is_linux={is_linux} holds_credential={holds_credential} \
                 jobs_behind_wall={jobs_behind_wall} grant={grant:?}"
            );
        }
    }

    #[test]
    fn wall_facts_current_is_todays_pr1_shape() {
        // Pins the one place PR2 (ADR-0026) will change: today every host that
        // claims jobs holds a credential, and none run jobs behind a wall yet.
        // `is_linux` alone tracks the real compile target.
        let facts = WallFacts::current();
        assert!(facts.holds_credential);
        assert!(!facts.jobs_behind_wall);
        assert_eq!(facts.is_linux, cfg!(target_os = "linux"));
    }

    #[test]
    fn validate_rejects_the_linux_dind_wall_while_structural_accepts_it() {
        // A config with nothing else wrong: a Docker profile that never states
        // `dind`, so the grant is `Unstated` — "absent means granted" — the
        // realistic upgrade case, not a config an operator wrote to be unsafe.
        let cfg: FileConfig = toml::from_str(
            r#"
            [[profile]]
            name = "linux-docker"
            labels = ["faber", "docker"]
            backend = "docker"
            image = "ghcr.io/owner/my-runner:latest"
        "#,
        )
        .unwrap();

        // Structural: parses and would start. Never rejects on policy — the
        // updater's future `--for-update` gate depends on exactly this.
        assert!(
            cfg.validate_structural().is_empty(),
            "{:?}",
            cfg.validate_structural()
        );

        // The real `validate()` on an actual macOS test run doesn't see this —
        // `WallFacts::current().is_linux` is `false` here — so the policy check
        // is exercised through the same facts-injected seam `validate()` itself
        // uses, matching the pattern `crate::platform` already uses for its own
        // environment reads (issue #47).
        let linux = WallFacts {
            is_linux: true,
            holds_credential: true,
            jobs_behind_wall: false,
            vm_session_account: || false,
        };
        let not_linux = WallFacts {
            is_linux: false,
            ..linux
        };
        assert!(cfg.validate_with_facts(not_linux).is_empty());
        let errs = cfg.validate_with_facts(linux);
        assert!(
            errs.iter()
                .any(|e| e.contains("linux-docker") && e.contains("ADR-0025")),
            "{errs:?}"
        );

        // A denied grant never needs the wall, on either platform.
        let mut denied = cfg.clone();
        denied.profiles[0].dind = Some(false);
        assert!(denied.validate_with_facts(linux).is_empty());

        // A native profile is never a dind concern regardless of platform.
        let native: FileConfig = toml::from_str(
            r#"
            [[profile]]
            name = "macos-native"
            labels = ["faber", "macOS"]
            backend = "macos-vm"
            image = "macos-27"
        "#,
        )
        .unwrap();
        assert!(native.validate_with_facts(linux).is_empty());
    }

    // --- ADR-0052: a native profile is a config error -----------------------

    const NATIVE_AND_DOCKER: &str = r#"
        [[profile]]
        name = "macos-native"
        labels = ["faber", "macOS"]
        backend = "native"

        [[profile]]
        name = "linux-docker"
        labels = ["faber", "docker"]
        backend = "docker"
        image = "ghcr.io/owner/my-runner:latest"
        dind = false

        [[profile]]
        name = "macos-native-2"
        labels = ["faber", "macOS", "big"]
        backend = "native"
    "#;

    #[test]
    fn a_native_profile_is_a_structural_error_naming_it_and_macos_vm() {
        // Still parses, so the error can name the profile and the fix.
        let cfg: FileConfig = toml::from_str(NATIVE_AND_DOCKER).unwrap();
        let errs = cfg.validate_structural();
        let native: Vec<&String> = errs.iter().filter(|e| e.contains("native")).collect();
        assert_eq!(native.len(), 2, "{errs:?}");
        for (e, name) in native.iter().zip(["macos-native", "macos-native-2"]) {
            assert!(e.starts_with(&format!("profile '{name}': ")), "{e}");
            assert!(e.contains("backend = \"macos-vm\""), "{e}");
            assert!(e.contains("ADR-0052"), "{e}");
        }
        // The docker profile beside them is fine, and `validate` (check-config,
        // the settings save) refuses the same file.
        assert_eq!(errs.len(), 2, "{errs:?}");
        assert!(cfg.validate().iter().any(|e| e.contains("macos-native-2")));
    }

    #[test]
    fn into_profiles_refuses_a_native_profile() {
        let cfg: FileConfig = toml::from_str(NATIVE_AND_DOCKER).unwrap();
        let err = cfg
            .into_profiles("2.330.0")
            .expect_err("a native profile is never built")
            .to_string();
        assert!(err.starts_with("profile 'macos-native': "), "{err}");
        assert!(err.contains("macos-vm"), "{err}");
    }

    // --- ADR-0051: the per-host VM layout --------------------------------------

    #[test]
    fn absent_layout_keys_keep_todays_layout_and_are_not_written_back() {
        let cfg = vm_profile("");
        assert_eq!(cfg.vm_layout(), VmLayout::default());
        assert_eq!(
            VmLayout::default(),
            VmLayout {
                linux_vm: true,
                macos_vms: 2,
                macos_vm_mib: 8 * 1024,
            }
        );
        assert!(cfg.linux_vm.runs());
        assert!(!cfg.macos_vm.turned_on());
        let text = toml::to_string(&cfg).unwrap();
        assert!(!text.contains("macos_vm"), "{text}");
        let back: FileConfig = toml::from_str(&text).unwrap();
        assert_eq!(back.linux_vm, LinuxVmFile::default(), "{text}");
    }

    #[test]
    fn the_layout_keys_resolve_and_are_checked_where_a_human_looks() {
        let mut cfg = vm_profile("");
        cfg.linux_vm.enabled = Some(false);
        cfg.macos_vm = toml::from_str("count = 1\nmemory_mib = 12288\n").unwrap();
        assert_eq!(
            cfg.vm_layout(),
            VmLayout {
                linux_vm: false,
                macos_vms: 1,
                macos_vm_mib: 12288,
            }
        );
        assert!(
            cfg.validate_structural().is_empty(),
            "{:?}",
            cfg.validate_structural()
        );
        // Written back once set, so a save keeps the operator's layout.
        let back: FileConfig = toml::from_str(&toml::to_string(&cfg).unwrap()).unwrap();
        assert_eq!(back.vm_layout(), cfg.vm_layout());

        let bad = MacosVmFile {
            count: Some(3),
            memory_mib: Some(512),
        };
        assert_eq!(
            bad.errors(),
            [
                "macos_vm.count must be 0 to 2: Apple allows 2 macOS VMs per Mac",
                "macos_vm.memory_mib must be 1024 to 262144"
            ]
        );
        cfg.macos_vm = bad;
        // The two above. That the profile's 8 GiB VM is more than 512 MiB is
        // policy, for `vm_layout_host_errors`, never the updater's gate.
        assert_eq!(
            cfg.validate_structural().len(),
            2,
            "{:?}",
            cfg.validate_structural()
        );
        // A boot on the same file clamps to Apple's limit and keeps the default
        // size, never fatal (AGENTS.md).
        assert_eq!(cfg.vm_layout().macos_vms, 2);
        assert_eq!(cfg.vm_layout().macos_vm_mib, 8 * 1024);
        assert!(toml::from_str::<FileConfig>("[macos_vm]\nvms = 1\n").is_err());
    }

    #[test]
    fn a_profile_vm_may_not_outgrow_the_size_the_share_counts() {
        fn unasked() -> bool {
            panic!("no count is set, so the session account is never asked about")
        }
        // Unset: a profile of any size passes, as before the key existed.
        assert!(vm_profile("memory_mib = 16384")
            .vm_layout_host_errors(false, unasked)
            .is_empty());
        let mut cfg = vm_profile("memory_mib = 16384");
        cfg.macos_vm.memory_mib = Some(8192);
        let errs = cfg.vm_layout_host_errors(false, unasked);
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(
            errs[0]
                .starts_with("profile 'macos': memory_mib 16384 is more than macos_vm.memory_mib"),
            "{errs:?}"
        );
        // Policy, not structure: the updater's gate never holds a release back
        // over it, while a human-run check and a save refuse it.
        assert!(cfg.validate_structural().is_empty());
        let facts = WallFacts {
            is_linux: false,
            vm_session_account: unasked,
            ..WallFacts::current()
        };
        assert_eq!(cfg.validate_with_facts(facts), errs);
        // The profile's default size counts too.
        let mut small = vm_profile("");
        small.macos_vm.memory_mib = Some(4096);
        assert_eq!(small.vm_layout_host_errors(false, unasked).len(), 1);
        assert!(small.validate_structural().is_empty());
    }

    #[test]
    fn turning_macos_vms_on_needs_the_session_account() {
        fn present() -> bool {
            true
        }
        fn absent() -> bool {
            false
        }
        fn never() -> bool {
            panic!("asked about the session account for a file that does not turn macOS VMs on")
        }
        let mut cfg = vm_profile("");
        // Absent and zero are not a choice to turn them on: never probed.
        assert!(cfg.vm_layout_host_errors(false, never).is_empty());
        cfg.macos_vm.count = Some(0);
        assert!(cfg.vm_layout_host_errors(false, never).is_empty());

        cfg.macos_vm.count = Some(1);
        assert!(cfg.vm_layout_host_errors(false, present).is_empty());
        let errs = cfg.vm_layout_host_errors(false, absent);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("_faber_mac"), "{errs:?}");
        assert!(errs[0].contains("sudo faber setup"), "{errs:?}");
        // Policy, not structure: the updater's gate never sees it.
        assert!(cfg.validate_structural().is_empty());
        let facts = WallFacts {
            is_linux: false,
            vm_session_account: absent,
            ..WallFacts::current()
        };
        assert_eq!(cfg.validate_with_facts(facts), errs);
        // Linux has no macOS VMs and no session account to make: setup is not
        // the answer there, and the key is a warning that it does not apply.
        assert!(cfg.vm_layout_host_errors(true, never).is_empty());
        let linux = vm_layout_warnings(&cfg, true, false);
        assert_eq!(linux.len(), 1, "{linux:?}");
        assert!(
            linux[0].starts_with("[macos_vm] does not apply on this host"),
            "{linux:?}"
        );
        assert!(!linux[0].contains("sudo faber setup"), "{linux:?}");
        assert!(vm_layout_warnings(&cfg, false, true).is_empty());
    }

    #[test]
    fn switching_off_a_linux_vm_this_host_does_not_run_is_a_warning() {
        let mut cfg = vm_profile("");
        // Absent or on: nothing to say anywhere.
        assert!(vm_layout_warnings(&cfg, true, false).is_empty());
        cfg.linux_vm.enabled = Some(true);
        assert!(vm_layout_warnings(&cfg, false, false).is_empty());
        cfg.linux_vm.enabled = Some(false);
        // A set-up Mac runs faber's own Linux VM: the switch applies.
        assert!(vm_layout_warnings(&cfg, false, true).is_empty());
        // Linux, and a LEGACY Mac: Docker is the host's, and the key does nothing.
        for (is_linux, has_engine) in [(true, false), (false, false)] {
            let w = vm_layout_warnings(&cfg, is_linux, has_engine);
            assert_eq!(w.len(), 1, "{w:?}");
            assert!(
                w[0].starts_with("[linux_vm] enabled = false does nothing on this host"),
                "{w:?}"
            );
        }
        // A warning, never an error: the file is valid.
        assert!(cfg.validate_structural().is_empty());
    }

    const DOCKER_AND_VM: &str = r#"
        [[profile]]
        name = "linux-docker"
        labels = ["faber", "docker"]
        backend = "docker"
        image = "ghcr.io/owner/my-runner:latest"
        dind = false

        [[profile]]
        name = "macos"
        labels = ["faber", "macOS", "ARM64"]
        backend = "macos-vm"
        image = "macos-27"
    "#;

    fn names(profiles: &[Profile]) -> Vec<&str> {
        profiles.iter().map(|p| p.name.as_str()).collect()
    }

    #[test]
    fn the_layout_drops_the_profiles_it_starts_no_vm_for() {
        let cfg: FileConfig = toml::from_str(DOCKER_AND_VM).unwrap();
        let profiles = || cfg.into_profiles("2.330.0").unwrap();

        // Today's layout drops nothing.
        let (kept, drops) = ignore_vm_layout(profiles(), VmLayout::default(), true);
        assert_eq!(names(&kept), ["linux-docker", "macos"]);
        assert_eq!(drops, VmLayoutDrops::default());
        assert!(drops.notices().is_empty());

        let off = VmLayout {
            linux_vm: false,
            ..VmLayout::default()
        };
        let (kept, drops) = ignore_vm_layout(profiles(), off, true);
        assert_eq!(names(&kept), ["macos"]);
        assert_eq!(drops.docker, ["linux-docker"]);
        let notices = drops.notices();
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0].0, "linux_vm_off");
        assert!(
            notices[0]
                .1
                .contains("docker profile linux-docker gets no runner"),
            "{notices:?}"
        );

        // Where Docker is the host's own (Linux, LEGACY), the switch does nothing.
        let (kept, _) = ignore_vm_layout(profiles(), off, false);
        assert_eq!(names(&kept), ["linux-docker", "macos"]);

        let no_macs = VmLayout {
            macos_vms: 0,
            ..VmLayout::default()
        };
        let (kept, drops) = ignore_vm_layout(profiles(), no_macs, false);
        assert_eq!(names(&kept), ["linux-docker"]);
        assert_eq!(drops.macos, ["macos"]);
        assert_eq!(drops.notices()[0].0, "macos_vms_off");
    }

    // --- ADR-0026: backend = "macos-vm" ----------------------------------------

    fn vm_profile(extra: &str) -> FileConfig {
        toml::from_str(&format!(
            r#"
            [[profile]]
            name = "macos"
            labels = ["faber", "macOS", "ARM64"]
            backend = "macos-vm"
            image = "macos-27"
            {extra}
        "#
        ))
        .unwrap()
    }

    #[test]
    fn a_macos_vm_profile_keeps_its_labels_and_takes_the_defaults() {
        let cfg = vm_profile("");
        assert!(cfg.validate_structural().is_empty());
        let profiles = cfg.into_profiles("2.330.0").unwrap();
        assert_eq!(profiles[0].labels, ["faber", "macOS", "ARM64"]);
        assert_eq!(profiles[0].backend.kind(), "macos-vm");
        let Backend::MacosVm(vm) = &profiles[0].backend else {
            panic!("not a macos-vm backend");
        };
        assert_eq!(vm.image.as_str(), "macos-27");
        assert_eq!(
            vm.runner_version, "2.330.0",
            "the host's runner version names the tools disk, with the installed agent"
        );
        assert_eq!(vm.cpus.get(), DEFAULT_VM_CPUS);
        assert_eq!(vm.memory_mib.get(), DEFAULT_VM_MEMORY_MIB);
        assert_eq!(
            vm.timeouts.job,
            crate::docker::DEFAULT_RUNNER_TIMEOUT,
            "the lifetime cap is the docker backend's, unless runner_timeout_secs says"
        );
    }

    #[test]
    fn a_macos_vm_profile_follows_the_hosts_runner_timeout_and_its_own_sizes() {
        let mut cfg = vm_profile("cpus = 6\nmemory_mib = 6144");
        cfg.runner_timeout_secs = Some(600);
        let profiles = cfg.into_profiles("2.330.0").unwrap();
        let Backend::MacosVm(vm) = &profiles[0].backend else {
            panic!()
        };
        assert_eq!((vm.cpus.get(), vm.memory_mib.get()), (6, 6144));
        assert_eq!(vm.timeouts.job, Duration::from_secs(600));
    }

    #[test]
    fn absent_vm_keys_are_not_written_back() {
        let out = toml::to_string(&vm_profile("")).unwrap();
        let profile = &out[out.find("[[profile]]").expect("a profile table")..];
        assert!(profile.contains("backend = \"macos-vm\""), "{profile}");
        for absent in ["cpus", "memory_mib", "dind", "git_mirror", "mirror"] {
            assert!(
                !profile.contains(absent),
                "{absent} written back: {profile}"
            );
        }
    }

    #[test]
    fn check_config_rejects_a_bad_macos_vm_profile_everywhere_a_human_looks() {
        let cases: &[(&str, FileConfig)] = &[
            ("requires `image`", {
                let mut c = vm_profile("");
                c.profiles[0].image = None;
                c
            }),
            ("never a path", {
                let mut c = vm_profile("");
                c.profiles[0].image = Some("../etc/golden".into());
                c
            }),
            ("`cpus` must be 1 to 64", vm_profile("cpus = 0")),
            ("`memory_mib` must be", vm_profile("memory_mib = 512")),
            (
                "`env` is not supported",
                vm_profile("[profile.env]\nK = \"v\""),
            ),
            ("belong to a docker profile", vm_profile("dind = true")),
            (
                "git_mirror needs a docker backend",
                vm_profile("git_mirror = true"),
            ),
        ];
        for (want, cfg) in cases {
            let errs = cfg.validate_structural();
            assert!(
                errs.iter().any(|e| e.contains(want)),
                "{want:?} not in {errs:?}"
            );
        }
        // And vm keys on a docker profile are a mistake worth naming.
        let mut docker: FileConfig = toml::from_str(NATIVE_AND_DOCKER).unwrap();
        docker.profiles[1].cpus = Some(4);
        assert!(docker
            .validate_structural()
            .iter()
            .any(|e| e.contains("belong to a macos-vm profile")));
    }

    #[test]
    fn a_daemon_booting_on_a_bad_macos_vm_profile_warns_and_drops_it_instead_of_failing() {
        let mut cfg = vm_profile("");
        cfg.profiles[0].image = None;
        let profiles = cfg.into_profiles("2.330.0").unwrap();
        assert!(profiles.is_empty());
    }

    /// The tools disk is derived (#260): faber names it from the runner version
    /// and the installed agent's bytes, so there is no key to set, or get wrong.
    #[test]
    fn a_macos_vm_profile_has_no_tools_key() {
        let err = toml::from_str::<FileConfig>(
            r#"
            [[profile]]
            name = "macos"
            labels = ["faber", "macOS", "ARM64"]
            backend = "macos-vm"
            image = "macos-27"
            tools = "runner-2.330.0-agent-0.4.0"
        "#,
        )
        .unwrap_err();
        assert!(err.to_string().contains("tools"), "{err}");
    }

    /// The config a host holds after the cut's host sitting: the native profile,
    /// its backend switched and pointed at the golden image, and nothing else. `check-config` runs
    /// exactly this validation.
    #[test]
    fn the_cut_runbooks_macos_profile_is_accepted() {
        let cfg: FileConfig = toml::from_str(
            r#"
            [[profile]]
            name = "macos-native"
            labels = ["self-hosted", "macOS", "ARM64"]
            backend = "macos-vm"
            image = "golden-27"
        "#,
        )
        .unwrap();
        assert!(
            cfg.validate_structural().is_empty(),
            "{:?}",
            cfg.validate_structural()
        );
        assert_eq!(cfg.into_profiles("2.330.0").unwrap().len(), 1);
    }

    fn overrides_tmp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "faber-overrides-{tag}-{}-{}",
            std::process::id(),
            crate::audit::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("overrides.json")
    }

    #[test]
    fn an_overrides_update_keeps_every_concurrent_writers_fields() {
        let path = overrides_tmp("concurrent");
        std::thread::scope(|scope| {
            for i in 0..8u64 {
                let path = &path;
                scope.spawn(move || {
                    let _guard = overrides_lock();
                    Overrides::update_at(path, |o| {
                        o.repo_holds.push(RepoHold {
                            repo: format!("owner/demo{i}"),
                            reason: "r".into(),
                            by: "b".into(),
                            since_ms: i,
                        })
                    })
                    .unwrap();
                });
            }
        });
        let back: Overrides =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.repo_holds.len(), 8);
        // Atomic: no temporary file is left beside it.
        let left: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left.len(), 1, "{left:?}");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn an_overrides_update_never_saves_defaults_over_a_file_it_cannot_read() {
        let path = overrides_tmp("corrupt");
        std::fs::write(&path, "{ not json").unwrap();
        let err = Overrides::update_at(&path, |o| o.margin = Some(2.0));
        assert!(err.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{ not json");
        // A missing file is not an error: it starts from defaults.
        std::fs::remove_file(&path).unwrap();
        Overrides::update_at(&path, |o| o.margin = Some(2.0)).unwrap();
        let back: Overrides =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(back.margin, Some(2.0));
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn repo_config_cannot_widen_trust() {
        // The whole safety argument: a PR-reachable `.faber/config.toml` is
        // structurally unable to name a trust/gate/backend/image knob — every one
        // of these is an unknown field the strict schema rejects.
        for forbidden in [
            r#"trusted_actors = ["attacker"]"#,
            r#"backend = "docker""#,
            r#"image = "ghcr.io/owner/my-runner:latest""#,
            "[gate]\nallow = true",
            "[playwright]\ntrust = true", // unknown key inside the table
            "[playwright]\nbackend = \"x\"",
            // The daemon grant (issue #43) is the widest privilege faber hands
            // out — a PR that could set it would be asking for host root in a
            // diff. It belongs to the operator's own config, and nowhere else.
            "dind = true",
            "[playwright]\ndind = true",
            // The trust boundary (issue #42). A gate key here would let a fork PR
            // widen the boundary that is meant to be judging it — the single most
            // dangerous thing an in-repo file could be allowed to say.
            "[gate]\nallow_public_repos = true",
            r#"[gate]
allowed_events = ["pull_request_target"]"#,
            "allow_public_repos = true",
            r#"allowed_events = ["push"]"#,
            // ADR-0030: a PR that could switch enforcement off would be asking
            // for its own bypass to go unanswered.
            "[gate]\non_bypass = \"record\"",
            "[gate]\non_bypass = \"enforce\"",
            r#"on_bypass = "record""#,
            // ADR-0059: a job that could turn job-token dispatches back on would
            // be asking for its own pushed code to run.
            "[gate]\nallow_job_token_dispatches = true",
            "allow_job_token_dispatches = true",
        ] {
            assert!(
                toml::from_str::<RepoConfig>(forbidden).is_err(),
                "RepoConfig must reject: {forbidden:?}"
            );
        }
    }

    #[test]
    fn repo_config_rejects_a_bad_mode_value() {
        assert!(toml::from_str::<RepoConfig>("[playwright]\nmode = \"cancel\"").is_err());
    }

    // --- on-demand runner images (ADR-0008) --------------------------------

    #[test]
    fn a_repo_is_healed_unless_it_says_otherwise() {
        // The default has to be "yes": a repo that bakes the opt-in label and says
        // nothing more is exactly the repo this exists for.
        let resolved = RepoConfig::resolve(&DriftPolicy::default(), None);
        assert!(resolved.heal);
    }

    #[test]
    fn a_repo_can_opt_out_of_being_healed() {
        let rc: RepoConfig = toml::from_str("[playwright]\nheal = false").unwrap();
        assert!(!RepoConfig::resolve(&DriftPolicy::default(), Some(&rc)).heal);
    }

    #[test]
    fn a_repo_still_cannot_choose_its_own_image() {
        // Healing does not widen what a pull request can say. The recipe and the
        // base are the operator's; a repo's only word on the subject is "no".
        for forbidden in [
            "[playwright]\ndockerfile_context = \"./ci\"",
            "[runner_image]\nbuild = \"on-demand\"",
            "[playwright]\nbase = \"evil:latest\"",
        ] {
            assert!(
                toml::from_str::<RepoConfig>(forbidden).is_err(),
                "RepoConfig must reject: {forbidden:?}"
            );
        }
    }

    #[test]
    fn on_demand_image_building_is_off_unless_asked_for() {
        // It ships to a follower host that nobody is watching (ADR-0006), so a
        // release carrying it must change nothing on a host that didn't ask.
        let cfg: FileConfig = toml::from_str(SAMPLE).unwrap();
        assert_eq!(cfg.runner_image.build, ImageBuildMode::Off);
        assert!(!cfg.runner_image.build.enabled());
    }

    #[test]
    fn the_runner_image_section_parses_with_its_defaults() {
        let cfg: FileConfig = toml::from_str(
            r#"
            [runner_image]
            build = "on-demand"
            "#,
        )
        .unwrap();
        assert!(cfg.runner_image.build.enabled());
        assert_eq!(cfg.runner_image.keep, 3);
        assert_eq!(cfg.runner_image.idle_ttl_hours, 168);
        assert_eq!(cfg.runner_image.build_timeout_secs, 1200);
        assert!(cfg.runner_image.dockerfile.is_none());
    }

    #[test]
    fn the_runner_image_section_rejects_an_unknown_knob() {
        assert!(toml::from_str::<FileConfig>("[runner_image]\npush = true").is_err());
    }

    // --- operator-owned needs allowlist (ADR-0010 §4) -----------------------

    #[test]
    fn needs_section_is_empty_by_default() {
        // Absent `[needs]` entirely ⇒ every tool fails closed, not open.
        let cfg: FileConfig = toml::from_str(SAMPLE).unwrap();
        assert!(cfg.needs.is_empty());
        assert_eq!(
            cfg.needs_allow("playwright", "1.61.1"),
            NeedsVerdict::UnknownTool
        );
    }

    #[test]
    fn needs_entry_rejects_an_unknown_knob() {
        assert!(toml::from_str::<FileConfig>(
            "[needs.playwright]\nversions = \"1.61.1\"\nchannel = \"stable\"\n"
        )
        .is_err());
    }

    #[test]
    fn needs_section_parses_the_range_form_from_toml() {
        let cfg: FileConfig = toml::from_str(
            r#"
            [needs.playwright]
            versions = ">=1.55, <1.70"

            [[profile]]
            name = "n"
            labels = ["faber"]
            backend = "macos-vm"
            image = "macos-27"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.needs["playwright"].versions, ">=1.55, <1.70");
    }

    #[test]
    fn needs_allow_range_form() {
        let mut cfg = FileConfig::default();
        cfg.needs.insert(
            "playwright".into(),
            ToolNeeds {
                versions: ">=1.55, <1.70".into(),
            },
        );
        assert_eq!(
            cfg.needs_allow("playwright", "1.60.0"),
            NeedsVerdict::Allowed
        );
        // Bounds: `>=` is inclusive, `<` is exclusive.
        assert_eq!(
            cfg.needs_allow("playwright", "1.55.0"),
            NeedsVerdict::Allowed
        );
        assert_ne!(
            cfg.needs_allow("playwright", "1.70.0"),
            NeedsVerdict::Allowed
        );
        assert_ne!(
            cfg.needs_allow("playwright", "1.54.9"),
            NeedsVerdict::Allowed
        );
    }

    #[test]
    fn needs_allow_list_form() {
        let mut cfg = FileConfig::default();
        cfg.needs.insert(
            "playwright".into(),
            ToolNeeds {
                versions: "1.61.1, 1.62.1".into(),
            },
        );
        assert_eq!(
            cfg.needs_allow("playwright", "1.61.1"),
            NeedsVerdict::Allowed
        );
        assert_eq!(
            cfg.needs_allow("playwright", "1.62.1"),
            NeedsVerdict::Allowed
        );
        assert_ne!(
            cfg.needs_allow("playwright", "1.61.2"),
            NeedsVerdict::Allowed
        );
    }

    #[test]
    fn needs_mixed_form_is_a_validation_error_with_a_legible_message() {
        let mut cfg = full();
        cfg.needs.insert(
            "playwright".into(),
            ToolNeeds {
                versions: ">=1.55, 1.61.1".into(),
            },
        );
        let errs = cfg.validate();
        assert!(
            errs.iter()
                .any(|e| e.contains("needs.playwright") && e.contains("mixes")),
            "{errs:?}"
        );
    }

    #[test]
    fn needs_allow_fails_closed_on_a_mixed_versions_string() {
        // Reachable only via a hand-edited file that bypassed `validate()` —
        // `needs_allow` must still never allow anything against it, the same
        // way a malformed `versions` can only ever narrow, never widen.
        let mut cfg = FileConfig::default();
        cfg.needs.insert(
            "playwright".into(),
            ToolNeeds {
                versions: ">=1.55, 1.61.1".into(),
            },
        );
        assert_ne!(
            cfg.needs_allow("playwright", "1.61.1"),
            NeedsVerdict::Allowed
        );
        assert_ne!(
            cfg.needs_allow("playwright", "1.55.0"),
            NeedsVerdict::Allowed
        );
    }

    #[test]
    fn needs_allow_fails_closed_for_an_unconfigured_tool() {
        let cfg = FileConfig::default();
        assert_eq!(
            cfg.needs_allow("playwright", "1.61.1"),
            NeedsVerdict::UnknownTool
        );
    }

    #[test]
    fn needs_allow_compares_versions_numerically_not_lexically() {
        let mut cfg = FileConfig::default();
        cfg.needs.insert(
            "playwright".into(),
            ToolNeeds {
                versions: "<1.10.0".into(),
            },
        );
        // A naive string compare gets this backwards: "1.9.0" > "1.10.0"
        // lexically, because '9' > '1'.
        assert_eq!(
            cfg.needs_allow("playwright", "1.9.0"),
            NeedsVerdict::Allowed
        );
        assert_ne!(
            cfg.needs_allow("playwright", "1.10.0"),
            NeedsVerdict::Allowed
        );
        assert_ne!(
            cfg.needs_allow("playwright", "1.11.0"),
            NeedsVerdict::Allowed
        );
    }

    #[test]
    fn needs_allow_tolerates_a_missing_patch_on_either_side() {
        // Configured without a patch component...
        let mut cfg = FileConfig::default();
        cfg.needs.insert(
            "playwright".into(),
            ToolNeeds {
                versions: "1.62".into(),
            },
        );
        assert_eq!(
            cfg.needs_allow("playwright", "1.62.0"),
            NeedsVerdict::Allowed
        );

        // ...and requested without one.
        cfg.needs.insert(
            "playwright".into(),
            ToolNeeds {
                versions: "1.62.0".into(),
            },
        );
        assert_eq!(cfg.needs_allow("playwright", "1.62"), NeedsVerdict::Allowed);
    }

    #[test]
    fn needs_allow_rejects_an_unparseable_requested_version() {
        let mut cfg = FileConfig::default();
        cfg.needs.insert(
            "playwright".into(),
            ToolNeeds {
                versions: ">=1.55, <1.70".into(),
            },
        );
        assert_ne!(
            cfg.needs_allow("playwright", "not-a-version"),
            NeedsVerdict::Allowed
        );
    }

    #[test]
    fn needs_allow_with_empty_versions_permits_nothing() {
        let mut cfg = full();
        cfg.needs.insert(
            "playwright".into(),
            ToolNeeds {
                versions: String::new(),
            },
        );
        assert_ne!(
            cfg.needs_allow("playwright", "1.61.1"),
            NeedsVerdict::Allowed
        );
        // Not itself a config error: an operator can use an empty string as a
        // deliberate "block this tool entirely" switch without deleting the
        // section — see `ToolNeeds::versions`.
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
    }

    #[test]
    fn needs_section_round_trips_through_to_toml() {
        // Same property `written_toml_reads_back_identically` proves for the
        // rest of `FileConfig`: what faber writes, faber (or an operator) reads
        // back the same. Also pins that `needs` sits before `[[profile]]` in
        // field order — a table written after an array of tables lands inside
        // its last element instead of at the top level.
        let mut cfg = full();
        cfg.needs.insert(
            "playwright".into(),
            ToolNeeds {
                versions: ">=1.55, <1.70".into(),
            },
        );
        let text = cfg.to_toml().unwrap();
        let reread: FileConfig = toml::from_str(&text)
            .unwrap_or_else(|e| panic!("re-parsing our own output failed: {e}\n---\n{text}"));
        assert_eq!(
            reread.needs.get("playwright").map(|n| n.versions.as_str()),
            Some(">=1.55, <1.70")
        );
        assert_eq!(reread.into_profiles("2.330.0").unwrap().len(), 2);
    }

    // --- hard_ceiling: "auto" | "off" | N (ADR-0019) ------------------------

    #[test]
    fn hard_ceiling_parses_auto_off_number_and_absent() {
        let auto: AutoLimitFile = toml::from_str(r#"hard_ceiling = "auto""#).unwrap();
        assert_eq!(auto.hard_ceiling, HardCeiling::Auto);

        let off: AutoLimitFile = toml::from_str(r#"hard_ceiling = "off""#).unwrap();
        assert_eq!(off.hard_ceiling, HardCeiling::Off);

        let fixed: AutoLimitFile = toml::from_str("hard_ceiling = 4").unwrap();
        assert_eq!(fixed.hard_ceiling, HardCeiling::Fixed(4));

        // Absent means the same thing it always has: no ceiling.
        let absent: AutoLimitFile = toml::from_str("").unwrap();
        assert_eq!(absent.hard_ceiling, HardCeiling::Off);
    }

    #[test]
    fn hard_ceiling_rejects_an_unrecognized_word() {
        assert!(toml::from_str::<AutoLimitFile>(r#"hard_ceiling = "bogus""#).is_err());
    }

    #[test]
    fn hard_ceiling_of_zero_is_rejected_by_validate() {
        let mut cfg = full();
        cfg.autolimit.hard_ceiling = HardCeiling::Fixed(0);
        assert!(!cfg.validate().is_empty());
    }

    #[test]
    fn hard_ceiling_json_null_deserializes_to_off() {
        // The settings form's blank input, and the menu-bar app's "clear the
        // ceiling" POST — both send a literal JSON `null`.
        assert_eq!(
            serde_json::from_str::<HardCeiling>("null").unwrap(),
            HardCeiling::Off
        );
    }

    #[test]
    fn hard_ceiling_serializes_each_variant() {
        assert_eq!(
            serde_json::to_value(HardCeiling::Auto).unwrap(),
            serde_json::json!("auto")
        );
        assert_eq!(
            serde_json::to_value(HardCeiling::Off).unwrap(),
            serde_json::json!("off")
        );
        assert_eq!(
            serde_json::to_value(HardCeiling::Fixed(3)).unwrap(),
            serde_json::json!(3)
        );
    }

    #[test]
    fn hard_ceiling_off_is_omitted_from_written_toml() {
        // A file that never set a ceiling must keep saying nothing about one —
        // checked as the `key =` form, since the file's own header comment
        // names `hard_ceiling` in prose (documenting which knobs are live).
        let mut cfg = full();
        cfg.autolimit.hard_ceiling = HardCeiling::Off;
        let text = cfg.to_toml().unwrap();
        assert!(!text.contains("hard_ceiling ="), "{text}");
    }

    #[test]
    fn hard_ceiling_limit_resolves_auto_to_the_constant() {
        assert_eq!(HardCeiling::Auto.limit(), Some(AUTO_HARD_CEILING));
        assert_eq!(HardCeiling::Off.limit(), None);
        assert_eq!(HardCeiling::Fixed(7).limit(), Some(7));
    }

    #[test]
    fn overrides_hard_ceiling_setting_reads_the_legacy_numeric_mirror() {
        // An older binary wrote only the numeric `hard_ceiling` mirror; this
        // binary must still recover a setting from it after a rollback.
        let legacy: Overrides = serde_json::from_str(r#"{"hard_ceiling": 4}"#).unwrap();
        assert_eq!(legacy.hard_ceiling_setting(), Some(HardCeiling::Fixed(4)));
    }

    #[test]
    fn overrides_hard_ceiling_setting_prefers_ceiling_over_the_legacy_mirror() {
        let both: Overrides =
            serde_json::from_str(r#"{"ceiling": "auto", "hard_ceiling": 4}"#).unwrap();
        assert_eq!(both.hard_ceiling_setting(), Some(HardCeiling::Auto));

        let neither: Overrides = serde_json::from_str("{}").unwrap();
        assert_eq!(neither.hard_ceiling_setting(), None);
    }

    // --- the hold (issue #66) ------------------------------------------------

    /// When the zero-ceiling fixtures below were "written".
    const WRITTEN: u64 = 1_790_000_000_000;

    fn migrated(json: &str) -> (Overrides, bool) {
        let mut ov: Overrides = serde_json::from_str(json).unwrap();
        let changed = ov.migrate_zero_ceiling(WRITTEN);
        (ov, changed)
    }

    #[test]
    fn a_legacy_zero_ceiling_becomes_a_hold_dated_by_the_file() {
        // A legacy file: an older binary wrote only the numeric key.
        let (ov, changed) = migrated(r#"{"hard_ceiling": 0, "reserve_mib": 4096}"#);
        assert!(changed);
        assert_eq!(
            ov.hold,
            Some(Hold {
                reason: ZERO_CEILING_HOLD_REASON.to_string(),
                since_ms: WRITTEN,
            })
        );
        // Cleared, so the config file's ceiling applies rather than a zero.
        assert_eq!((ov.hard_ceiling, ov.ceiling), (None, None));
        assert_eq!(ov.hard_ceiling_setting(), None);
        assert_eq!(ov.reserve_mib, Some(4096), "the other knobs are untouched");
    }

    #[test]
    fn a_zero_ceiling_setting_becomes_a_hold_too() {
        // What this binary's API wrote for `{"hard_ceiling": 0}` before it refused
        // one: the setting and its mirror.
        for json in [r#"{"ceiling": 0}"#, r#"{"ceiling": 0, "hard_ceiling": 0}"#] {
            let (ov, changed) = migrated(json);
            assert!(changed, "{json}");
            assert_eq!(
                ov.hold.as_ref().map(|h| h.since_ms),
                Some(WRITTEN),
                "{json}"
            );
            assert_eq!(ov.hard_ceiling_setting(), None, "{json}");
        }
    }

    #[test]
    fn a_zero_ceiling_keeps_a_hold_that_is_already_recorded() {
        // A settings save while held writes the hold and a bare zero mirror; the
        // hold's own reason and date are the ones that count.
        let (ov, changed) =
            migrated(r#"{"hard_ceiling": 0, "hold": {"reason": "VM resize", "since_ms": 1000}}"#);
        assert!(changed);
        assert_eq!(
            ov.hold,
            Some(Hold {
                reason: "VM resize".to_string(),
                since_ms: 1000,
            })
        );
        assert_eq!(ov.hard_ceiling_setting(), None);
    }

    #[test]
    fn a_nonzero_ceiling_and_our_own_held_writes_are_left_alone() {
        for json in [
            "{}",
            r#"{"hard_ceiling": 4}"#,
            r#"{"ceiling": "auto", "hard_ceiling": 4}"#,
            r#"{"ceiling": "off"}"#,
            // Held, as this binary persists it: the true setting in `ceiling`,
            // the zero only in the mirror an older binary reads.
            r#"{"ceiling": "auto", "hard_ceiling": 0, "hold": {"reason": "r", "since_ms": 5}}"#,
        ] {
            let before: Overrides = serde_json::from_str(json).unwrap();
            let (after, changed) = migrated(json);
            assert!(!changed, "{json}");
            assert_eq!(after.hold, before.hold, "{json}");
            assert_eq!(
                after.hard_ceiling_setting(),
                before.hard_ceiling_setting(),
                "{json}"
            );
            assert_eq!(after.hard_ceiling, before.hard_ceiling, "{json}");
        }
    }

    #[test]
    fn the_hold_key_is_optional_and_omitted_when_there_is_none() {
        // A file from before holds reads as not held, and a file with no hold
        // says nothing about one — the shape older binaries have always written.
        let ov: Overrides = serde_json::from_str(r#"{"margin": 1.5}"#).unwrap();
        assert_eq!(ov.hold, None);
        let text = serde_json::to_string(&ov).unwrap();
        assert!(!text.contains("hold"), "{text}");

        let held = Overrides {
            hold: Some(Hold {
                reason: "resize".to_string(),
                since_ms: 7,
            }),
            ..Overrides::default()
        };
        let json = serde_json::to_value(&held).unwrap();
        assert_eq!(
            json["hold"],
            serde_json::json!({"reason": "resize", "since_ms": 7})
        );
    }

    #[test]
    fn a_hold_is_dated_in_rfc3339_utc() {
        let at = |since_ms| {
            Hold {
                reason: String::new(),
                since_ms,
            }
            .since_utc()
        };
        assert_eq!(at(0), "1970-01-01T00:00:00Z");
        assert_eq!(at(951_782_400_000), "2000-02-29T00:00:00Z");
        assert_eq!(at(1_704_067_199_999), "2023-12-31T23:59:59Z");
        // The inverse of the parser the retry sweep reads GitHub's stamps with,
        // across leap days, month ends and the century rule.
        for ms in [
            86_399_000,
            68_169_600_000,
            951_868_799_000,
            4_107_542_400_000,
            WRITTEN,
        ] {
            let text = at(ms);
            assert_eq!(
                crate::retry::parse_rfc3339_to_ms(&text),
                Some(ms - ms % 1000),
                "{text}"
            );
        }
    }

    // --- autolimit.reserve (ADR-0033 §2) -------------------------------------

    fn with_autolimit(autolimit: &str) -> FileConfig {
        toml::from_str(&format!(
            r#"
            [autolimit]
            {autolimit}

            [[profile]]
            name = "linux-docker"
            labels = ["faber", "docker"]
            backend = "docker"
            image = "ghcr.io/owner/my-runner:latest"
            dind = false
        "#
        ))
        .unwrap()
    }

    #[test]
    fn priority_repos_absent_is_empty_present_parses_and_repo_config_rejects_it() {
        let absent = with_autolimit("");
        assert!(absent.autolimit.priority_repos.is_empty());
        assert!(absent.validate().is_empty(), "{:?}", absent.validate());
        // A file that never set it keeps saying nothing about it.
        assert!(!absent.to_toml().unwrap().contains("priority_repos"));
        let set = with_autolimit(r#"priority_repos = ["acme/tooling", "Acme/App"]"#);
        assert_eq!(set.autolimit.priority_repos, ["acme/tooling", "Acme/App"]);
        assert!(set.validate().is_empty(), "{:?}", set.validate());
        assert!(set.to_toml().unwrap().contains("priority_repos"));
        assert_eq!(
            set.autolimit
                .resolve(&Overrides::default())
                .priority_repos
                .len(),
            2
        );
        let bad = with_autolimit(r#"priority_repos = ["tooling"]"#);
        assert!(bad.validate().iter().any(|e| e.contains("priority_repos")));
        // A repo must never be able to raise its own priority: the in-repo file
        // refuses the key, both bare and under [autolimit].
        assert!(toml::from_str::<RepoConfig>(r#"priority_repos = ["acme/app"]"#).is_err());
        assert!(
            toml::from_str::<RepoConfig>("[autolimit]\npriority_repos = [\"acme/app\"]").is_err()
        );
    }

    #[test]
    fn reserve_absent_is_max_and_stays_unwritten() {
        use crate::reserve::ReserveMode;
        let absent = with_autolimit("");
        assert_eq!(absent.autolimit.reserve, ReserveSetting::Unstated);
        assert_eq!(absent.autolimit.reserve.mode(), ReserveMode::Max);
        assert!(absent.validate().is_empty(), "{:?}", absent.validate());
        // A file that never set it keeps saying nothing about it.
        assert!(!absent.to_toml().unwrap().contains("reserve ="));
        let max = with_autolimit(r#"reserve = "max""#);
        assert_eq!(max.autolimit.reserve.mode(), ReserveMode::Max);
        let p75 = with_autolimit(r#"reserve = "p75""#);
        assert_eq!(p75.autolimit.reserve.mode(), ReserveMode::P75);
        assert!(p75.to_toml().unwrap().contains(r#"reserve = "p75""#));
        assert_eq!(
            p75.autolimit.resolve(&Overrides::default()).reserve,
            ReserveSetting::P75
        );
    }

    #[test]
    fn an_unknown_reserve_parses_runs_as_max_and_is_rejected_where_a_human_looks() {
        use crate::reserve::ReserveMode;
        // Parsing must not fail: a daemon booting on it warns and runs as "max".
        let odd = with_autolimit(r#"reserve = "p50""#);
        assert_eq!(odd.autolimit.reserve.unknown(), Some("p50"));
        assert_eq!(odd.autolimit.reserve.mode(), ReserveMode::Max);
        // `faber check-config` and the settings save reject it.
        let errs = odd.validate();
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("autolimit.reserve"), "{errs:?}");
        // From the settings form's JSON too.
        let json: AutoLimitFile = serde_json::from_str(r#"{"reserve": "p90"}"#).unwrap();
        assert_eq!(json.reserve.unknown(), Some("p90"));
        let null: AutoLimitFile = serde_json::from_str(r#"{"reserve": null}"#).unwrap();
        assert!(null.reserve.is_unstated());
    }

    // --- declared services (ADR-0046) ---------------------------------------

    /// One docker profile and `services`, the `[services.*]` tables, appended.
    fn with_services(services: &str) -> FileConfig {
        docker_profile_with(&format!("\n{services}"))
    }

    const SERVICES: &str = r#"
        [services.postgres]
        versions = "16, 17"
        image = "postgres:{version}-alpine"
        extensions = ["postgis", "pg_trgm"]
        memory_mib = 2048

        [services.redis]
        image = "redis:7-alpine"
        memory_mib = 256

        [services.mailpit]
        image = "axllent/mailpit:v1.21"
        memory_mib = 128
    "#;

    #[test]
    fn services_resolve_into_the_policy_with_the_idle_default() {
        let cfg = with_services(SERVICES);
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
        let p = cfg.services_policy();
        let pg = p.postgres.expect("postgres");
        assert_eq!(pg.versions, ["16", "17"]);
        assert_eq!(pg.image, "postgres:{version}-alpine");
        assert_eq!(pg.extensions, ["postgis", "pg_trgm"]);
        assert_eq!(pg.memory_mib, 2048);
        assert_eq!(pg.idle_stop, Duration::from_secs(SERVICE_IDLE_STOP_SECS));
        let redis = &p.per_job["redis"];
        assert_eq!(
            (redis.image.as_str(), redis.memory_mib),
            ("redis:7-alpine", 256)
        );
        // A table that states no `job_env` gets the built-in variables (A1).
        assert_eq!(redis.job_env["REDIS_URL"], "redis://redis:6379/0");
        assert!(redis.args.is_empty() && redis.env.is_empty() && redis.health_cmd.is_none());
        let mail = &p.per_job["mailpit"];
        assert_eq!(mail.memory_mib, 128);
        assert_eq!(mail.job_env["MAIL_SMTP_URL"], "smtp://mailpit:1025");
        assert_eq!(mail.job_env["MAIL_API_URL"], "http://mailpit:8025");

        let set = with_services(
            "[services.postgres]\nversions = \"16\"\nimage = \"postgres:{version}\"\n\
             memory_mib = 1024\nidle_stop_secs = 60",
        );
        let pg = set.services_policy().postgres.unwrap();
        assert_eq!(pg.idle_stop, Duration::from_secs(60));
        assert!(set.services_policy().per_job.is_empty());
    }

    const S3: &str = r#"
        [services.s3]
        image = "versity/versitygw:v1.8.0"
        args = ["--port", ":7070", "posix", "/data"]
        env = { ROOT_ACCESS_KEY = "ci", ROOT_SECRET_KEY = "ci-secret" }
        job_env = { S3_ENDPOINT = "http://s3:7070", AWS_REGION = "us-east-1" }
        health_cmd = "wget -qO- http://127.0.0.1:7070/health"
        memory_mib = 256
    "#;

    /// ADR-0046 A1: any name but `postgres` is a per-job service, described by
    /// its own table; `redis` and `mailpit` are the same kind with defaults.
    #[test]
    fn a_generic_table_resolves_into_a_per_job_service() {
        let cfg = with_services(&format!("{SERVICES}{S3}"));
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
        let p = cfg.services_policy();
        assert_eq!(p.per_job.len(), 3);
        let s3 = &p.per_job["s3"];
        assert_eq!(s3.image, "versity/versitygw:v1.8.0");
        assert_eq!(s3.args, ["--port", ":7070", "posix", "/data"]);
        assert_eq!(s3.env["ROOT_SECRET_KEY"], "ci-secret");
        assert_eq!(s3.job_env["S3_ENDPOINT"], "http://s3:7070");
        assert_eq!(
            s3.health_cmd.as_deref(),
            Some("wget -qO- http://127.0.0.1:7070/health")
        );
        assert_eq!(s3.memory_mib, 256);
        assert!(p.is_service_tool("s3"));
        // An explicit `job_env` replaces the built-in default.
        let own = with_services(
            "[services.redis]\nimage = \"redis:7\"\nmemory_mib = 1\n\
             job_env = { CACHE_URL = \"redis://redis:6379/1\" }",
        );
        let redis = &own.services_policy().per_job["redis"];
        assert_eq!(redis.job_env.len(), 1);
        assert_eq!(redis.job_env["CACHE_URL"], "redis://redis:6379/1");
        // The tables round-trip through a save, and an absent key stays absent.
        let back: FileConfig = toml::from_str(&cfg.to_toml().unwrap()).unwrap();
        assert_eq!(back.services, cfg.services);
        let text = with_services(SERVICES).to_toml().unwrap();
        assert!(
            !text.contains("job_env") && !text.contains("health_cmd"),
            "{text}"
        );
    }

    /// ADR-0062: `{secret}` is accepted in `env` and `job_env` and kept as
    /// written, since faber fills it per job, not when it reads the config.
    #[test]
    fn a_secret_placeholder_is_kept_in_the_policy_for_the_job_to_fill() {
        let cfg = with_services(
            r#"
            [services.s3]
            image = "versity/versitygw:v1.10.0"
            args = ["--port", ":7070", "--health", "/health", "posix", "/tmp/vgw"]
            env = { ROOT_ACCESS_KEY = "faber", ROOT_SECRET_KEY = "{secret}" }
            health_cmd = "wget -qO- http://127.0.0.1:7070/health"
            memory_mib = 256
            [services.s3.job_env]
            S3_ENDPOINT = "http://s3:7070"
            AWS_ACCESS_KEY_ID = "faber"
            AWS_SECRET_ACCESS_KEY = "{secret}"
            AWS_REGION = "us-east-1"
            "#,
        );
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
        let s3 = &cfg.services_policy().per_job["s3"];
        assert_eq!(s3.env["ROOT_SECRET_KEY"], SERVICE_SECRET_PLACEHOLDER);
        assert_eq!(
            s3.job_env["AWS_SECRET_ACCESS_KEY"],
            SERVICE_SECRET_PLACEHOLDER
        );
        assert!(s3.wants_secret());
    }

    /// One case per rule of A1 and A2: refused where a human looks, left out of
    /// the policy, named in the boot warning, and never echoing an `env` value.
    #[test]
    fn a_bad_generic_table_is_refused_and_left_out_of_the_policy() {
        let t = |name: &str, extra: &str| {
            format!("[services.{name}]\nimage = \"ghcr.io/owner/demo:1\"\nmemory_mib = 1\n{extra}")
        };
        let cases: Vec<(String, &str)> = vec![
            (t("S3", ""), "host name"),
            (t("3s", ""), "host name"),
            (t("my-s3", ""), "host name"),
            (t(&"a".repeat(33), ""), "host name"),
            (t("dind", ""), "already puts on a job's network"),
            (t("s3", "versions = \"1\""), "versions does not apply"),
            (t("s3", "extensions = [\"x\"]"), "extensions belong"),
            (t("s3", "idle_stop_secs = 5"), "idle_stop_secs belongs"),
            (t("s3", "args = [\"a\\u0000b\"]"), "NUL"),
            (
                t("s3", "args = [\"--key\", \"{secret}\"]"),
                "cannot carry {secret}",
            ),
            (
                t("s3", "health_cmd = \"curl -u faber:{secret} http://s3\""),
                "cannot carry {secret}",
            ),
            (t("s3", "env = { lower = \"x\" }"), "env name"),
            (t("s3", "env = { \"A-B\" = \"x\" }"), "env name"),
            (t("s3", "env = { A = \"x\\ny\" }"), "env A has a value"),
            (t("s3", "env = { A = \"x\\u0000\" }"), "env A has a value"),
            (
                t("s3", "job_env = { A = \"x\\ry\" }"),
                "job_env A has a value",
            ),
            (t("s3", "job_env = { \"1A\" = \"x\" }"), "job_env name"),
            (
                t("s3", "job_env = { DATABASE_URL = \"x\" }"),
                "set by faber itself",
            ),
            (
                t("s3", "job_env = { PGHOST = \"x\" }"),
                "set by faber itself",
            ),
            (
                t("s3", "job_env = { PGPORT = \"x\" }"),
                "set by faber itself",
            ),
            (
                t("s3", "job_env = { PGUSER = \"x\" }"),
                "set by faber itself",
            ),
            (
                t("s3", "job_env = { PGPASSWORD = \"x\" }"),
                "set by faber itself",
            ),
            (
                t("s3", "job_env = { PGDATABASE = \"x\" }"),
                "set by faber itself",
            ),
            (
                t("s3", "job_env = { DOCKER_HOST = \"x\" }"),
                "read by the docker CLI",
            ),
            (
                t("s3", "env = { DOCKER_HOST = \"x\" }"),
                "read by the docker CLI",
            ),
            (
                t("s3", "env = { DOCKER_CONFIG = \"x\" }"),
                "read by the docker CLI",
            ),
            (
                t("s3", "job_env = { DOCKER_CONTEXT = \"x\" }"),
                "read by the docker CLI",
            ),
            (t("s3", "env = { PATH = \"x\" }"), "read by the docker CLI"),
            (t("s3", "env = { HOME = \"x\" }"), "read by the docker CLI"),
            (
                t("s3", "job_env = { TMPDIR = \"x\" }"),
                "read by the docker CLI",
            ),
            (t("s3", "env = { LANG = \"x\" }"), "read by the docker CLI"),
            (
                t("s3", "env = { BUILDKIT_X = \"x\" }"),
                "read by the docker CLI",
            ),
            (
                t("s3", "job_env = { DOCKER_ANYTHING = \"x\" }"),
                "read by the docker CLI",
            ),
            (
                t("s3", "job_env = { RUNNER_TEMP = \"x\" }"),
                "set by faber itself",
            ),
            (
                t("s3", "job_env = { GITHUB_TOKEN = \"x\" }"),
                "set by faber itself",
            ),
            (
                t("s3", "job_env = { ACTIONS_X = \"x\" }"),
                "set by faber itself",
            ),
            (
                t("s3", "job_env = { FABER_X = \"x\" }"),
                "set by faber itself",
            ),
            (t("s3", "health_cmd = \" \""), "health_cmd"),
            (t("s3", "health_cmd = \"a\\u0000\""), "health_cmd"),
            // Postgres refuses the per-job keys.
            (
                "[services.postgres]\nversions = \"16\"\nimage = \"postgres:{version}\"\n\
                 memory_mib = 1\nargs = [\"-c\"]"
                    .into(),
                "args does not apply",
            ),
            (
                "[services.postgres]\nversions = \"16\"\nimage = \"postgres:{version}\"\n\
                 memory_mib = 1\nenv = { A = \"b\" }"
                    .into(),
                "env does not apply",
            ),
            (
                "[services.postgres]\nversions = \"16\"\nimage = \"postgres:{version}\"\n\
                 memory_mib = 1\njob_env = { A = \"b\" }"
                    .into(),
                "job_env does not apply",
            ),
            (
                "[services.postgres]\nversions = \"16\"\nimage = \"postgres:{version}\"\n\
                 memory_mib = 1\nhealth_cmd = \"true\""
                    .into(),
                "health_cmd does not apply",
            ),
        ];
        for (table, want) in &cases {
            let cfg = with_services(table);
            let errs = cfg.validate_structural();
            assert!(
                errs.iter()
                    .any(|e| e.starts_with("services.") && e.contains(want)),
                "{table}: want {want:?} in {errs:?}"
            );
            assert!(cfg.services_policy().is_empty(), "{table}");
            let warning = services_ignored_warning(&cfg).expect("boot warns");
            assert!(warning.contains(want), "{warning}");
        }
        // The message names the key, never the value (a value may be a credential).
        let cfg = with_services(&t("s3", "env = { A = \"hunter2\\nx\" }"));
        let all = cfg.validate_structural().join("\n");
        assert!(all.contains("env A") && !all.contains("hunter2"), "{all}");
        // The names that are fine.
        for ok in ["s3", "a", "gateway2", &"z".repeat(32)] {
            let cfg = with_services(&t(
                ok,
                "env = { _A1 = \"v\" }\njob_env = { MY_URL = \"u\" }",
            ));
            assert!(cfg.validate_structural().is_empty(), "{ok}");
            assert!(cfg.services_policy().per_job.contains_key(ok), "{ok}");
        }
    }

    /// A2: two tables exporting the same name would give a job declaring both
    /// whichever was written last. The built-in defaults count.
    #[test]
    fn two_tables_may_not_export_the_same_job_env_name() {
        let t = |name: &str, job_env: &str| {
            format!(
                "[services.{name}]\nimage = \"ghcr.io/owner/demo:1\"\nmemory_mib = 1\n{job_env}\n"
            )
        };
        let both = with_services(&format!(
            "{}{}",
            t("s3", "job_env = { SHARED = \"1\", S3_URL = \"u\" }"),
            t("blob", "job_env = { SHARED = \"2\" }")
        ));
        let errs = both.validate_structural();
        assert_eq!(errs.len(), 2, "{errs:?}");
        assert!(errs.iter().any(|e| e.starts_with("services.s3:")
            && e.contains("SHARED")
            && e.contains("services.blob")));
        assert!(errs
            .iter()
            .any(|e| e.starts_with("services.blob:") && e.contains("services.s3")));
        assert!(both.services_policy().is_empty(), "both are left out");
        // A built-in default is an export: a table without `job_env` exports it.
        let clash = with_services(&format!(
            "{}{}",
            t("redis", ""),
            t("cache", "job_env = { REDIS_URL = \"redis://cache:6379\" }")
        ));
        let errs = clash.validate_structural();
        assert!(errs.iter().any(|e| e.contains("REDIS_URL")), "{errs:?}");
        // …unless the table states its own `job_env`, which replaces the default.
        let fine = with_services(&format!(
            "{}{}",
            t("redis", "job_env = { CACHE_URL = \"x\" }"),
            t("cache", "job_env = { REDIS_URL = \"redis://cache:6379\" }")
        ));
        assert!(fine.validate_structural().is_empty());
        assert_eq!(fine.services_policy().per_job.len(), 2);
        // An invalid table exports nothing, so it does not take a good one down.
        let mixed = with_services(&format!(
            "{}{}",
            t("s3", "job_env = { SHARED = \"1\" }"),
            t("Blob", "job_env = { SHARED = \"2\" }")
        ));
        assert!(mixed.services_policy().per_job.contains_key("s3"));
    }

    #[test]
    fn no_services_table_is_an_empty_policy_and_is_never_written() {
        let cfg = docker_profile_with("");
        assert!(cfg.services_policy().is_empty());
        assert!(!cfg.to_toml().unwrap().contains("[services"));
        // And the tables round-trip through a save.
        let cfg = with_services(SERVICES);
        let back: FileConfig = toml::from_str(&cfg.to_toml().unwrap()).unwrap();
        assert_eq!(back.services, cfg.services);
    }

    #[test]
    fn postgres_versions_are_exact_majors_from_13() {
        assert_eq!(parse_service_versions("16, 17").unwrap(), ["16", "17"]);
        assert_eq!(parse_service_versions(" 13 ").unwrap(), ["13"]);
        for bad in [
            "", " ", "12", "16.2", "16,", "016", "v16", "16 17", "16, 16", ">=16", "-16",
        ] {
            assert!(parse_service_versions(bad).is_err(), "{bad:?}");
        }
    }

    /// The structural bar, one case per rule: each is refused by `check-config
    /// --for-update` as well as by a save, left out of the policy, and named in
    /// the boot warning.
    #[test]
    fn a_bad_services_table_is_refused_and_left_out_of_the_policy() {
        let pg = |extra: &str| {
            format!(
                "[services.postgres]\nversions = \"16\"\nimage = \"postgres:{{version}}\"\n{extra}"
            )
        };
        let cases: Vec<(String, &str)> = vec![
            (
                "[services.postgres]\nimage = \"postgres:16\"\nmemory_mib = 1".into(),
                "versions is required",
            ),
            (
                "[services.postgres]\nversions = \"12\"\nimage = \"postgres:{version}\"\n\
                 memory_mib = 1"
                    .into(),
                "below 13",
            ),
            (
                "[services.postgres]\nversions = \"16\"\nimage = \"postgres:16\"\nmemory_mib = 1"
                    .into(),
                "must contain {version}",
            ),
            (
                "[services.postgres]\nversions = \"16\"\nimage = \"postgres:{version} -v\"\n\
                 memory_mib = 1"
                    .into(),
                "not an image reference",
            ),
            (
                pg("memory_mib = 1\nextensions = [\"PostGIS\"]"),
                "extensions entry",
            ),
            (
                pg("memory_mib = 1\nextensions = [\"x; DROP\"]"),
                "extensions entry",
            ),
            (pg("memory_mib = 0"), "memory_mib"),
            (pg("memory_mib = 1\nidle_stop_secs = 0"), "idle_stop_secs"),
            (
                "[services.redis]\nversions = \"7\"\nimage = \"redis:7\"\nmemory_mib = 1".into(),
                "versions does not apply",
            ),
            (
                "[services.redis]\nimage = \"redis:{version}\"\nmemory_mib = 1".into(),
                "contains {version}",
            ),
            (
                "[services.mailpit]\nimage = \"axllent/mailpit\"\nmemory_mib = 1\n\
                 extensions = [\"x\"]"
                    .into(),
                "extensions belong",
            ),
            (
                "[services.redis]\nimage = \"redis:7\"\nmemory_mib = 1\nidle_stop_secs = 60".into(),
                "idle_stop_secs belongs",
            ),
            (
                "[services.redis]\nimage = \"\"\nmemory_mib = 1".into(),
                "not an image reference",
            ),
            (
                "[services.redis]\nimage = \"redis:7\"\nmemory_mib = 0".into(),
                "memory_mib",
            ),
        ];
        for (table, want) in &cases {
            let cfg = with_services(table);
            let errs = cfg.validate_structural();
            assert!(
                errs.iter()
                    .any(|e| e.starts_with("services.") && e.contains(want)),
                "{table}: want {want:?} in {errs:?}"
            );
            assert!(cfg.services_policy().is_empty(), "{table}");
            let warning = services_ignored_warning(&cfg).expect("boot warns");
            assert!(warning.contains(want), "{warning}");
        }
        // A good table beside a bad one still resolves.
        let mixed = with_services(
            "[services.redis]\nimage = \"redis:7\"\nmemory_mib = 64\n\
             [services.mailpit]\nimage = \"axllent/mailpit\"\nmemory_mib = 0",
        );
        let p = mixed.services_policy();
        assert!(p.per_job.contains_key("redis") && !p.per_job.contains_key("mailpit"));
        assert!(services_ignored_warning(&with_services(SERVICES)).is_none());
    }

    #[test]
    fn unknown_keys_in_a_services_table_do_not_parse() {
        let text = "[services.redis]\nimage = \"redis:7\"\nmemory_mib = 1\nport = 6380";
        assert!(toml::from_str::<FileConfig>(text).is_err());
    }

    /// `postgres`, `redis` and `mailpit`, and every configured `[services]`
    /// name, are services, never `[needs]` tools: refused where a human is
    /// looking, but never by the updater's gate.
    #[test]
    fn service_names_are_reserved_in_needs() {
        let mut cfg = docker_profile_with(S3);
        let mut names: Vec<&str> = SERVICE_NAMES.to_vec();
        names.push("s3");
        // A tool no table names stays a tool.
        cfg.needs.insert(
            "playwright".to_string(),
            ToolNeeds {
                versions: "1.61.1".into(),
            },
        );
        for name in &names {
            cfg.needs.insert(
                name.to_string(),
                ToolNeeds {
                    versions: "16.0".into(),
                },
            );
        }
        let errs = cfg.validate();
        for name in &names {
            assert!(
                errs.iter()
                    .any(|e| e.starts_with(&format!("needs.{name}:")) && e.contains("service")),
                "{name}: {errs:?}"
            );
        }
        assert!(
            !errs.iter().any(|e| e.starts_with("needs.playwright")),
            "{errs:?}"
        );
        assert!(
            cfg.validate_structural().is_empty(),
            "{:?}",
            cfg.validate_structural()
        );
    }

    #[test]
    fn an_unpullable_service_image_is_warned_about() {
        let host = |v: &str| format!("{SERVICES}\n[host]\npull_unqualified_images = {v}");
        let w = services_pull_warnings(&with_services(&host("false")));
        // postgres-16, postgres-17, redis and mailpit all name no registry host.
        assert_eq!(w.len(), 4, "{w:?}");
        assert!(
            w[0].contains("postgres-16") && w[0].contains("postgres:16-alpine"),
            "{}",
            w[0]
        );
        assert!(w.iter().any(|l| l.contains("service mailpit")), "{w:?}");
        // Unstated or true: pulled as before, nothing to say here.
        assert!(services_pull_warnings(&with_services(SERVICES)).is_empty());
        assert!(services_pull_warnings(&with_services(&host("true"))).is_empty());
        // A qualified image is pulled whatever the setting.
        let qualified = with_services(
            "[services.redis]\nimage = \"docker.io/library/redis:7\"\nmemory_mib = 1\n\
             [host]\npull_unqualified_images = false",
        );
        assert!(services_pull_warnings(&qualified).is_empty());
    }
}
