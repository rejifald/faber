//! Jobs fetch through a mirror only faber fills (ADR-0047;
//! docs/mirrors.md).
//!
//! A pull-through cache service for the things jobs download again and again
//! (LFS objects, npm packages, later more). faber runs **one** `faber-mirror`
//! container per host on the Docker engine the runners use, with a named cache
//! volume, and connects it to each opted-in job's private network under the
//! alias [`faber_mirror::ALIAS`]. The runner is handed [`ENV`] and, as
//! ecosystems are added, the variables that redirect each tool ([`ECOSYSTEMS`]).
//!
//! Two ecosystems ship:
//!
//! - **Git LFS** ([`LFS`], ADR-0047 §4 without warming), on when `[mirror.lfs]`
//!   is set. Each runner pointed at the mirror then gets its own token
//!   ([`RepoToken`]), minted at provision and revoked at teardown, and the
//!   `GIT_CONFIG_*` entries that send git-lfs's downloads to the mirror and its
//!   pushes to GitHub.
//! - **npm** ([`NPM`], ADR-0047 §5), on when `[mirror.npm]` is set: the
//!   default registry of npm, pnpm, bun and yarn, pointed at the mirror. It
//!   needs no token: everything it serves is public, and the URL it injects is
//!   the same for every job, so no secret lands in a lockfile or a job log. The
//!   service tells jobs apart, for the counters and for `cause: mirror`, by its
//!   own address on each job's network, which every sync sends ([`addrs_args`]).
//!
//! **How the token table reaches the service.** By `docker exec -i faber-mirror
//! faber-mirror sync`, the table on its stdin ([`Mirror::sync`]): no published
//! port, no argument, no file. The service hands it to its own control port,
//! bound to the container's loopback, which no job network can reach. Each sync
//! carries the whole table, so it is idempotent: the janitor sends it every
//! [`POLL`], which heals a service that restarted and forgot it, and its answer
//! carries the counters the dashboard shows and the mirror-caused failures that
//! become a job's `cause: mirror` (ADR-0047 §7).
//!
//! What keeps it safe and inert, in the order it matters:
//!
//! 1. **Absent `[mirror]` is off.** [`Mirror::configure`] is never called and
//!    no job is pointed at a mirror. The one Docker call left is a labelled
//!    listing at boot ([`retire_lingering`]): a mirror container a previous
//!    config left is disconnected as its jobs end and then removed. A host that
//!    never ran one makes no other call for it.
//! 2. **Best-effort, like ADR-0035's git mirror** (ADR-0047 §7). A job is
//!    pointed at the mirror only while its latest health reading succeeded and
//!    is recent ([`plan`]). A failed probe, a foreign container under the name
//!    or a replace stops that at once; a mirror that is missing, down or not yet
//!    started gets the job nothing, and it runs as before. A failed network
//!    connect does the same.
//! 3. **Read labels, never names** (AGENTS.md). The container is adopted across a
//!    restart (ADR-0011) only when it carries `org.faber.kind=mirror`; a
//!    container of that name faber did not make is left alone and reported.
//! 4. **Never swept.** [`crate::docker::KIND_MIRROR`] is not a per-job kind, so
//!    the janitor neither reaps the container nor its cache volume.
//! 5. **Nothing is published.** The image is built from the checkout into the
//!    runners' engine (`deploy/build-mirror-image.sh`) and run `--pull=never`.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::Serialize;
use tracing::{debug, info, warn};

use crate::audit::now_ms;
use crate::docker::{
    self, CredentialFill, CredentialVolume, CLI_TIMEOUT, KIND_MIRROR, KIND_MIRROR_AUTH, LABEL_KIND,
    LABEL_OWNED, LAUNCH_TIMEOUT,
};
use crate::untrusted;

/// The one mirror container. Used to create, inspect and connect it; whether
/// an existing one is faber's is read from its labels ([`decide`]).
pub const CONTAINER: &str = "faber-mirror";

/// The mirror's cache volume, mounted at [`faber_mirror::CACHE_DIR`].
pub const VOLUME: &str = "faber-mirror-cache";

/// The volume holding faber's GitHub credential for the LFS adapter, mounted
/// read-only at [`faber_mirror::CREDENTIAL_DIR`]. Filled the way the git
/// mirror's refresh credential is ([`CredentialVolume`]), and only while
/// `[mirror.lfs]` is on.
pub const AUTH_VOLUME: &str = "faber-mirror-auth";

/// The label carrying a fingerprint of the credential the container was
/// started with, so a changed token replaces it (read from the label, never
/// guessed): the first 16 hex digits of the token's sha256.
const LABEL_CREDENTIAL: &str = "org.faber.credential";

/// The `cause` a failed job's outcome record carries when the mirror failed
/// one of its requests (ADR-0047 §7).
pub const CAUSE: &str = "mirror";

/// How far above ADR-0024's admission floor the host's free disk must stay for
/// the mirror to keep storing (ADR-0047 §8).
pub const LOW_DISK_MARGIN_GIB: u64 = 5;

/// How many affected jobs the "Mirror down" row names.
const AFFECTED_CAP: usize = 50;

/// The variable an opted-in runner gets: the mirror's base URL. Set only when
/// the mirror is connected to the job's network, so it always names something
/// that answers.
pub const ENV: &str = "FABER_MIRROR";

/// How often the janitor looks at the container and reads its health.
pub const POLL: Duration = Duration::from_secs(15);

/// How recent the last healthy reading must be for a job to be pointed at the
/// mirror, and how long a mirror may go without one before it is reported down.
/// Four polls: one slow probe is not an outage.
pub const HEALTHY_WINDOW_MS: u64 = 60_000;

/// The mirror container's memory cap. The LFS adapter and npm's tarballs stream
/// through files, so their memory does not grow with object size; npm's
/// packuments are held in memory, bounded (64 MiB of cache, 16 MiB per
/// packument read whole).
const MEMORY: &str = "256m";

/// The most of a sync's answer faber reads.
const SYNC_CAP: usize = faber_mirror::SYNC_CAP;

/// The most of a health probe's output faber reads. The report is one short line.
const PROBE_CAP: usize = 16 * 1024;

/// How much of a reason or an error the dashboard is shown.
const TEXT_CHARS: usize = 300;

/// `[mirror]`, resolved ([`crate::config::FileConfig::mirror_policy`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MirrorPolicy {
    /// The cache volume's limit, in GiB. Above 0.
    pub max_size_gib: u32,
    /// `[mirror.lfs] fill`, or `None` with `[mirror.lfs]` absent: LFS is not
    /// redirected.
    pub lfs: Option<faber_mirror::Fill>,
    /// `[mirror.npm] fill`, or `None` with `[mirror.npm]` absent: the default
    /// npm registry is not redirected.
    pub npm: Option<faber_mirror::Fill>,
}

/// The image a faber of this version runs: `faber-mirror:<version>`, the tag
/// `deploy/build-mirror-image.sh` gives an image built from the same checkout.
pub fn image() -> String {
    format!("faber-mirror:{}", env!("CARGO_PKG_VERSION"))
}

/// The URL a job reaches the mirror at, on its own network.
pub fn base_url() -> String {
    format!("http://{}:{}", faber_mirror::ALIAS, faber_mirror::PORT)
}

// --- the injection hook (pure) ---------------------------------------------------

/// One variable an ecosystem hands the runner.
pub struct EnvVar {
    pub name: String,
    pub value: String,
    /// Whether the value carries a secret (ADR-0047 §4: the per-container token
    /// travels inside `lfs.url`). A secret goes the way the JIT config and the
    /// services' `DATABASE_URL` go: its name alone on the `docker run` argv, its
    /// value in that one CLI child's environment. Never an argument, a label or a
    /// log line.
    pub secret: bool,
}

/// What an ecosystem's redirect is built from, for one runner container.
pub struct EcosystemCtx<'a> {
    /// The mirror's base URL on the job's network ([`base_url`]).
    pub base_url: &'a str,
    /// The repo the runner serves. A JIT runner is minted for one repo, so this
    /// is the only repo its token may speak for (ADR-0047 §4).
    pub owner_repo: &'a str,
    /// The container's own token, when one was minted.
    pub token: Option<&'a RepoToken>,
    /// The profile's own `env`, which an ecosystem may have to extend rather
    /// than overwrite (`GIT_CONFIG_COUNT`).
    pub profile_env: &'a BTreeMap<String, String>,
}

/// One ecosystem's redirect: the variables that point its tool at the mirror.
/// Each adapter the service gains adds one entry to [`ECOSYSTEMS`]; nothing else
/// on the daemon side changes.
#[derive(Clone, Copy)]
pub struct Ecosystem {
    pub name: &'static str,
    /// Whether the config turns it on.
    pub on: fn(&MirrorPolicy) -> bool,
    /// Whether its redirect carries the container's own token ([`RepoToken`]),
    /// which must reach the service before the runner starts.
    pub needs_token: bool,
    /// Its variables, or why this runner cannot be redirected (it then runs as
    /// it would without the ecosystem).
    pub env: fn(&EcosystemCtx<'_>) -> Result<Vec<EnvVar>, String>,
}

/// The ecosystems the mirror serves.
pub const ECOSYSTEMS: &[Ecosystem] = &[LFS, NPM];

/// Git LFS (ADR-0047 §4): on with `[mirror.lfs]`.
pub const LFS: Ecosystem = Ecosystem {
    name: "lfs",
    on: lfs_on,
    needs_token: true,
    env: lfs_env,
};

fn lfs_on(p: &MirrorPolicy) -> bool {
    p.lfs.is_some()
}

/// npm (ADR-0047 §5): on with `[mirror.npm]`.
pub const NPM: Ecosystem = Ecosystem {
    name: "npm",
    on: npm_on,
    needs_token: false,
    env: npm_env,
};

fn npm_on(p: &MirrorPolicy) -> bool {
    p.npm.is_some()
}

/// The variables that point npm, pnpm and bun (`NPM_CONFIG_REGISTRY`) and
/// yarn berry (`YARN_NPM_REGISTRY_SERVER`) at the mirror. yarn berry also
/// refuses plain-http registries unless their host is allowed
/// (`unsafeHttpWhitelist`), so the mirror's alias is allowed for it.
pub const NPM_REGISTRY_VARS: &[&str] = &["NPM_CONFIG_REGISTRY", "YARN_NPM_REGISTRY_SERVER"];

/// yarn berry's allowance for a plain-http registry host.
pub const YARN_HTTP_ALLOW: &str = "YARN_UNSAFE_HTTP_WHITELIST";

/// npm's redirect: the default registry at `<base>/npm/`. Plain variables, not
/// secret: they carry no token and are the same for every job. Only the default
/// registry moves; a scope a project points at its own registry (`@scope:registry`
/// in its `.npmrc`) is untouched.
pub fn npm_env(ctx: &EcosystemCtx<'_>) -> Result<Vec<EnvVar>, String> {
    let url = format!("{}/npm/", ctx.base_url);
    let var = |name: &str, value: String| EnvVar {
        name: name.to_string(),
        value,
        secret: false,
    };
    let mut vars: Vec<EnvVar> = NPM_REGISTRY_VARS
        .iter()
        .map(|n| var(n, url.clone()))
        .collect();
    vars.push(var(YARN_HTTP_ALLOW, faber_mirror::ALIAS.to_string()));
    Ok(vars)
}

/// git's count of environment config entries.
pub const GIT_CONFIG_COUNT: &str = "GIT_CONFIG_COUNT";

/// The most `GIT_CONFIG_COUNT` a profile may already set and still be appended
/// to. git reads every index below the count, so anything near this is a typo.
const GIT_CONFIG_COUNT_MAX: usize = 1000;

/// git-lfs's redirect, through git's environment config: `lfs.url` at the
/// mirror, with the container's token in its path, and `lfs.pushurl` at the
/// repo's own GitHub LFS endpoint, so pushes keep the job's own credentials.
///
/// **A profile that sets `GIT_CONFIG_COUNT` is appended to, not overwritten.**
/// Its entries `0..n` keep their indexes and stay on the runner (they are the
/// profile's own `env`); faber's two take `n` and `n + 1`, and the count it sets
/// is `n + 2`, after the profile's on the argv, so it wins. A count that is not a
/// number git would read is refused, with the reason, and the job runs without
/// the LFS redirect.
///
/// Every variable is secret: the token is in `lfs.url`, and the count and keys
/// travel the same way so the set is never half on the argv.
pub fn lfs_env(ctx: &EcosystemCtx<'_>) -> Result<Vec<EnvVar>, String> {
    let token = ctx
        .token
        .ok_or_else(|| "no mirror token was minted for this container".to_string())?;
    if !faber_mirror::store::valid_repo(ctx.owner_repo) {
        return Err(format!(
            "{:?} is not a repo the mirror can index",
            ctx.owner_repo
        ));
    }
    let base = match ctx.profile_env.get(GIT_CONFIG_COUNT) {
        None => 0,
        Some(v) => v
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|n| *n <= GIT_CONFIG_COUNT_MAX)
            .ok_or_else(|| {
                format!(
                    "the profile's env sets {GIT_CONFIG_COUNT}={v:?}, which is not a count \
                     faber can append to"
                )
            })?,
    };
    let url = format!(
        "{}/lfs/{}/{}/info/lfs",
        ctx.base_url,
        token.secret(),
        ctx.owner_repo
    );
    let push = format!("https://github.com/{}.git/info/lfs", ctx.owner_repo);
    let var = |name: String, value: String| EnvVar {
        name,
        value,
        secret: true,
    };
    Ok(vec![
        var(GIT_CONFIG_COUNT.to_string(), (base + 2).to_string()),
        var(format!("GIT_CONFIG_KEY_{base}"), "lfs.url".into()),
        var(format!("GIT_CONFIG_VALUE_{base}"), url),
        var(format!("GIT_CONFIG_KEY_{}", base + 1), "lfs.pushurl".into()),
        var(format!("GIT_CONFIG_VALUE_{}", base + 1), push),
    ])
}

/// A credential minted for one runner container's requests through the mirror,
/// which the service maps to that container's repo (ADR-0047 §4). It has no
/// `Debug`, `Display` or `Serialize`, so it cannot reach a log line or the
/// dashboard by accident.
pub struct RepoToken(String);

impl RepoToken {
    pub fn new(secret: String) -> Self {
        Self(secret)
    }

    /// A fresh random token: 128 bits from `/dev/urandom`, in hex so it is
    /// URL-safe. `None` when the system will not give randomness.
    pub fn mint() -> Option<Self> {
        crate::services::random_password().ok().map(Self)
    }

    /// The secret, for an ecosystem to put in a [`EnvVar`] marked `secret`.
    pub fn secret(&self) -> &str {
        &self.0
    }
}

/// Secret variables. No `Debug`, `Display` or `Serialize`, like [`RepoToken`].
#[derive(Default)]
pub struct SecretEnv(Vec<(String, String)>);

impl SecretEnv {
    /// The names, for the argv (`-e NAME`, no value).
    pub fn names(&self) -> Vec<&str> {
        self.0.iter().map(|(k, _)| k.as_str()).collect()
    }

    /// The pairs, for the one `docker run` child's own environment.
    pub fn pairs(&self) -> Vec<(&str, &str)> {
        self.0
            .iter()
            .map(|(k, v)| (k.as_str(), v.as_str()))
            .collect()
    }
}

/// What a runner is handed when it is pointed at the mirror.
#[derive(Default)]
pub struct Injection {
    /// The plain variables, in order: [`ENV`] first, then each ecosystem's. They
    /// go on the runner's argv after the profile's own `env`, so faber's value
    /// wins, as for `FABER_GIT_MIRROR`.
    pub env: Vec<(String, String)>,
    /// The secret ones: names on the argv, values in the CLI child's environment.
    pub secret_env: SecretEnv,
    /// The ecosystems this runner is redirected for.
    pub ecosystems: Vec<&'static str>,
    /// The ecosystems it is not, and why.
    pub skipped: Vec<(&'static str, String)>,
}

/// Whether a runner is pointed at the mirror: only when its profile opted in and
/// the mirror is usable right now. Otherwise nothing is injected and the job runs
/// as it would without the mirror. `token` is the container's own, handed to each
/// ecosystem with the runner's repo.
pub fn plan(
    opted_in: bool,
    condition: &Condition,
    owner_repo: &str,
    token: Option<RepoToken>,
    profile_env: &BTreeMap<String, String>,
    ecosystems: &[Ecosystem],
) -> Option<Injection> {
    if !(opted_in && condition.usable()) {
        return None;
    }
    let base = base_url();
    let ctx = EcosystemCtx {
        base_url: &base,
        owner_repo,
        token: token.as_ref(),
        profile_env,
    };
    let mut inj = Injection {
        env: vec![(ENV.to_string(), base.clone())],
        ..Injection::default()
    };
    for e in ecosystems {
        let vars = match (e.env)(&ctx) {
            Ok(vars) => vars,
            Err(why) => {
                inj.skipped.push((e.name, why));
                continue;
            }
        };
        inj.ecosystems.push(e.name);
        for v in vars {
            if v.secret {
                inj.secret_env.0.push((v.name, v.value));
            } else {
                inj.env.push((v.name, v.value));
            }
        }
    }
    Some(inj)
}

/// Whether the host is short of disk for the mirror (ADR-0047 §8): the latest
/// fresh reading of the Docker volume is below ADR-0024's admission floor plus
/// [`LOW_DISK_MARGIN_GIB`]. With no floor set, the margin alone. No fresh
/// reading is not evidence, and the mirror keeps storing.
pub fn low_disk(floor_gib: Option<u64>, free_mib: Option<u64>) -> bool {
    let floor = floor_gib.unwrap_or(0) + LOW_DISK_MARGIN_GIB;
    free_mib.is_some_and(|free| free < floor * 1024)
}

/// The fingerprint of the credential the mirror is started with: what its
/// container's [`LABEL_CREDENTIAL`] records. `None` with LFS off (no credential
/// is mounted); `anonymous` when faber itself has no token. 64 bits of a
/// sha256 of a high-entropy token say nothing about it.
pub fn credential_fingerprint(lfs: bool, token: &str) -> Option<String> {
    use sha2::Digest as _;
    if !lfs {
        return None;
    }
    if token.is_empty() {
        return Some("anonymous".into());
    }
    let digest = sha2::Sha256::digest(token.as_bytes());
    Some(digest.iter().take(8).map(|b| format!("{b:02x}")).collect())
}

// --- health (pure) ------------------------------------------------------------------

/// What faber last learned about the mirror.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    /// Whether [`image`] is on the runners' engine. `None` until first checked,
    /// and when the engine could not say.
    pub image_present: Option<bool>,
    /// Since when the image has been missing.
    pub image_missing_since_ms: Option<u64>,
    /// The last time the mirror answered its health probe.
    pub last_healthy_ms: Option<u64>,
    /// Whether the latest reading was that answer. Cleared at once by a failed
    /// probe, a foreign container under the name, or a replace, so no job is
    /// pointed at a mirror that stopped answering (ADR-0047 §7), however recent
    /// its last good answer.
    pub reading_ok: bool,
    /// Since when it has not answered: the first failed reading after a healthy
    /// one, or when faber started tending it.
    pub unhealthy_since_ms: Option<u64>,
    /// The last report it gave.
    pub report: Option<faber_mirror::Health>,
    /// Why the last reading failed.
    pub last_error: Option<String>,
}

/// The mirror's state, as jobs and the operator see it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    /// The image this faber runs is not on the runners' engine.
    ImageMissing { since_ms: Option<u64> },
    /// Never healthy yet, and not for longer than the window.
    Starting,
    /// Was healthy, and has stopped answering for less than the window. Not
    /// used by jobs, not yet reported.
    Faltering { since_ms: Option<u64> },
    /// Answering, and caching.
    Healthy,
    /// Answering, but not caching: every request goes to upstream.
    PassingThrough { reason: String },
    /// No healthy reading for longer than the window.
    Down {
        since_ms: Option<u64>,
        error: Option<String>,
    },
}

impl Condition {
    /// Whether a job may be pointed at the mirror.
    pub fn usable(&self) -> bool {
        matches!(self, Self::Healthy | Self::PassingThrough { .. })
    }

    /// Whether it is a fault worth an audit line and a red row.
    pub fn bad(&self) -> bool {
        matches!(self, Self::ImageMissing { .. } | Self::Down { .. })
    }

    fn word(&self) -> &'static str {
        match self {
            Self::ImageMissing { .. } => "image_missing",
            Self::Starting => "starting",
            Self::Faltering { .. } => "faltering",
            Self::Healthy => "ok",
            Self::PassingThrough { .. } => "passing_through",
            Self::Down { .. } => "down",
        }
    }
}

/// The condition `s` describes at `now_ms`.
pub fn condition(s: &Snapshot, now_ms: u64) -> Condition {
    if s.image_present == Some(false) {
        return Condition::ImageMissing {
            since_ms: s.image_missing_since_ms,
        };
    }
    let fresh = s.reading_ok
        && s.last_healthy_ms
            .is_some_and(|t| now_ms.saturating_sub(t) <= HEALTHY_WINDOW_MS);
    if fresh {
        return match &s.report {
            Some(r) if r.status == faber_mirror::HealthStatus::PassingThrough => {
                Condition::PassingThrough {
                    reason: r
                        .reason
                        .clone()
                        .unwrap_or_else(|| "no reason given".to_string()),
                }
            }
            _ => Condition::Healthy,
        };
    }
    let since = s.unhealthy_since_ms.or(s.last_healthy_ms);
    let within = since.is_none_or(|u| now_ms.saturating_sub(u) < HEALTHY_WINDOW_MS);
    match (within, s.last_healthy_ms) {
        (true, None) => Condition::Starting,
        (true, Some(_)) => Condition::Faltering { since_ms: since },
        (false, _) => Condition::Down {
            since_ms: since,
            error: s.last_error.clone(),
        },
    }
}

/// What the audit log last said about the mirror. Starting and faltering do not
/// change it: they are not yet worth a line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    PassingThrough,
    Bad,
}

impl Level {
    fn of(c: &Condition) -> Option<Self> {
        match c {
            Condition::Healthy => Some(Self::Ok),
            Condition::PassingThrough { .. } => Some(Self::PassingThrough),
            Condition::ImageMissing { .. } | Condition::Down { .. } => Some(Self::Bad),
            Condition::Starting | Condition::Faltering { .. } => None,
        }
    }

    /// The level audit `entries` leave the mirror at. A restart starts from it,
    /// not from `Ok`, so a mirror that was down or passing through and is healthy
    /// by the time this process looks still gets its `recovered` line.
    pub fn replayed(entries: &[crate::audit::Entry]) -> Self {
        let open = crate::reconcile::replayed_open_faults(entries);
        let last = open
            .iter()
            .find(|f| f.scope == "mirror" && f.repo == "-")
            .map(|f| f.action.as_str());
        match last {
            Some("failed") => Self::Bad,
            Some("passing_through") => Self::PassingThrough,
            _ => Self::Ok,
        }
    }
}

/// The audit `fault` line a move to `next` earns, given the last level written,
/// and the level it writes: `failed` on going down, `passing_through` on starting
/// to pass through, `recovered` on getting back to healthy. One per change of
/// level, never one per poll.
pub fn transition(prev: Level, next: &Condition) -> Option<(&'static str, Option<String>, Level)> {
    let level = Level::of(next).filter(|l| *l != prev)?;
    let (action, cause) = match next {
        Condition::ImageMissing { .. } => (
            "failed",
            Some(format!(
                "image {} is not on the runners' engine; build it with \
                 deploy/build-mirror-image.sh",
                image()
            )),
        ),
        Condition::Down { error, .. } => (
            "failed",
            Some(format!(
                "no healthy reading for {} s{}",
                HEALTHY_WINDOW_MS / 1000,
                error
                    .as_deref()
                    .map(|e| format!(": {e}"))
                    .unwrap_or_default()
            )),
        ),
        Condition::PassingThrough { reason } => ("passing_through", Some(reason.clone())),
        _ => ("recovered", None),
    };
    Some((action, cause, level))
}

// --- surface (pure) -----------------------------------------------------------------

/// One attention row, for the dashboard and the tray (docs/design/popover.md).
/// Text only: the clients format `since_ms` themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AttentionRow {
    pub key: &'static str,
    /// `bad` or `warn`.
    pub severity: &'static str,
    pub title: &'static str,
    /// The prose. Names, images, commands and errors are never baked into it:
    /// they travel in the fields below, so each client sets them in mono.
    pub detail: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since_ms: Option<u64>,
    /// The image the row is about (`Mirror image missing`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The command that fixes it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    /// The last error, verbatim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The runners the outage affected (`Mirror down`). The popover shows their
    /// count, the dashboard their names.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub affected: Vec<String>,
}

/// The script that builds the mirror's image.
pub const BUILD_COMMAND: &str = "deploy/build-mirror-image.sh";

/// The rows `c` earns, the down row naming the jobs it `affected`. A healthy
/// or starting mirror earns none.
pub fn attention(c: &Condition, affected: &[String]) -> Vec<AttentionRow> {
    match c {
        Condition::ImageMissing { since_ms } => vec![AttentionRow {
            key: "mirror-image",
            severity: "bad",
            title: "Mirror image missing",
            detail: "Not on the runners' engine; jobs run without the mirror until it is built."
                .to_string(),
            since_ms: *since_ms,
            image: Some(image()),
            command: Some(BUILD_COMMAND.to_string()),
            error: None,
            affected: Vec::new(),
        }],
        Condition::Down { since_ms, error } => vec![AttentionRow {
            key: "mirror-down",
            severity: "bad",
            title: "Mirror down",
            detail: "No healthy answer from the mirror; new jobs run without it.".to_string(),
            since_ms: *since_ms,
            image: None,
            command: None,
            error: error.clone(),
            affected: affected.to_vec(),
        }],
        Condition::PassingThrough { reason } => vec![AttentionRow {
            key: "mirror-passing",
            severity: "warn",
            title: "Mirror passing through",
            detail: format!("{reason}. Jobs still fetch through it, uncached."),
            since_ms: None,
            image: None,
            command: None,
            error: None,
            affected: Vec::new(),
        }],
        Condition::Starting | Condition::Faltering { .. } | Condition::Healthy => Vec::new(),
    }
}

/// The mirror in `/api/state`. `null` there when `[mirror]` is absent.
#[derive(Debug, Clone, Serialize)]
pub struct MirrorView {
    /// The image this faber runs.
    pub image: String,
    /// `ok`, `passing_through`, `starting`, `faltering`, `down` or `image_missing`.
    pub state: &'static str,
    pub max_size_gib: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_healthy_ms: Option<u64>,
    /// The version the running service reports.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// How many job networks it is connected to now.
    pub attached: usize,
    pub attention: Vec<AttentionRow>,
    /// Git LFS through the mirror, with `[mirror.lfs]` on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lfs: Option<EcosystemView>,
    /// The npm registry through the mirror, with `[mirror.npm]` on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub npm: Option<EcosystemView>,
}

/// One adapter's figures since the service started: what the trial reads
/// (ADR-0047 §10), since CI logs carry no byte counts.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct EcosystemView {
    /// `relaxed` or `off`.
    pub fill: &'static str,
    /// This ecosystem's objects and bytes in the store, and the store's one cap
    /// across ecosystems (ADR-0047 §8).
    pub objects: u64,
    pub bytes: u64,
    pub max_bytes: u64,
    /// Per repo, busiest first. npm's may hold `(unattributed)`: requests on an
    /// address no sync had named yet.
    pub repos: Vec<RepoView>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct RepoView {
    pub repo: String,
    pub hits: u64,
    pub misses: u64,
    pub bytes_from_cache: u64,
    pub bytes_from_upstream: u64,
    pub passed_through: u64,
}

/// The LFS figures from the service's last sync answer.
pub fn lfs_view(fill: faber_mirror::Fill, stats: Option<&faber_mirror::Stats>) -> EcosystemView {
    ecosystem_view(LFS.name, fill, stats, |s| &s.repos)
}

/// The npm figures from the service's last sync answer.
pub fn npm_view(fill: faber_mirror::Fill, stats: Option<&faber_mirror::Stats>) -> EcosystemView {
    ecosystem_view(NPM.name, fill, stats, |s| &s.npm)
}

fn ecosystem_view(
    name: &str,
    fill: faber_mirror::Fill,
    stats: Option<&faber_mirror::Stats>,
    counters: fn(&faber_mirror::Stats) -> &BTreeMap<String, faber_mirror::RepoCounters>,
) -> EcosystemView {
    let mut repos: Vec<RepoView> = stats
        .map(|s| {
            counters(s)
                .iter()
                .map(|(repo, c)| RepoView {
                    repo: untrusted::for_log(repo, 200),
                    hits: c.hits,
                    misses: c.misses,
                    bytes_from_cache: c.bytes_from_cache,
                    bytes_from_upstream: c.bytes_from_upstream,
                    passed_through: c.passed_through,
                })
                .collect()
        })
        .unwrap_or_default();
    repos.sort_by(|a, b| {
        (b.bytes_from_cache + b.bytes_from_upstream)
            .cmp(&(a.bytes_from_cache + a.bytes_from_upstream))
            .then_with(|| a.repo.cmp(&b.repo))
    });
    let store = stats.map(|s| s.store.clone()).unwrap_or_default();
    let usage = store.usage.get(name).cloned().unwrap_or_default();
    EcosystemView {
        fill: fill.word(),
        objects: usage.objects,
        bytes: usage.bytes,
        max_bytes: store.max_bytes,
        repos,
    }
}

fn view_of(
    policy: &MirrorPolicy,
    s: &Snapshot,
    attached: usize,
    affected: &[String],
    stats: Option<&faber_mirror::Stats>,
    now_ms: u64,
) -> MirrorView {
    let c = condition(s, now_ms);
    MirrorView {
        image: image(),
        state: c.word(),
        max_size_gib: policy.max_size_gib,
        last_healthy_ms: s.last_healthy_ms,
        version: s.report.as_ref().map(|r| r.version.clone()),
        attached,
        attention: attention(&c, affected),
        lfs: policy.lfs.map(|fill| lfs_view(fill, stats)),
        npm: policy.npm.map(|fill| npm_view(fill, stats)),
    }
}

// --- the container (pure) -----------------------------------------------------------

/// What `docker container inspect` said about [`CONTAINER`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Found {
    Absent,
    Present {
        running: bool,
        /// The ID of the image it runs (`sha256:…`), not its tag: builds from
        /// one version share a tag, and only the ID says a rebuild happened.
        image_id: String,
        /// Its `org.faber.kind`, when it has one.
        kind: Option<String>,
        /// Its [`LABEL_CREDENTIAL`], when it has one.
        credential: Option<String>,
    },
}

/// What the janitor does about [`Found`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// None exists: create the volume and the container.
    Create,
    /// It is faber's, current and running: adopt it as it is (ADR-0011).
    Keep,
    /// It is faber's but stopped, dead, running another image than the one
    /// [`image`] names now, or started with another credential: remove it and
    /// create it again. The cache volume stays.
    Replace(&'static str),
    /// Something faber did not make holds the name: leave it alone.
    Foreign,
}

/// The janitor's decision, against `want_id`, the ID [`image`] resolves to now,
/// and `want_credential`, the [`credential_fingerprint`] it should run with.
/// Provenance is the label, never the name.
pub fn decide(found: &Found, want_id: &str, want_credential: Option<&str>) -> Step {
    match found {
        Found::Absent => Step::Create,
        Found::Present { kind, .. } if kind.as_deref() != Some(KIND_MIRROR) => Step::Foreign,
        Found::Present { image_id, .. } if image_id != want_id => Step::Replace("another image"),
        Found::Present { credential, .. } if credential.as_deref() != want_credential => {
            Step::Replace("its GitHub credential changed")
        }
        Found::Present { running: false, .. } => Step::Replace("not running"),
        Found::Present { .. } => Step::Keep,
    }
}

/// The marks on the container and its volume: faber's, kind `mirror`, and no
/// repo or claim, so nothing reads them as a job's.
fn mirror_marks() -> Vec<String> {
    [
        format!("{LABEL_OWNED}=true"),
        format!("{LABEL_KIND}={KIND_MIRROR}"),
    ]
    .into_iter()
    .flat_map(|kv| ["--label".to_string(), kv])
    .collect()
}

fn volume_create_args() -> Vec<String> {
    let mut a: Vec<String> = vec!["volume".into(), "create".into()];
    a.extend(mirror_marks());
    a.push(VOLUME.into());
    a
}

/// `docker run` for the mirror. Detached, restarted by the engine itself unless
/// faber stops it, never pulled, on the default bridge for its own upstream
/// traffic. Jobs reach it only on their own networks, where faber connects it.
fn run_args(policy: &MirrorPolicy, image: &str, credential: Option<&Credential>) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "run".into(),
        "-d".into(),
        "--pull=never".into(),
        "--name".into(),
        CONTAINER.into(),
        "--restart".into(),
        "unless-stopped".into(),
        "--memory".into(),
        MEMORY.into(),
        "--memory-swap".into(),
        MEMORY.into(),
    ];
    a.extend(mirror_marks());
    if let Some(c) = credential {
        a.extend([
            "--label".into(),
            format!("{LABEL_CREDENTIAL}={}", c.fingerprint),
        ]);
        if c.mounted {
            a.extend([
                "-v".into(),
                format!("{AUTH_VOLUME}:{}:ro", faber_mirror::CREDENTIAL_DIR),
            ]);
        }
    }
    a.extend([
        "-v".into(),
        format!("{VOLUME}:{}", faber_mirror::CACHE_DIR),
        "-e".into(),
        format!("{}={}", faber_mirror::MAX_SIZE_ENV, policy.max_size_gib),
        image.into(),
        "serve".into(),
    ]);
    a
}

fn image_inspect_args(image: &str) -> Vec<String> {
    ["image", "inspect", "--format", "{{.Id}}", image]
        .map(String::from)
        .to_vec()
}

/// One JSON object: whether it runs, its image's ID, its kind label.
const INSPECT_FORMAT: &str = concat!(
    r#"{"Running":{{json .State.Running}},"ImageId":{{json .Image}},"#,
    r#""Kind":{{json (index .Config.Labels "org.faber.kind")}},"#,
    r#""Credential":{{json (index .Config.Labels "org.faber.credential")}}}"#
);

fn inspect_args() -> Vec<String> {
    [
        "container",
        "inspect",
        "--format",
        INSPECT_FORMAT,
        CONTAINER,
    ]
    .map(String::from)
    .to_vec()
}

/// The networks container `name` is on, one name per line.
fn networks_args(name: &str) -> Vec<String> {
    [
        "container",
        "inspect",
        "--format",
        r#"{{range $k, $v := .NetworkSettings.Networks}}{{println $k}}{{end}}"#,
        name,
    ]
    .map(String::from)
    .to_vec()
}

/// The mirror's networks with its IPv4 address on each, one `<net> <ip>` per
/// line: what a sync attributes npm requests by.
fn addrs_args() -> Vec<String> {
    [
        "container",
        "inspect",
        "--format",
        r#"{{range $k, $v := .NetworkSettings.Networks}}{{$k}} {{$v.IPAddress}}{{println}}{{end}}"#,
        CONTAINER,
    ]
    .map(String::from)
    .to_vec()
}

/// Job network to the mirror's address on it, from [`addrs_args`]' output. A
/// line that is not a job network and an address is dropped.
fn parse_addrs(lines: &[String]) -> HashMap<String, String> {
    lines
        .iter()
        .filter_map(|l| {
            let (net, ip) = l.trim().split_once(' ')?;
            let ip = ip.trim().parse::<std::net::IpAddr>().ok()?;
            let ok = net.starts_with("faber-net-") && untrusted::object_name(net);
            ok.then(|| (net.to_string(), ip.to_string()))
        })
        .collect()
}

fn probe_args() -> Vec<String> {
    ["exec", CONTAINER, "/usr/local/bin/faber-mirror", "health"]
        .map(String::from)
        .to_vec()
}

fn connect_args(net: &str) -> Vec<String> {
    [
        "network",
        "connect",
        "--alias",
        faber_mirror::ALIAS,
        net,
        CONTAINER,
    ]
    .map(String::from)
    .to_vec()
}

fn disconnect_args(net: &str, name: &str) -> Vec<String> {
    ["network", "disconnect", "-f", net, name]
        .map(String::from)
        .to_vec()
}

/// Every container carrying faber's mirror labels, running or not: what a host
/// with `[mirror]` removed still has to disconnect and retire. The filters are
/// ANDed, so nothing unlabelled can be listed.
fn labelled_ls_args() -> Vec<String> {
    vec![
        "ps".into(),
        "-a".into(),
        "--filter".into(),
        format!("label={LABEL_OWNED}=true"),
        "--filter".into(),
        format!("label={LABEL_KIND}={KIND_MIRROR}"),
        "--format".into(),
        "{{.Names}}".into(),
    ]
}

#[derive(serde::Deserialize)]
struct Inspected {
    #[serde(rename = "Running")]
    running: bool,
    #[serde(rename = "ImageId")]
    image_id: String,
    #[serde(rename = "Kind")]
    kind: Option<String>,
    #[serde(rename = "Credential", default)]
    credential: Option<String>,
}

/// [`Found`] from the inspect's output. `None` when it is not the one object
/// [`INSPECT_FORMAT`] prints.
fn parse_inspect(text: &str) -> Option<Found> {
    let i: Inspected = serde_json::from_str(text.trim()).ok()?;
    Some(Found::Present {
        running: i.running,
        image_id: i.image_id,
        kind: i.kind.filter(|k| !k.is_empty()),
        credential: i.credential.filter(|k| !k.is_empty()),
    })
}

/// The job networks among `lines`: the ones to reconnect after a replace.
fn job_networks(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|l| l.trim())
        .filter(|n| n.starts_with("faber-net-") && untrusted::object_name(n))
        .map(String::from)
        .collect()
}

/// The health report in a probe's output, its text made safe for the
/// dashboard: the service is faber's, but jobs talk to it.
fn parse_report(stdout: &str) -> Result<faber_mirror::Health, String> {
    let mut h: faber_mirror::Health = serde_json::from_str(stdout.trim())
        .map_err(|_| "the health probe printed something that is not a report".to_string())?;
    h.version = untrusted::for_log(&h.version, 64);
    h.reason = h.reason.map(|r| untrusted::for_log(&r, TEXT_CHARS));
    Ok(h)
}

/// Whether a failed docker call says the object does not exist.
fn no_such(err: &str) -> bool {
    err.contains("No such") || err.contains("no such")
}

// --- the live state -----------------------------------------------------------------

/// How many released networks are remembered. A network name is unique per
/// claim, and the race the set closes lasts seconds, so the oldest can go.
const RELEASED_CAP: usize = 4096;

/// The job networks the mirror serves, and the ones teardown has let go.
#[derive(Default)]
struct Nets {
    attached: HashSet<String>,
    /// Networks [`Mirror::release_network`] disconnected. A replace never
    /// reconnects one, so a reconnect cannot land between teardown's disconnect
    /// and its `network rm` and leave an endpoint that blocks the removal.
    released: HashSet<String>,
}

impl Nets {
    fn release(&mut self, net: &str) {
        self.attached.remove(net);
        if self.released.len() >= RELEASED_CAP {
            self.released.clear();
        }
        self.released.insert(net.to_string());
    }
}

/// A runner container's token, as far as this process knows it.
#[derive(Clone, PartialEq, Eq)]
enum Token {
    /// No ecosystem that needs one is on for it.
    None,
    /// Minted here; the value is sent at every sync.
    Minted(String),
    /// The service holds one this process never knew: a container adopted
    /// across faber's own restart. Sent by id alone, so the service keeps it.
    Adopted,
}

/// One runner container the mirror serves, by its job network.
#[derive(Clone)]
struct Job {
    repo: String,
    runner: String,
    token: Token,
    /// Whether the service knows this job: it was redirected for an ecosystem
    /// (a token, or an address it is attributed by), or adopted across faber's
    /// own restart. Its teardown syncs, and a service restart is its incident.
    redirected: bool,
    /// Why the mirror failed this job, once it has (ADR-0047 §7): what turns a
    /// failed outcome into `cause: mirror`.
    incident: Option<String>,
    /// The service's failure count for it, already noted.
    seen_failures: u64,
}

/// What the syncs so far have established.
#[derive(Default)]
struct SyncState {
    /// A sync has answered since this process started, so the next one may
    /// revoke: before it, the service's table is adopted, not replaced.
    adopted: bool,
    /// The service instance the last answer came from.
    instance: Option<String>,
}

/// The process's mirror ([`global`]).
#[derive(Default)]
pub struct Mirror {
    policy: OnceLock<MirrorPolicy>,
    state: Mutex<Snapshot>,
    /// Held across each connect and disconnect, so attach, release and a
    /// replace's reconnects happen one at a time.
    nets: tokio::sync::Mutex<Nets>,
    /// `nets.attached.len()`, readable without the async lock.
    attached_count: AtomicUsize,
    /// With `[mirror]` absent: the faber-labelled mirror containers a boot
    /// found, still to be disconnected from job networks and retired.
    lingering: Mutex<Vec<String>>,
    /// The runner containers it serves, by job network.
    jobs: Mutex<HashMap<String, Job>>,
    /// One sync at a time, each built from the table as it is then, so an older
    /// table never lands after a newer one.
    sync: tokio::sync::Mutex<SyncState>,
    /// The host is short of disk (ADR-0047 §8), as the janitor last read it.
    low_disk: AtomicBool,
    /// The service's last sync answer.
    stats: Mutex<Option<faber_mirror::Stats>>,
    /// The runners an outage has affected, for the "Mirror down" row. Cleared
    /// when the mirror is healthy again.
    affected: Mutex<VecDeque<String>>,
    /// Incidents found outside the janitor, for it to write to the audit log:
    /// `(repo, what happened)`.
    faults: Mutex<Vec<(String, String)>>,
    /// faber's GitHub client, whose token the LFS adapter fetches with.
    gh: OnceLock<Arc<crate::github::Github>>,
}

/// The process's mirror.
pub fn global() -> &'static Mirror {
    static MIRROR: OnceLock<Mirror> = OnceLock::new();
    MIRROR.get_or_init(Mirror::default)
}

impl Mirror {
    /// Turn the mirror on for this process. Called once at boot, and only when
    /// `[mirror]` is present and valid.
    pub fn configure(&self, policy: MirrorPolicy) {
        if self.policy.set(policy).is_err() {
            warn!("mirror: configured twice; keeping the first");
            return;
        }
        self.state.lock().unwrap().unhealthy_since_ms = Some(now_ms());
    }

    /// Whether `[mirror]` is on.
    pub fn enabled(&self) -> bool {
        self.policy.get().is_some()
    }

    /// The mirror's condition now, or `None` with the feature off.
    pub fn condition(&self) -> Option<Condition> {
        self.policy.get()?;
        Some(condition(&self.state.lock().unwrap(), now_ms()))
    }

    /// The mirror in `/api/state`, or `None` with the feature off.
    pub fn view(&self) -> Option<MirrorView> {
        let policy = self.policy.get()?;
        let attached = self.attached_count.load(Ordering::Relaxed);
        let affected: Vec<String> = self.affected.lock().unwrap().iter().cloned().collect();
        Some(view_of(
            policy,
            &self.state.lock().unwrap(),
            attached,
            &affected,
            self.stats.lock().unwrap().as_ref(),
            now_ms(),
        ))
    }

    /// Note that the mirror failed `job`, for its outcome. A single failed
    /// request (`record`) earns an audit `fault` line of its own; an outage
    /// instead names the job on the "Mirror down" row and in the one line the
    /// janitor writes for the outage.
    fn note_incident(&self, job: &mut Job, why: &str, record: bool) {
        if job.incident.is_none() {
            job.incident = Some(untrusted::for_log(why, TEXT_CHARS));
            if record {
                self.faults
                    .lock()
                    .unwrap()
                    .push((job.repo.clone(), format!("{}: {why}", job.runner)));
            }
        }
        if record {
            return;
        }
        let mut a = self.affected.lock().unwrap();
        if !a.contains(&job.runner) {
            if a.len() >= AFFECTED_CAP {
                a.pop_front();
            }
            a.push_back(job.runner.clone());
        }
    }

    /// The sync request for the table as it is now, with the service's address
    /// on each job's network from `addrs`. Built under the sync lock.
    fn sync_request(
        &self,
        policy: &MirrorPolicy,
        adopt: bool,
        addrs: &HashMap<String, String>,
    ) -> faber_mirror::SyncRequest {
        let entries = self
            .jobs
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, j)| j.redirected)
            .map(|(net, j)| faber_mirror::SyncEntry {
                id: net.clone(),
                repo: j.repo.clone(),
                token: match &j.token {
                    Token::Minted(t) => Some(t.clone()),
                    Token::None | Token::Adopted => None,
                },
                addr: addrs.get(net).cloned(),
            })
            .collect();
        faber_mirror::SyncRequest {
            entries,
            lfs: policy.lfs,
            npm: policy.npm,
            low_disk: self.low_disk.load(Ordering::Relaxed),
            max_size_bytes: Some(u64::from(policy.max_size_gib) << 30),
            adopt,
        }
    }

    /// Hand the service the whole token table and the host's disk state, and
    /// read back its counters (module docs: `docker exec -i`, the table on
    /// stdin). The answer also says which containers the mirror failed, and
    /// whether the service restarted since the last one.
    pub async fn sync(&self) -> anyhow::Result<faber_mirror::Stats> {
        let policy = self
            .policy
            .get()
            .ok_or_else(|| anyhow::anyhow!("the mirror is off"))?;
        let mut st = self.sync.lock().await;
        // The service's address on each job network, for npm's attribution. Read
        // at every sync, so a replace (which moves it) heals within one POLL.
        // Without npm nothing needs it, and no inspect runs.
        let addrs = if policy.npm.is_some() {
            match docker::docker_lines(&addrs_args(), CLI_TIMEOUT).await {
                Ok(lines) => parse_addrs(&lines),
                Err(e) => {
                    debug!("mirror: reading its addresses: {e} (requests go unattributed)");
                    HashMap::new()
                }
            }
        } else {
            HashMap::new()
        };
        let req = self.sync_request(policy, !st.adopted, &addrs);
        let body = serde_json::to_vec(&req)?;
        drop(req);
        let mut cmd = docker::docker_command();
        cmd.args(sync_args());
        let out = untrusted::output_within_fed(
            cmd,
            body,
            CLI_TIMEOUT,
            SYNC_CAP,
            "docker exec (mirror sync)",
        )
        .await?;
        if !out.status.success() {
            let err = untrusted::for_log(&String::from_utf8_lossy(&out.stderr), TEXT_CHARS);
            anyhow::bail!("the mirror sync exited {}: {err}", out.status);
        }
        let stats: faber_mirror::Stats = serde_json::from_slice(&out.stdout)
            .map_err(|_| anyhow::anyhow!("the mirror sync printed something that is not stats"))?;
        self.absorb(&mut st, &stats);
        *self.stats.lock().unwrap() = Some(stats.clone());
        Ok(stats)
    }

    /// What a sync answer says about the jobs.
    fn absorb(&self, st: &mut SyncState, stats: &faber_mirror::Stats) {
        let mut jobs = self.jobs.lock().unwrap();
        if !st.adopted {
            // The containers a previous faber minted for: keep their tokens
            // until their teardown releases the network.
            for id in &stats.ids {
                if id.starts_with("faber-net-") && untrusted::object_name(id) {
                    jobs.entry(id.clone()).or_insert_with(|| Job {
                        repo: String::new(),
                        runner: id.clone(),
                        token: Token::Adopted,
                        redirected: true,
                        incident: None,
                        seen_failures: 0,
                    });
                }
            }
            st.adopted = true;
        }
        let restarted = st.instance.as_ref().is_some_and(|i| *i != stats.instance);
        st.instance = Some(stats.instance.clone());
        for (id, job) in jobs.iter_mut() {
            if restarted && job.redirected {
                self.note_incident(
                    job,
                    "the mirror restarted while this job could use it",
                    true,
                );
            }
            if let Some(f) = stats.failures.get(id) {
                if f.count > job.seen_failures {
                    job.seen_failures = f.count;
                    let why = untrusted::for_log(&f.last, TEXT_CHARS);
                    self.note_incident(job, &format!("the mirror failed a request: {why}"), true);
                }
            }
        }
    }

    /// Every job the mirror serves now has been affected by an outage.
    fn note_outage(&self, why: &str) -> Vec<String> {
        let mut jobs = self.jobs.lock().unwrap();
        let mut names = Vec::new();
        for job in jobs.values_mut() {
            self.note_incident(job, why, false);
            names.push(job.runner.clone());
        }
        names.sort();
        names
    }

    fn note_unhealthy(&self, error: String) {
        let mut s = self.state.lock().unwrap();
        s.reading_ok = false;
        if s.unhealthy_since_ms.is_none() {
            s.unhealthy_since_ms = Some(now_ms());
        }
        s.last_error = Some(untrusted::for_log(&error, TEXT_CHARS));
    }

    fn note_healthy(&self, report: faber_mirror::Health) {
        let mut s = self.state.lock().unwrap();
        s.reading_ok = true;
        s.last_healthy_ms = Some(now_ms());
        s.unhealthy_since_ms = None;
        s.last_error = None;
        s.report = Some(report);
    }

    /// The injection hook, called once per runner after its network exists: when
    /// `opted_in` and the mirror is usable, connect it to `net` and return what
    /// the runner is handed. `None` otherwise, or when the connect fails; the job
    /// then runs as before. No Docker call with the feature off.
    ///
    /// Once connected, the job is synced to the service before the runner can
    /// use it: with a token when an ecosystem that needs one is on (LFS), and
    /// with the service's address on its network (npm's attribution). A sync
    /// that fails costs the job the ecosystems that need a token only: it gets
    /// the rest, and runs its other tools straight to upstream.
    pub async fn attach(
        &self,
        opted_in: bool,
        net: &str,
        owner_repo: &str,
        runner: &str,
        profile_env: &BTreeMap<String, String>,
    ) -> Option<Injection> {
        if !untrusted::object_name(net) {
            return None;
        }
        // Checked before the lock and again under it: a replace marks the mirror
        // unusable before it removes the container, and holds the lock while it
        // reconnects, so a job never joins a container on its way out.
        self.condition().filter(|c| opted_in && c.usable())?;
        let mut nets = self.nets.lock().await;
        let condition = self.condition().filter(|c| c.usable())?;
        let policy = self.policy.get()?;
        let mut ecosystems: Vec<Ecosystem> = ECOSYSTEMS
            .iter()
            .copied()
            .filter(|e| (e.on)(policy))
            .collect();
        if let Err(e) = docker::docker_ok(&connect_args(net), LAUNCH_TIMEOUT).await {
            warn!("mirror: connecting to {net} failed ({e}) — this job runs without the mirror");
            return None;
        }
        let mut token = None;
        if ecosystems.iter().any(|e| e.needs_token) {
            token = RepoToken::mint();
        }
        self.jobs.lock().unwrap().insert(
            net.to_string(),
            Job {
                repo: owner_repo.to_string(),
                runner: runner.to_string(),
                token: token
                    .as_ref()
                    .map_or(Token::None, |t| Token::Minted(t.secret().to_string())),
                redirected: !ecosystems.is_empty(),
                incident: None,
                seen_failures: 0,
            },
        );
        if !ecosystems.is_empty() {
            if let Err(e) = self.sync().await {
                if token.is_some() {
                    warn!(
                        "mirror: handing {runner}'s token to the mirror failed ({e:#}) — it runs \
                         without the LFS redirect"
                    );
                }
                token = None;
            }
        }
        if token.is_none() {
            ecosystems.retain(|e| !e.needs_token);
        }
        let injection = plan(
            opted_in,
            &condition,
            owner_repo,
            token,
            profile_env,
            &ecosystems,
        );
        let Some(injection) = injection else {
            disconnect(net, CONTAINER).await;
            self.forget(net).await;
            return None;
        };
        for (name, why) in &injection.skipped {
            warn!("mirror: {runner}: {name} is not redirected through the mirror: {why}");
        }
        if let Some(job) = self.jobs.lock().unwrap().get_mut(net) {
            if !injection.ecosystems.contains(&LFS.name) {
                // Minted but not used (the redirect refused this runner): the
                // next sync revokes it.
                job.token = Token::None;
            }
            job.redirected = !injection.ecosystems.is_empty();
        }
        nets.attached.insert(net.to_string());
        self.attached_count
            .store(nets.attached.len(), Ordering::Relaxed);
        Some(injection)
    }

    /// Drop `net`'s job and, if the service held a token for it, revoke it now.
    async fn forget(&self, net: &str) {
        let had = self.jobs.lock().unwrap().remove(net);
        if had.is_some_and(|j| j.redirected) {
            if let Err(e) = self.sync().await {
                debug!("mirror: revoking a token: {e:#} (the next sync revokes it)");
            }
        }
    }

    /// Disconnect the mirror from `net`, if it is on it. Called before any job
    /// network is removed: by teardown, and by the janitor for a leaked one. "Not
    /// connected" and "gone" are both done.
    ///
    /// Whatever the config, like ADR-0046's shared servers: with `[mirror]` on it
    /// is [`CONTAINER`]; with it off, the faber-labelled mirror containers a
    /// boot found ([`Mirror::find_lingering`]), which may still sit on the
    /// networks of jobs adopted across the restart. A host that never ran a
    /// mirror makes no call here.
    ///
    /// Teardown revokes the job's token here, with one last sync, and gets back
    /// why the mirror failed the job, if it did: the outcome's `cause: mirror`.
    pub async fn release_network(&self, net: &str) -> Option<String> {
        if !untrusted::object_name(net) {
            return None;
        }
        let names = release_targets(self.enabled(), &self.lingering.lock().unwrap());
        if names.is_empty() {
            return None;
        }
        let mut nets = self.nets.lock().await;
        nets.release(net);
        self.attached_count
            .store(nets.attached.len(), Ordering::Relaxed);
        for name in names {
            disconnect(net, &name).await;
        }
        drop(nets);
        let mut job = self.jobs.lock().unwrap().remove(net)?;
        if job.redirected {
            match self.sync().await {
                // The failures the service counted for it since the last sync.
                Ok(stats) => {
                    if let Some(f) = stats.failures.get(net) {
                        if f.count > job.seen_failures {
                            let why = untrusted::for_log(&f.last, TEXT_CHARS);
                            self.note_incident(
                                &mut job,
                                &format!("the mirror failed a request: {why}"),
                                true,
                            );
                        }
                    }
                }
                Err(e) => debug!("mirror: revoking {net}'s token: {e:#} (the next sync does)"),
            }
        }
        job.incident
    }

    /// The audit lines incidents found since the last call earned.
    fn take_faults(&self) -> Vec<(String, String)> {
        std::mem::take(&mut *self.faults.lock().unwrap())
    }

    /// With `[mirror]` absent: list the containers carrying faber's mirror labels,
    /// and remember them for [`Mirror::release_network`] and [`retire_lingering`].
    /// One Docker call, at boot. Returns how many it found.
    pub async fn find_lingering(&self) -> usize {
        if self.enabled() {
            return 0;
        }
        let names = match docker::docker_lines(&labelled_ls_args(), CLI_TIMEOUT).await {
            Ok(lines) => lingering_names(&lines),
            Err(e) => {
                debug!("mirror: listing labelled mirror containers: {e}");
                Vec::new()
            }
        };
        let n = names.len();
        *self.lingering.lock().unwrap() = names;
        n
    }
}

/// The containers a network release disconnects: [`CONTAINER`] with `[mirror]`
/// on, the lingering labelled ones with it off. Empty on a host that never ran
/// a mirror, which then makes no Docker call.
fn release_targets(enabled: bool, lingering: &[String]) -> Vec<String> {
    if enabled {
        vec![CONTAINER.to_string()]
    } else {
        lingering.to_vec()
    }
}

/// The names in a labelled listing that are safe to put on an argv.
fn lingering_names(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .map(|l| l.trim())
        .filter(|n| untrusted::object_name(n))
        .map(String::from)
        .collect()
}

/// `network disconnect -f`, where "not connected" and "gone" are both done.
async fn disconnect(net: &str, name: &str) {
    if let Err(e) = docker::docker_ok(&disconnect_args(net, name), docker::TEARDOWN_TIMEOUT).await {
        let msg = e.to_string();
        if !(no_such(&msg) || msg.contains("not connected")) {
            warn!("mirror: disconnecting {name} from {net}: {e:#}");
        }
    }
}

// --- the janitor --------------------------------------------------------------------

/// Tend the mirror forever: every [`POLL`], make sure its container exists and
/// runs the right image, read its health, and write an audit `fault` line when
/// it goes down, starts passing through, or recovers. Spawned only when
/// `[mirror]` is on.
pub async fn run_loop(orch: Arc<crate::reconcile::Orchestrator>) {
    let mirror = global();
    let Some(policy) = mirror.policy.get().cloned() else {
        return;
    };
    let _ = mirror.gh.set(orch.github().clone());
    info!(
        "mirror: tending {CONTAINER} ({}, cache limit {} GiB, LFS {}, npm {}) every {POLL:?}",
        image(),
        policy.max_size_gib,
        policy.lfs.map_or("off", |f| f.word()),
        policy.npm.map_or("off", |f| f.word())
    );
    let mut level = Level::replayed(&orch.audit_log().read_kind("fault"));
    let mut tick = tokio::time::interval(POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        tend(mirror, &policy).await;
        let limiter = orch.limiter();
        let low = low_disk(
            limiter.disk_admission().floor_gib(),
            limiter.fresh_disk_reading(now_ms()).map(|r| r.free_mib),
        );
        if mirror.low_disk.swap(low, Ordering::Relaxed) != low {
            info!(
                "mirror: host disk is {} faber's admission floor plus {LOW_DISK_MARGIN_GIB} GiB — \
                 the mirror {}",
                if low { "below" } else { "back above" },
                if low {
                    "stores nothing and evicts toward 80% of its cap"
                } else {
                    "stores again"
                }
            );
        }
        let next = mirror.condition().unwrap_or(Condition::Starting);
        // The table, the disk state and the counters, every pass: a service that
        // restarted gets its tokens back within one POLL.
        if next.usable() {
            if let Err(e) = mirror.sync().await {
                debug!("mirror: sync: {e:#}");
            }
        }
        if let Some((action, mut cause, new)) = transition(level, &next) {
            match action {
                "failed" => {
                    let names = mirror.note_outage("the mirror was down while this job used it");
                    if !names.is_empty() {
                        let named = format!("; jobs affected: {}", names.join(", "));
                        cause = Some(cause.unwrap_or_default() + &named);
                    }
                    warn!(
                        "mirror: {} — new jobs run without it",
                        cause.as_deref().unwrap_or("down")
                    )
                }
                "passing_through" => warn!(
                    "mirror: passing through ({}) — jobs fetch through it uncached",
                    cause.as_deref().unwrap_or("no reason given")
                ),
                _ => {
                    mirror.affected.lock().unwrap().clear();
                    info!("mirror: healthy again")
                }
            }
            orch.record_fault("-", "mirror", action, cause).await;
            level = new;
        }
        for (repo, what) in mirror.take_faults() {
            warn!("mirror: {what}");
            orch.record_fault(&repo, "mirror", "request_failed", Some(what))
                .await;
        }
    }
}

/// With `[mirror]` absent, which is the feature off (ADR-0047 §6): retire the
/// faber-labelled mirror containers a previous config left. Each is removed once
/// no job network holds it; until then [`Mirror::release_network`] keeps
/// disconnecting it as those jobs end. Its cache volume is kept: it is
/// reclaimable, and a mirror turned back on starts warm. A container without the
/// labels is never listed, so never touched. One Docker call on a host that
/// never ran a mirror.
pub async fn retire_lingering() {
    let m = global();
    if m.find_lingering().await == 0 {
        return;
    }
    info!("mirror: [mirror] is absent; retiring the mirror container once no job uses it");
    let mut tick = tokio::time::interval(POLL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tick.tick().await;
        let names = m.lingering.lock().unwrap().clone();
        if names.is_empty() {
            return;
        }
        for name in names {
            let nets = match docker::docker_lines(&networks_args(&name), CLI_TIMEOUT).await {
                Ok(lines) => job_networks(&lines),
                Err(e) if no_such(&e.to_string()) => Vec::new(),
                Err(e) => {
                    debug!("mirror: inspecting {name}: {e}");
                    continue;
                }
            };
            if !nets.is_empty() {
                continue;
            }
            // Under the nets lock, so no release is mid-disconnect against it.
            let _nets = m.nets.lock().await;
            match docker::docker_rm(&["rm", "-f", name.as_str()]).await {
                Ok(_) => {
                    info!("mirror: removed {name}, left from a config with [mirror]");
                    m.lingering.lock().unwrap().retain(|n| *n != name);
                }
                Err(e) => warn!("mirror: removing {name}: {e}"),
            }
        }
        if m.lingering.lock().unwrap().is_empty() {
            // The credential is not a cache: it goes with the container.
            remove_auth_volumes().await;
        }
    }
}

/// faber's labels on the credential volume and its fill container.
fn auth_marks() -> Vec<String> {
    [
        format!("{LABEL_OWNED}=true"),
        format!("{LABEL_KIND}={KIND_MIRROR_AUTH}"),
    ]
    .into_iter()
    .flat_map(|kv| ["--label".to_string(), kv])
    .collect()
}

/// Every credential volume faber made for the mirror, by its labels.
fn auth_ls_args() -> Vec<String> {
    vec![
        "volume".into(),
        "ls".into(),
        "-q".into(),
        "--filter".into(),
        format!("label={LABEL_OWNED}=true"),
        "--filter".into(),
        format!("label={LABEL_KIND}={KIND_MIRROR_AUTH}"),
    ]
}

/// Remove the mirror's credential volumes, found by their labels. Called only
/// once no mirror container mounts them: before one is started, and when the
/// last lingering one is retired.
async fn remove_auth_volumes() {
    let names = match docker::docker_lines(&auth_ls_args(), CLI_TIMEOUT).await {
        Ok(lines) => lingering_names(&lines),
        Err(e) => {
            debug!("mirror: listing credential volumes: {e}");
            return;
        }
    };
    for name in names {
        if let Err(e) = docker::docker_rm(&["volume", "rm", name.as_str()]).await {
            if !no_such(&e.to_string()) {
                warn!("mirror: removing credential volume {name}: {e}");
            }
        }
    }
}

/// The container [`fill_auth`] writes [`AUTH_VOLUME`] from.
const AUTH_FILL_CONTAINER: &str = "faber-mirror-auth-fill";

/// faber's GitHub credential, written into [`AUTH_VOLUME`] for the service: the
/// git mirror's refresh credential, by the same code ([`CredentialVolume`],
/// [`crate::git_mirror::credential_file`]), except that it outlives the call,
/// because the service reads it for as long as it runs. Handed to the service's
/// own user, read-only to it.
async fn fill_auth(image: &str, token: &str) -> anyhow::Result<()> {
    let labels = auth_marks();
    // The fill's name is fixed, and its container is removed after the fill
    // rather than by `--rm` (#348), so one whose removal failed would refuse the
    // name to every later fill.
    if let Err(e) = docker::docker_rm(&["rm", "-f", AUTH_FILL_CONTAINER]).await {
        warn!("mirror: removing a leftover {AUTH_FILL_CONTAINER}: {e}");
    }
    let fill = CredentialFill {
        volume: AUTH_VOLUME,
        labels: &labels,
        container: AUTH_FILL_CONTAINER,
        image,
        dir: faber_mirror::CREDENTIAL_DIR,
        file: faber_mirror::CREDENTIAL_FILE,
        owner: Some(faber_mirror::SERVICE_USER),
    };
    let body = crate::git_mirror::credential_file(token);
    CredentialVolume::fill(&fill, body.as_bytes()).await?.keep();
    Ok(())
}

/// What a mirror container is started with, credential-wise.
struct Credential {
    /// Its [`credential_fingerprint`], for [`LABEL_CREDENTIAL`].
    fingerprint: String,
    /// Whether [`AUTH_VOLUME`] is mounted: not when faber has no token.
    mounted: bool,
}

/// One pass: the image, the container, the health reading.
async fn tend(m: &Mirror, policy: &MirrorPolicy) {
    let want = image();
    // faber's token, only while LFS is on: nothing else in the service needs it.
    let token = match (policy.lfs.is_some(), m.gh.get()) {
        (true, Some(gh)) => Some(gh.token_for_git().await),
        _ => None,
    };
    let token = token.as_ref().map(|t| t.expose());
    let want_credential = credential_fingerprint(token.is_some(), token.unwrap_or(""));
    let want_id = match docker::docker_lines(&image_inspect_args(&want), CLI_TIMEOUT).await {
        Ok(lines) => {
            let mut s = m.state.lock().unwrap();
            s.image_present = Some(true);
            s.image_missing_since_ms = None;
            lines.into_iter().next().unwrap_or_default()
        }
        Err(e) if no_such(&e.to_string()) => {
            let mut s = m.state.lock().unwrap();
            if s.image_present != Some(false) {
                s.image_missing_since_ms = Some(now_ms());
            }
            s.image_present = Some(false);
            s.reading_ok = false;
            return;
        }
        Err(e) => {
            m.state.lock().unwrap().image_present = None;
            m.note_unhealthy(format!("checking the image: {e}"));
            return;
        }
    };

    let found = match docker::docker_stdout(&inspect_args(), CLI_TIMEOUT, PROBE_CAP).await {
        Ok(text) => parse_inspect(&text),
        Err(e) if no_such(&e.to_string()) => Some(Found::Absent),
        Err(e) => {
            m.note_unhealthy(format!("inspecting {CONTAINER}: {e}"));
            return;
        }
    };
    let Some(found) = found else {
        m.note_unhealthy(format!("{CONTAINER} could not be inspected"));
        return;
    };
    match decide(&found, &want_id, want_credential.as_deref()) {
        Step::Keep => {}
        Step::Foreign => {
            m.note_unhealthy(format!(
                "a container named {CONTAINER} exists that faber did not create"
            ));
            return;
        }
        Step::Create => {
            m.note_unhealthy(format!("{CONTAINER} does not exist"));
            info!("mirror: creating {CONTAINER} from {want}");
            if let Err(e) = start(policy, &want, token).await {
                m.note_unhealthy(format!("starting {CONTAINER}: {e}"));
                return;
            }
        }
        Step::Replace(why) => {
            // Unusable from this moment: no job is connected or handed the
            // variables while the container is replaced.
            m.note_unhealthy(format!("replacing {CONTAINER} ({why})"));
            info!("mirror: replacing {CONTAINER} ({why})");
            let mut nets = m.nets.lock().await;
            let mut candidates = docker::docker_lines(&networks_args(CONTAINER), CLI_TIMEOUT)
                .await
                .map(|lines| job_networks(&lines))
                .unwrap_or_default();
            candidates.extend(nets.attached.iter().cloned());
            let candidates = reconnect_candidates(candidates, &nets.released);
            if let Err(e) = docker::docker_rm(&["rm", "-f", CONTAINER]).await {
                m.note_unhealthy(format!("removing the old {CONTAINER}: {e}"));
                return;
            }
            nets.attached.clear();
            if let Err(e) = start(policy, &want, token).await {
                m.note_unhealthy(format!("starting {CONTAINER}: {e}"));
                m.attached_count.store(0, Ordering::Relaxed);
                return;
            }
            // The jobs it served keep their variables; give them the mirror back,
            // under the lock teardown's release takes, so a network released
            // before this point is never rejoined. A network that is gone is
            // skipped.
            for net in candidates {
                match docker::docker_ok(&connect_args(&net), LAUNCH_TIMEOUT).await {
                    Ok(()) => {
                        nets.attached.insert(net);
                    }
                    Err(e) => debug!("mirror: reconnecting to {net}: {e}"),
                }
            }
            m.attached_count
                .store(nets.attached.len(), Ordering::Relaxed);
        }
    }

    match docker::docker_stdout(&probe_args(), CLI_TIMEOUT, PROBE_CAP).await {
        Ok(out) => match parse_report(&out) {
            Ok(report) => m.note_healthy(report),
            Err(e) => m.note_unhealthy(e),
        },
        Err(e) => m.note_unhealthy(format!("health probe: {e}")),
    }
}

/// The networks a replace reconnects: job networks it was on, less any teardown
/// has released, each once.
fn reconnect_candidates(mut nets: Vec<String>, released: &HashSet<String>) -> Vec<String> {
    nets.retain(|n| !released.contains(n));
    nets.sort();
    nets.dedup();
    nets
}

/// Create the cache volume, the credential volume when `token` is given (LFS
/// on), and the container. Only ever called with no mirror container left, so
/// the previous credential volume is mounted by nothing and goes first.
async fn start(policy: &MirrorPolicy, image: &str, token: Option<&str>) -> anyhow::Result<()> {
    // Idempotent: an existing volume is a no-op that echoes its name.
    docker::docker_ok(&volume_create_args(), LAUNCH_TIMEOUT).await?;
    remove_auth_volumes().await;
    let credential = match token {
        None => None,
        Some(t) => {
            let mounted = !t.is_empty();
            if mounted {
                fill_auth(image, t).await?;
            }
            Some(Credential {
                fingerprint: credential_fingerprint(true, t).unwrap_or_default(),
                mounted,
            })
        }
    };
    docker::docker_ok(
        &run_args(policy, image, credential.as_ref()),
        LAUNCH_TIMEOUT,
    )
    .await
}

/// `docker exec -i` for a sync: the request is the child's stdin, never an
/// argument.
fn sync_args() -> Vec<String> {
    [
        "exec",
        "-i",
        CONTAINER,
        "/usr/local/bin/faber-mirror",
        "sync",
    ]
    .map(String::from)
    .to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 10_000_000;

    fn healthy_at(t: u64, status: faber_mirror::HealthStatus, reason: Option<&str>) -> Snapshot {
        Snapshot {
            image_present: Some(true),
            last_healthy_ms: Some(t),
            reading_ok: true,
            report: Some(faber_mirror::Health {
                version: "0.3.12".into(),
                status,
                reason: reason.map(String::from),
            }),
            ..Snapshot::default()
        }
    }

    fn has(args: &[String], pair: [&str; 2]) -> bool {
        args.windows(2).any(|w| w[0] == pair[0] && w[1] == pair[1])
    }

    fn down() -> Condition {
        Condition::Down {
            since_ms: Some(1),
            error: Some("probe".into()),
        }
    }

    fn passing(reason: &str) -> Condition {
        Condition::PassingThrough {
            reason: reason.into(),
        }
    }

    /// An ecosystem shaped like ADR-0047 §4's LFS redirect: the token inside
    /// the URL, so the whole value is secret.
    fn lfs_like(ctx: &EcosystemCtx<'_>) -> Result<Vec<EnvVar>, String> {
        let token = ctx.token.map(RepoToken::secret).unwrap_or("none");
        Ok(vec![
            EnvVar {
                name: "GIT_CONFIG_KEY_0".into(),
                value: "lfs.url".into(),
                secret: false,
            },
            EnvVar {
                name: "GIT_CONFIG_VALUE_0".into(),
                value: format!("{}/lfs/{token}/{}", ctx.base_url, ctx.owner_repo),
                secret: true,
            },
        ])
    }

    const LFS_LIKE: &[Ecosystem] = &[Ecosystem {
        name: "lfs",
        on: lfs_on,
        needs_token: true,
        env: lfs_like,
    }];

    fn no_env() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    fn policy(lfs: Option<faber_mirror::Fill>) -> MirrorPolicy {
        MirrorPolicy {
            max_size_gib: 20,
            lfs,
            npm: None,
        }
    }

    // --- condition ---------------------------------------------------------------

    #[test]
    fn a_recent_healthy_reading_is_usable_and_a_stale_one_is_down() {
        use faber_mirror::HealthStatus::*;
        let s = healthy_at(NOW - HEALTHY_WINDOW_MS, Ok, None);
        assert_eq!(condition(&s, NOW), Condition::Healthy);
        let c = condition(&healthy_at(NOW - 1, PassingThrough, Some("disk full")), NOW);
        assert_eq!(c, passing("disk full"));
        assert!(c.usable() && !c.bad());

        let mut stale = healthy_at(NOW - HEALTHY_WINDOW_MS - 1, Ok, None);
        stale.unhealthy_since_ms = Some(NOW - HEALTHY_WINDOW_MS - 1);
        stale.last_error = Some("health probe: exited 1".into());
        let c = condition(&stale, NOW);
        assert_eq!(
            c,
            Condition::Down {
                since_ms: Some(NOW - HEALTHY_WINDOW_MS - 1),
                error: Some("health probe: exited 1".into())
            }
        );
        assert!(!c.usable() && c.bad());
    }

    #[test]
    fn a_failed_reading_stops_jobs_at_once_and_is_reported_after_the_window() {
        // ADR-0047 §7: healthy a second ago, and the probe just failed (or a
        // replace or a foreign container cleared the reading).
        let m = Mirror::default();
        *m.state.lock().unwrap() = healthy_at(now_ms(), faber_mirror::HealthStatus::Ok, None);
        m.note_unhealthy("health probe: exited 1".into());
        let s = m.state.lock().unwrap().clone();
        let c = condition(&s, now_ms());
        assert!(matches!(c, Condition::Faltering { .. }), "{c:?}");
        assert!(!c.usable(), "no job is pointed at it");
        assert!(!c.bad(), "not reported yet");
        assert!(attention(&c, &[]).is_empty());
        let later = condition(&s, now_ms() + HEALTHY_WINDOW_MS);
        assert!(later.bad(), "{later:?}");
        // A good reading makes it usable again.
        m.note_healthy(faber_mirror::Health {
            version: "0.3.12".into(),
            status: faber_mirror::HealthStatus::Ok,
            reason: None,
        });
        assert_eq!(
            condition(&m.state.lock().unwrap(), now_ms()),
            Condition::Healthy
        );
    }

    #[test]
    fn a_mirror_never_healthy_is_starting_for_one_window_then_down() {
        let s = Snapshot {
            unhealthy_since_ms: Some(NOW - HEALTHY_WINDOW_MS + 1),
            ..Snapshot::default()
        };
        assert_eq!(condition(&s, NOW), Condition::Starting);
        assert!(!condition(&s, NOW).usable());
        let s = Snapshot {
            unhealthy_since_ms: Some(NOW - HEALTHY_WINDOW_MS),
            ..Snapshot::default()
        };
        assert!(
            matches!(condition(&s, NOW), Condition::Down { since_ms: Some(t), .. } if t == NOW - HEALTHY_WINDOW_MS)
        );
        assert_eq!(condition(&Snapshot::default(), NOW), Condition::Starting);
    }

    #[test]
    fn a_missing_image_outranks_every_reading() {
        let mut s = healthy_at(NOW, faber_mirror::HealthStatus::Ok, None);
        s.image_present = Some(false);
        s.image_missing_since_ms = Some(5);
        let c = condition(&s, NOW);
        assert_eq!(c, Condition::ImageMissing { since_ms: Some(5) });
        assert!(c.bad() && !c.usable());
    }

    // --- injection -----------------------------------------------------------------

    #[test]
    fn a_runner_is_pointed_at_the_mirror_only_when_opted_in_and_usable() {
        let usable = [Condition::Healthy, passing("x")];
        let not = [
            Condition::Starting,
            Condition::Faltering { since_ms: None },
            Condition::ImageMissing { since_ms: None },
            down(),
        ];
        for c in &usable {
            let inj = plan(true, c, "owner/demo", None, &no_env(), &[]).expect("injected");
            assert_eq!(
                inj.env,
                vec![(ENV.to_string(), "http://faber-mirror:7470".to_string())]
            );
            assert!(inj.secret_env.names().is_empty());
            assert!(
                plan(false, c, "owner/demo", None, &no_env(), ECOSYSTEMS).is_none(),
                "not opted in: {c:?}"
            );
        }
        for c in &not {
            assert!(
                plan(true, c, "owner/demo", None, &no_env(), ECOSYSTEMS).is_none(),
                "{c:?}"
            );
        }
    }

    #[test]
    fn npm_is_on_only_with_its_table_and_its_redirect_is_plain_env() {
        let mut p = policy(None);
        assert!(!(NPM.on)(&p));
        p.npm = Some(faber_mirror::Fill::Off);
        assert!((NPM.on)(&p));
        const { assert!(!NPM.needs_token, "npm carries no token") };
        // No token was minted (LFS off): npm is still redirected, in plain env.
        let inj = plan(
            true,
            &Condition::Healthy,
            "owner/demo",
            None,
            &no_env(),
            &[NPM],
        )
        .unwrap();
        assert_eq!(inj.ecosystems, vec!["npm"]);
        assert!(inj.secret_env.names().is_empty());
        assert_eq!(
            inj.env,
            vec![
                (ENV.to_string(), "http://faber-mirror:7470".to_string()),
                (
                    "NPM_CONFIG_REGISTRY".to_string(),
                    "http://faber-mirror:7470/npm/".to_string()
                ),
                (
                    "YARN_NPM_REGISTRY_SERVER".to_string(),
                    "http://faber-mirror:7470/npm/".to_string()
                ),
                (
                    "YARN_UNSAFE_HTTP_WHITELIST".to_string(),
                    "faber-mirror".to_string()
                ),
            ]
        );
        // Both on, the token minted: LFS's secret and npm's plain variables.
        let inj = plan(
            true,
            &Condition::Healthy,
            "owner/demo",
            Some(RepoToken::new("t0ken".into())),
            &no_env(),
            ECOSYSTEMS,
        )
        .unwrap();
        assert_eq!(inj.ecosystems, vec!["lfs", "npm"]);
        assert!(inj.env.iter().all(|(_, v)| !v.contains("t0ken")));
        // Off, or not opted in: nothing, as before.
        assert!(plan(
            false,
            &Condition::Healthy,
            "owner/demo",
            None,
            &no_env(),
            &[NPM]
        )
        .is_none());
    }

    #[test]
    fn lfs_is_on_only_with_its_table() {
        assert!(!(LFS.on)(&policy(None)));
        assert!((LFS.on)(&policy(Some(faber_mirror::Fill::Relaxed))));
        assert!((LFS.on)(&policy(Some(faber_mirror::Fill::Off))));
        assert_eq!(
            ECOSYSTEMS.iter().map(|e| e.name).collect::<Vec<_>>(),
            vec!["lfs", "npm"]
        );
    }

    #[test]
    fn the_lfs_redirect_is_secret_env_pointing_downloads_at_the_mirror_and_pushes_at_github() {
        let token = RepoToken::new("t0ken".into());
        let inj = plan(
            true,
            &Condition::Healthy,
            "owner/demo",
            Some(token),
            &no_env(),
            &[LFS],
        )
        .unwrap();
        assert_eq!(inj.ecosystems, vec!["lfs"]);
        assert_eq!(
            inj.env,
            vec![(ENV.to_string(), "http://faber-mirror:7470".to_string())],
            "nothing but FABER_MIRROR is plain"
        );
        assert_eq!(
            inj.secret_env.pairs(),
            vec![
                ("GIT_CONFIG_COUNT", "2"),
                ("GIT_CONFIG_KEY_0", "lfs.url"),
                (
                    "GIT_CONFIG_VALUE_0",
                    "http://faber-mirror:7470/lfs/t0ken/owner/demo/info/lfs"
                ),
                ("GIT_CONFIG_KEY_1", "lfs.pushurl"),
                (
                    "GIT_CONFIG_VALUE_1",
                    "https://github.com/owner/demo.git/info/lfs"
                ),
            ]
        );
    }

    #[test]
    fn a_profile_git_config_count_is_appended_to_and_a_bad_one_skips_lfs() {
        let mut env = no_env();
        env.insert(GIT_CONFIG_COUNT.into(), "3".into());
        let token = RepoToken::new("t".into());
        let inj = plan(
            true,
            &Condition::Healthy,
            "owner/demo",
            Some(token),
            &env,
            &[LFS],
        )
        .unwrap();
        let names = inj.secret_env.names();
        assert_eq!(
            names,
            vec![
                "GIT_CONFIG_COUNT",
                "GIT_CONFIG_KEY_3",
                "GIT_CONFIG_VALUE_3",
                "GIT_CONFIG_KEY_4",
                "GIT_CONFIG_VALUE_4"
            ]
        );
        assert_eq!(inj.secret_env.pairs()[0], ("GIT_CONFIG_COUNT", "5"));

        for bad in ["three", "-1", "100000", ""] {
            env.insert(GIT_CONFIG_COUNT.into(), bad.into());
            let inj = plan(
                true,
                &Condition::Healthy,
                "owner/demo",
                Some(RepoToken::new("t".into())),
                &env,
                &[LFS],
            )
            .unwrap();
            assert!(inj.ecosystems.is_empty(), "{bad:?}");
            assert!(inj.secret_env.names().is_empty(), "{bad:?}");
            assert_eq!(inj.skipped.len(), 1);
            assert!(inj.skipped[0].1.contains("GIT_CONFIG_COUNT"), "{bad:?}");
            assert_eq!(inj.env.len(), 1, "FABER_MIRROR still");
        }
        // No token, no LFS: the job still gets FABER_MIRROR.
        let inj = plan(
            true,
            &Condition::Healthy,
            "owner/demo",
            None,
            &no_env(),
            &[LFS],
        )
        .unwrap();
        assert!(inj.ecosystems.is_empty() && inj.skipped.len() == 1);
    }

    #[test]
    fn low_disk_is_the_admission_floor_plus_five_gib() {
        assert!(low_disk(Some(10), Some(14 * 1024)));
        assert!(!low_disk(Some(10), Some(15 * 1024)));
        assert!(low_disk(None, Some(4 * 1024)), "no floor: the margin alone");
        assert!(!low_disk(None, Some(5 * 1024)));
        assert!(!low_disk(Some(10), None), "no fresh reading is no evidence");
    }

    #[test]
    fn the_credential_fingerprint_names_no_token_and_tracks_a_change() {
        assert_eq!(credential_fingerprint(false, "abc"), None);
        assert_eq!(
            credential_fingerprint(true, "").as_deref(),
            Some("anonymous")
        );
        let a = credential_fingerprint(true, "github_pat_one").unwrap();
        let b = credential_fingerprint(true, "github_pat_two").unwrap();
        assert_eq!(a.len(), 16);
        assert_ne!(a, b);
        assert!(!a.contains("github_pat"));
    }

    #[test]
    fn an_ecosystem_gets_the_repo_and_token_and_its_secret_stays_out_of_the_plain_env() {
        let token = RepoToken::new("s3cret-token".into());
        let inj = plan(
            true,
            &Condition::Healthy,
            "owner/demo",
            Some(token),
            &no_env(),
            LFS_LIKE,
        )
        .unwrap();
        assert_eq!(
            inj.env,
            vec![
                (ENV.to_string(), "http://faber-mirror:7470".to_string()),
                ("GIT_CONFIG_KEY_0".to_string(), "lfs.url".to_string()),
            ]
        );
        assert!(inj.env.iter().all(|(_, v)| !v.contains("s3cret-token")));
        assert_eq!(inj.secret_env.names(), vec!["GIT_CONFIG_VALUE_0"]);
        assert_eq!(
            inj.secret_env.pairs(),
            vec![(
                "GIT_CONFIG_VALUE_0",
                "http://faber-mirror:7470/lfs/s3cret-token/owner/demo"
            )]
        );
    }

    #[test]
    fn each_container_gets_its_own_url_safe_token() {
        let a = RepoToken::mint().unwrap();
        let b = RepoToken::mint().unwrap();
        assert_eq!(a.secret().len(), 32);
        assert!(a.secret().bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a.secret(), b.secret());
    }

    // --- transitions and rows ---------------------------------------------------------

    #[test]
    fn each_change_of_level_earns_one_fault_line_and_no_poll_repeats_it() {
        let missing = Condition::ImageMissing { since_ms: None };
        let (a, cause, l) = transition(Level::Ok, &down()).unwrap();
        assert_eq!((a, l), ("failed", Level::Bad));
        assert!(cause.unwrap().contains("probe"));
        assert_eq!(transition(Level::Ok, &missing).unwrap().0, "failed");
        assert_eq!(
            transition(Level::Bad, &Condition::Healthy),
            Some(("recovered", None, Level::Ok))
        );
        // Passing through is its own line, with the reason, and so is leaving it.
        assert_eq!(
            transition(Level::Ok, &passing("cache full")),
            Some((
                "passing_through",
                Some("cache full".into()),
                Level::PassingThrough
            ))
        );
        assert_eq!(
            transition(Level::Bad, &passing("r")).map(|t| t.0),
            Some("passing_through")
        );
        assert_eq!(
            transition(Level::PassingThrough, &Condition::Healthy),
            Some(("recovered", None, Level::Ok))
        );
        assert_eq!(
            transition(Level::PassingThrough, &down()).map(|t| t.0),
            Some("failed")
        );
        // The same level again, startup and faltering earn nothing.
        assert_eq!(transition(Level::Bad, &missing), None);
        assert_eq!(transition(Level::Bad, &down()), None);
        assert_eq!(transition(Level::PassingThrough, &passing("other")), None);
        assert_eq!(transition(Level::Ok, &Condition::Healthy), None);
        assert_eq!(transition(Level::Ok, &Condition::Starting), None);
        assert_eq!(
            transition(
                Level::PassingThrough,
                &Condition::Faltering { since_ms: None }
            ),
            None
        );
    }

    /// A restart starts from the level the audit log left, so a mirror healthy by
    /// the time the new process looks still writes `recovered`.
    #[test]
    fn a_restart_resumes_the_mirror_level_the_audit_log_left() {
        let line = |ts_ms: u64, repo: &str, action: &str| -> crate::audit::Entry {
            serde_json::from_value(serde_json::json!({
                "ts_ms": ts_ms, "kind": "fault", "repo": repo, "job_id": 0,
                "result": "mirror", "action": action,
            }))
            .expect("an audit line")
        };
        assert_eq!(Level::replayed(&[]), Level::Ok);
        let passing = [line(1, "-", "passing_through")];
        assert_eq!(Level::replayed(&passing), Level::PassingThrough);
        assert_eq!(
            transition(Level::replayed(&passing), &Condition::Healthy).map(|t| t.0),
            Some("recovered")
        );
        assert_eq!(
            Level::replayed(&[
                line(1, "-", "failed"),
                line(2, "owner/demo", "request_failed")
            ]),
            Level::Bad,
            "a per-repo request failure is not the mirror's level"
        );
        assert_eq!(
            Level::replayed(&[line(1, "-", "failed"), line(2, "-", "recovered")]),
            Level::Ok
        );
    }

    #[test]
    fn each_condition_earns_its_row() {
        let rows = attention(
            &Condition::Down {
                since_ms: Some(42),
                error: Some("health probe: exited 1".into()),
            },
            &[],
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(
            (
                rows[0].key,
                rows[0].severity,
                rows[0].title,
                rows[0].since_ms
            ),
            ("mirror-down", "bad", "Mirror down", Some(42))
        );
        assert_eq!(rows[0].error.as_deref(), Some("health probe: exited 1"));
        assert!(!rows[0].detail.contains("exited 1"), "the error is a field");
        assert!(rows[0].affected.is_empty());

        // The down row carries the jobs it affected as data, never in the prose.
        let affected: Vec<String> = (1..=7).map(|n| format!("faber-runner-{n}")).collect();
        let rows = attention(&down(), &affected);
        assert_eq!(rows[0].affected, affected);
        assert!(
            !rows[0].detail.contains("faber-runner"),
            "{}",
            rows[0].detail
        );
        let json = serde_json::to_value(&rows[0]).unwrap();
        assert_eq!(json["affected"][6], "faber-runner-7");

        let rows = attention(&passing("cache volume is full"), &[]);
        assert_eq!(
            (rows[0].key, rows[0].severity, rows[0].title),
            ("mirror-passing", "warn", "Mirror passing through")
        );
        assert!(rows[0].detail.starts_with("cache volume is full"));

        let rows = attention(&Condition::ImageMissing { since_ms: Some(7) }, &[]);
        assert_eq!(
            (rows[0].key, rows[0].severity, rows[0].title),
            ("mirror-image", "bad", "Mirror image missing")
        );
        assert_eq!(rows[0].image.as_deref(), Some(image().as_str()));
        assert_eq!(rows[0].command.as_deref(), Some(BUILD_COMMAND));
        assert!(
            !rows[0].detail.contains("faber-mirror:"),
            "the image is a field"
        );

        assert!(attention(&Condition::Healthy, &[]).is_empty());
        assert!(attention(&Condition::Starting, &[]).is_empty());
        assert!(attention(&Condition::Faltering { since_ms: Some(1) }, &[]).is_empty());
    }

    #[test]
    fn the_state_json_carries_the_rows_and_the_state_word() {
        let policy = policy(None);
        let mut s = healthy_at(
            NOW - HEALTHY_WINDOW_MS - 1,
            faber_mirror::HealthStatus::Ok,
            None,
        );
        s.unhealthy_since_ms = Some(NOW - 90_000);
        let v = serde_json::to_value(view_of(&policy, &s, 2, &[], None, NOW)).unwrap();
        assert_eq!(v["state"], "down");
        assert_eq!(v["max_size_gib"], 20);
        assert_eq!(v["attached"], 2);
        assert_eq!(v["version"], "0.3.12");
        assert_eq!(v["image"], image());
        assert_eq!(v["attention"][0]["key"], "mirror-down");
        assert_eq!(v["attention"][0]["since_ms"], NOW - 90_000);

        let ok = serde_json::to_value(view_of(
            &policy,
            &healthy_at(NOW, faber_mirror::HealthStatus::Ok, None),
            0,
            &[],
            None,
            NOW,
        ))
        .unwrap();
        assert_eq!(ok["state"], "ok");
        assert_eq!(ok["attention"], serde_json::json!([]));
        assert!(
            ok.get("lfs").is_none(),
            "no lfs block with [mirror.lfs] absent"
        );
    }

    #[test]
    fn the_lfs_counters_reach_the_state_json_busiest_repo_first() {
        let mut stats = faber_mirror::Stats {
            store: faber_mirror::StoreStats {
                objects: 5,
                bytes: 350,
                max_bytes: 1000,
                in_flight: 0,
                usage: [
                    (
                        "lfs".to_string(),
                        faber_mirror::Usage {
                            objects: 3,
                            bytes: 300,
                        },
                    ),
                    (
                        "npm".to_string(),
                        faber_mirror::Usage {
                            objects: 2,
                            bytes: 50,
                        },
                    ),
                ]
                .into(),
            },
            ..Default::default()
        };
        stats.repos.insert(
            "owner/small".into(),
            faber_mirror::RepoCounters {
                hits: 1,
                bytes_from_cache: 10,
                ..Default::default()
            },
        );
        stats.repos.insert(
            "owner/demo".into(),
            faber_mirror::RepoCounters {
                hits: 4,
                misses: 2,
                bytes_from_cache: 400,
                bytes_from_upstream: 200,
                passed_through: 1,
            },
        );
        let v = serde_json::to_value(view_of(
            &policy(Some(faber_mirror::Fill::Relaxed)),
            &healthy_at(NOW, faber_mirror::HealthStatus::Ok, None),
            1,
            &[],
            Some(&stats),
            NOW,
        ))
        .unwrap();
        assert_eq!(v["lfs"]["fill"], "relaxed");
        assert_eq!(v["lfs"]["bytes"], 300);
        assert_eq!(v["lfs"]["max_bytes"], 1000);
        assert_eq!(v["lfs"]["repos"][0]["repo"], "owner/demo");
        assert_eq!(v["lfs"]["repos"][0]["bytes_from_cache"], 400);
        assert_eq!(v["lfs"]["repos"][0]["bytes_from_upstream"], 200);
        assert_eq!(v["lfs"]["repos"][0]["hits"], 4);
        assert_eq!(v["lfs"]["repos"][0]["misses"], 2);
        assert_eq!(v["lfs"]["repos"][1]["repo"], "owner/small");
        assert!(
            v.get("npm").is_none(),
            "no npm block with [mirror.npm] absent"
        );
    }

    #[test]
    fn the_npm_counters_reach_the_state_json_with_their_share_of_the_store() {
        let mut stats = faber_mirror::Stats {
            store: faber_mirror::StoreStats {
                objects: 5,
                bytes: 350,
                max_bytes: 1000,
                usage: [(
                    "npm".to_string(),
                    faber_mirror::Usage {
                        objects: 2,
                        bytes: 50,
                    },
                )]
                .into(),
                ..Default::default()
            },
            ..Default::default()
        };
        stats.npm.insert(
            "owner/demo".into(),
            faber_mirror::RepoCounters {
                hits: 9,
                misses: 3,
                bytes_from_cache: 900,
                bytes_from_upstream: 30,
                passed_through: 1,
            },
        );
        stats.npm.insert(
            faber_mirror::UNATTRIBUTED.into(),
            faber_mirror::RepoCounters {
                misses: 1,
                bytes_from_upstream: 5,
                ..Default::default()
            },
        );
        let mut p = policy(None);
        p.npm = Some(faber_mirror::Fill::Off);
        let v = serde_json::to_value(view_of(
            &p,
            &healthy_at(NOW, faber_mirror::HealthStatus::Ok, None),
            1,
            &[],
            Some(&stats),
            NOW,
        ))
        .unwrap();
        assert!(v.get("lfs").is_none());
        assert_eq!(v["npm"]["fill"], "off");
        assert_eq!(v["npm"]["objects"], 2);
        assert_eq!(v["npm"]["bytes"], 50);
        assert_eq!(v["npm"]["max_bytes"], 1000);
        assert_eq!(v["npm"]["repos"][0]["repo"], "owner/demo");
        assert_eq!(v["npm"]["repos"][0]["hits"], 9);
        assert_eq!(v["npm"]["repos"][1]["repo"], "(unattributed)");
    }

    fn job(runner: &str, token: Token) -> Job {
        Job {
            repo: "owner/demo".into(),
            runner: runner.into(),
            redirected: token != Token::None,
            token,
            incident: None,
            seen_failures: 0,
        }
    }

    #[test]
    fn a_sync_names_the_mirrors_address_on_each_redirected_jobs_network() {
        let m = Mirror::default();
        {
            let mut jobs = m.jobs.lock().unwrap();
            // npm only: no token, but the service must know its address.
            let mut npm_only = job("r1", Token::None);
            npm_only.redirected = true;
            jobs.insert("faber-net-1".into(), npm_only);
            jobs.insert("faber-net-2".into(), job("r2", Token::Minted("tok".into())));
            // Pointed at the mirror for no ecosystem: the service has nothing of it.
            jobs.insert("faber-net-3".into(), job("r3", Token::None));
        }
        let addrs = parse_addrs(&[
            "faber-net-1 172.20.0.3".to_string(),
            "faber-net-2 172.21.0.3".to_string(),
            "bridge 172.17.0.2".to_string(),
            "faber-net-9 not-an-ip".to_string(),
            "faber-net-8 ".to_string(),
        ]);
        assert_eq!(addrs.len(), 2);
        let mut p = policy(Some(faber_mirror::Fill::Relaxed));
        p.npm = Some(faber_mirror::Fill::Relaxed);
        let req = m.sync_request(&p, false, &addrs);
        let mut entries: Vec<(String, Option<String>, Option<String>)> = req
            .entries
            .iter()
            .map(|e| (e.id.clone(), e.token.clone(), e.addr.clone()))
            .collect();
        entries.sort();
        assert_eq!(
            entries,
            vec![
                ("faber-net-1".into(), None, Some("172.20.0.3".into())),
                (
                    "faber-net-2".into(),
                    Some("tok".into()),
                    Some("172.21.0.3".into())
                ),
            ]
        );
        assert_eq!(req.npm, Some(faber_mirror::Fill::Relaxed));
        assert!(has(&addrs_args(), ["inspect", "--format"]));
    }

    #[test]
    fn a_sync_sends_minted_and_adopted_tokens_and_no_others() {
        let m = Mirror::default();
        {
            let mut jobs = m.jobs.lock().unwrap();
            jobs.insert("faber-net-1".into(), job("r1", Token::Minted("tok".into())));
            jobs.insert("faber-net-2".into(), job("r2", Token::Adopted));
            jobs.insert("faber-net-3".into(), job("r3", Token::None));
        }
        m.low_disk.store(true, Ordering::Relaxed);
        let req = m.sync_request(
            &policy(Some(faber_mirror::Fill::Relaxed)),
            true,
            &HashMap::new(),
        );
        let mut entries: Vec<(String, Option<String>)> = req
            .entries
            .iter()
            .map(|e| (e.id.clone(), e.token.clone()))
            .collect();
        entries.sort();
        assert_eq!(
            entries,
            vec![
                ("faber-net-1".to_string(), Some("tok".to_string())),
                ("faber-net-2".to_string(), None),
            ]
        );
        assert_eq!(req.lfs, Some(faber_mirror::Fill::Relaxed));
        assert!(req.low_disk && req.adopt);
        assert_eq!(req.max_size_bytes, Some(20 << 30));
    }

    #[test]
    fn a_sync_answer_adopts_marks_failures_and_restarts_as_incidents() {
        let m = Mirror::default();
        m.jobs
            .lock()
            .unwrap()
            .insert("faber-net-1".into(), job("r1", Token::Minted("t".into())));
        let mut st = SyncState::default();
        let mut stats = faber_mirror::Stats {
            instance: "a".into(),
            ids: vec![
                "faber-net-1".into(),
                "faber-net-old".into(),
                "not-ours".into(),
            ],
            ..Default::default()
        };
        m.absorb(&mut st, &stats);
        {
            let jobs = m.jobs.lock().unwrap();
            assert!(jobs
                .get("faber-net-old")
                .is_some_and(|j| j.token == Token::Adopted));
            assert!(!jobs.contains_key("not-ours"));
            assert!(jobs["faber-net-1"].incident.is_none());
        }
        assert!(st.adopted);

        // A failure the service counted becomes the job's incident and one
        // audit line; the same count again adds nothing.
        stats.failures.insert(
            "faber-net-1".into(),
            faber_mirror::JobFailures {
                count: 1,
                last: "reading a stored object: I/O error".into(),
            },
        );
        m.absorb(&mut st, &stats);
        m.absorb(&mut st, &stats);
        assert!(m.jobs.lock().unwrap()["faber-net-1"]
            .incident
            .as_deref()
            .unwrap()
            .contains("I/O error"));
        let faults = m.take_faults();
        assert_eq!(faults.len(), 1);
        assert!(faults[0].1.starts_with("r1: "));

        // A new instance is a restart: every job with a token is affected.
        stats.instance = "b".into();
        stats.failures.clear();
        m.absorb(&mut st, &stats);
        assert!(m.jobs.lock().unwrap()["faber-net-old"]
            .incident
            .as_deref()
            .unwrap()
            .contains("restarted"));
    }

    #[test]
    fn an_outage_names_every_job_it_affected_and_recovery_clears_them() {
        let m = Mirror::default();
        m.jobs
            .lock()
            .unwrap()
            .insert("faber-net-2".into(), job("faber-runner-b", Token::None));
        m.jobs
            .lock()
            .unwrap()
            .insert("faber-net-1".into(), job("faber-runner-a", Token::None));
        let names = m.note_outage("the mirror was down");
        assert_eq!(names, vec!["faber-runner-a", "faber-runner-b"]);
        assert!(
            m.take_faults().is_empty(),
            "one outage line, not one per job"
        );
        assert_eq!(m.affected.lock().unwrap().len(), 2);
        assert!(m.jobs.lock().unwrap()["faber-net-1"].incident.is_some());
    }

    #[test]
    fn with_the_feature_off_and_no_lingering_container_nothing_calls_docker() {
        let m = Mirror::default();
        assert!(!m.enabled());
        assert!(m.view().is_none());
        assert!(m.condition().is_none());
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        // Returns before any Docker call: there is no daemon in a unit test, and
        // a call would fail or hang rather than return this quickly.
        assert!(rt
            .block_on(m.attach(true, "faber-net-1-2", "owner/demo", "r", &no_env()))
            .is_none());
        assert!(rt.block_on(m.release_network("faber-net-1-2")).is_none());
        assert!(rt.block_on(m.nets.lock()).released.is_empty());
    }

    #[test]
    fn with_the_feature_off_a_lingering_container_is_still_released() {
        // ADR-0046's precedent: a labelled container left from a config with
        // `[mirror]` may sit on an adopted job's network, so teardown disconnects
        // it whatever the config says. Only names from the labelled listing.
        assert_eq!(
            lingering_names(&["faber-mirror".into(), "bad name;rm".into(), " ".into()]),
            vec!["faber-mirror"]
        );
        let lingering = vec!["faber-mirror".to_string()];
        assert_eq!(release_targets(false, &lingering), lingering);
        assert!(release_targets(false, &[]).is_empty());
        assert_eq!(release_targets(true, &[]), vec![CONTAINER]);
        let ls = labelled_ls_args();
        assert!(has(&ls, ["--filter", "label=org.faber.owned=true"]));
        assert!(has(&ls, ["--filter", "label=org.faber.kind=mirror"]));
        assert!(
            !ls.iter().any(|a| a.contains("name=")),
            "labels, never names"
        );
    }

    #[test]
    fn a_replace_never_rejoins_a_network_teardown_released() {
        let mut nets = Nets::default();
        nets.attached.insert("faber-net-1-1".into());
        nets.attached.insert("faber-net-2-2".into());
        nets.release("faber-net-2-2");
        assert!(!nets.attached.contains("faber-net-2-2"));
        let listed = vec![
            "faber-net-3-3".to_string(),
            "faber-net-2-2".to_string(),
            "faber-net-1-1".to_string(),
        ];
        let mut all = listed;
        all.extend(nets.attached.iter().cloned());
        assert_eq!(
            reconnect_candidates(all, &nets.released),
            vec!["faber-net-1-1", "faber-net-3-3"]
        );
    }

    #[test]
    fn the_released_set_is_bounded() {
        let mut nets = Nets::default();
        for i in 0..=RELEASED_CAP {
            nets.release(&format!("faber-net-{i}-1"));
        }
        assert!(nets.released.len() <= RELEASED_CAP);
        assert!(nets
            .released
            .contains(&format!("faber-net-{RELEASED_CAP}-1")));
    }

    // --- the container ---------------------------------------------------------------

    #[test]
    fn the_janitor_adopts_only_its_own_container_running_the_current_image() {
        let want = "sha256:aaaa";
        let present = |running, image_id: &str, kind: Option<&str>| Found::Present {
            running,
            image_id: image_id.into(),
            kind: kind.map(String::from),
            credential: None,
        };
        assert_eq!(decide(&Found::Absent, want, None), Step::Create);
        assert_eq!(
            decide(&present(true, want, Some("mirror")), want, None),
            Step::Keep
        );
        assert_eq!(
            decide(&present(false, want, Some("mirror")), want, None),
            Step::Replace("not running")
        );
        // A rebuild under the same tag is a new ID: replaced, though the tag
        // (one version between releases) never changed.
        assert_eq!(
            decide(&present(true, "sha256:bbbb", Some("mirror")), want, None),
            Step::Replace("another image")
        );
        // The name alone is not provenance (AGENTS.md: read labels, never names).
        assert_eq!(
            decide(&present(true, want, None), want, None),
            Step::Foreign
        );
        assert_eq!(
            decide(&present(false, want, Some("runner")), want, None),
            Step::Foreign
        );
        // LFS turned on, or faber's token changed: the container is started
        // again with the credential it now needs.
        assert_eq!(
            decide(&present(true, want, Some("mirror")), want, Some("abcd")),
            Step::Replace("its GitHub credential changed")
        );
        let with = Found::Present {
            running: true,
            image_id: want.into(),
            kind: Some("mirror".into()),
            credential: Some("abcd".into()),
        };
        assert_eq!(decide(&with, want, Some("abcd")), Step::Keep);
        assert_eq!(
            decide(&with, want, None),
            Step::Replace("its GitHub credential changed")
        );
    }

    #[test]
    fn inspect_output_is_parsed_strictly() {
        assert_eq!(
            parse_inspect(r#"{"Running":true,"ImageId":"sha256:aa","Kind":"mirror"}"#),
            Some(Found::Present {
                running: true,
                image_id: "sha256:aa".into(),
                kind: Some("mirror".into()),
                credential: None,
            })
        );
        assert_eq!(
            parse_inspect(
                r#"{"Running":true,"ImageId":"sha256:aa","Kind":"mirror","Credential":"ab12"}"#
            ),
            Some(Found::Present {
                running: true,
                image_id: "sha256:aa".into(),
                kind: Some("mirror".into()),
                credential: Some("ab12".into()),
            })
        );
        assert_eq!(
            parse_inspect(r#"{"Running":false,"ImageId":"x","Kind":""}"#),
            Some(Found::Present {
                running: false,
                image_id: "x".into(),
                kind: None,
                credential: None,
            })
        );
        assert_eq!(parse_inspect("garbage"), None);
        assert_eq!(parse_inspect("{}\n{}"), None);
        assert!(
            INSPECT_FORMAT.contains("{{json .Image}}"),
            "the ID, not .Config.Image"
        );
    }

    #[test]
    fn the_mirror_runs_labelled_detached_restarting_and_never_pulled() {
        let a = run_args(
            &MirrorPolicy {
                max_size_gib: 7,
                lfs: None,
                npm: None,
            },
            "faber-mirror:0.3.12",
            None,
        );
        assert_eq!(&a[..3], &["run", "-d", "--pull=never"]);
        assert!(has(&a, ["--name", CONTAINER]));
        assert!(has(&a, ["--restart", "unless-stopped"]));
        assert!(has(&a, ["--label", "org.faber.owned=true"]));
        assert!(has(&a, ["--label", "org.faber.kind=mirror"]));
        assert!(has(&a, ["-v", "faber-mirror-cache:/cache"]));
        assert!(has(&a, ["-e", "FABER_MIRROR_MAX_SIZE_GIB=7"]));
        assert_eq!(&a[a.len() - 2..], &["faber-mirror:0.3.12", "serve"]);
        // No repo, no claim: nothing reads the mirror as a job's.
        assert!(!a
            .iter()
            .any(|x| x.starts_with("org.faber.repo") || x.starts_with("org.faber.id")));

        let v = volume_create_args();
        assert_eq!(&v[..2], &["volume", "create"]);
        assert!(has(&v, ["--label", "org.faber.kind=mirror"]));
        assert_eq!(v.last().unwrap(), VOLUME);
        assert!(
            !a.iter().any(|x| x.contains(AUTH_VOLUME)),
            "no LFS, no credential"
        );
    }

    #[test]
    fn with_lfs_the_credential_is_mounted_read_only_and_fingerprinted() {
        let policy = policy(Some(faber_mirror::Fill::Relaxed));
        let cred = Credential {
            fingerprint: "ab12".into(),
            mounted: true,
        };
        let a = run_args(&policy, "faber-mirror:0.3.12", Some(&cred));
        assert!(has(&a, ["-v", "faber-mirror-auth:/faber/auth:ro"]));
        assert!(has(&a, ["--label", "org.faber.credential=ab12"]));
        // Anonymous: the label, no mount.
        let anon = Credential {
            fingerprint: "anonymous".into(),
            mounted: false,
        };
        let a = run_args(&policy, "faber-mirror:0.3.12", Some(&anon));
        assert!(has(&a, ["--label", "org.faber.credential=anonymous"]));
        assert!(!a.iter().any(|x| x.contains(AUTH_VOLUME)));

        // The credential volume is found by its labels, never its name.
        let ls = auth_ls_args();
        assert!(has(&ls, ["--filter", "label=org.faber.owned=true"]));
        assert!(has(&ls, ["--filter", "label=org.faber.kind=mirror-auth"]));
        assert!(has(
            &auth_marks(),
            ["--label", "org.faber.kind=mirror-auth"]
        ));
    }

    #[test]
    fn a_sync_carries_its_request_on_stdin_never_the_argv() {
        let args = sync_args();
        assert_eq!(
            args,
            [
                "exec",
                "-i",
                CONTAINER,
                "/usr/local/bin/faber-mirror",
                "sync"
            ]
        );
    }

    #[test]
    fn a_job_network_gets_the_mirror_under_its_alias() {
        assert_eq!(
            connect_args("faber-net-1-2"),
            [
                "network",
                "connect",
                "--alias",
                "faber-mirror",
                "faber-net-1-2",
                CONTAINER
            ]
        );
        assert_eq!(
            disconnect_args("faber-net-1-2", CONTAINER),
            ["network", "disconnect", "-f", "faber-net-1-2", CONTAINER]
        );
        assert_eq!(networks_args("x").last().unwrap(), "x");
        assert_eq!(probe_args()[..2], ["exec", CONTAINER]);
        assert_eq!(
            image(),
            format!("faber-mirror:{}", env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn only_job_networks_are_reconnected_after_a_replace() {
        let lines: Vec<String> = [
            "bridge",
            "faber-net-1-2",
            "faber-svc",
            "faber-net-$(x)",
            " faber-net-3-4 ",
        ]
        .map(String::from)
        .to_vec();
        assert_eq!(job_networks(&lines), vec!["faber-net-1-2", "faber-net-3-4"]);
    }

    #[test]
    fn a_health_report_is_parsed_and_its_text_made_safe() {
        let r = parse_report(
            "{\"version\":\"0.3.12\",\"status\":\"passing_through\",\"reason\":\"a\\u001bb\"}\n",
        )
        .unwrap();
        assert_eq!(r.status, faber_mirror::HealthStatus::PassingThrough);
        assert!(!r.reason.unwrap().contains('\u{1b}'));
        assert!(parse_report("not json").is_err());
        assert!(parse_report("{\"status\":\"ok\"}").is_err());
    }

    #[test]
    fn not_found_errors_are_recognised() {
        assert!(no_such(
            "docker exited 1: Error: No such object: faber-mirror"
        ));
        assert!(no_such(
            "docker exited 1: Error response from daemon: No such image: x"
        ));
        assert!(!no_such(
            "docker exited 1: Cannot connect to the Docker daemon"
        ));
    }
}
