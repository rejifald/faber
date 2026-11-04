//! Fleet autoupdate (ADR-0006): a designated builder Mac cuts a release, and
//! every other opted-in host in the fleet quietly catches up on its own.
//!
//! The shape: poll the release repo's GitHub releases, verify a downloaded
//! candidate's SHA-256 against its published `SHA256SUMS`, stage it on disk,
//! wait for the host to go idle (or escalate to a drain if it never does),
//! swap the running binary, and `exit(0)` — launchd's `KeepAlive` restarts
//! the process on the new bytes. A boot-probation marker written just before
//! the swap lets the very next boot detect a crash-loop and roll itself back
//! automatically, with no operator required to notice or intervene.
//!
//! # Pure core, I/O shell
//!
//! [`decide`] is the entire *sequencing* decision — what to do next, given
//! the current state, the policy, and how busy the host is right now — and it
//! touches no clock, no filesystem, and no network: every timestamp arrives
//! as a parameter. That is what makes the interesting bug class here (an
//! off-by-one escalate window, a quarantined version sneaking back in, an
//! `escalate_after_secs == 0` that should never fire) exhaustively testable
//! without a Tokio runtime, a temp dir, or a mock HTTP server. [`Updater::run_loop`]
//! is the thin shell around it: call `decide`, match on the [`Action`] it
//! returns, do exactly that I/O, repeat. No sequencing decision is made
//! anywhere in the async half of this file — if it *looks* like a branch is
//! deciding "what to do next" inside `do_check`/`do_stage`/`do_commit`, it is
//! actually interpreting the *result* of I/O that already happened (a
//! checksum mismatch, a non-zero exit, a 404), which `decide` could not have
//! known in advance.
//!
//! One input can go stale inside a single action: an escalated install awaits a
//! drain that can take hours, and the operator may change the policy or skip
//! the version meanwhile. So `do_commit` asks [`staged_stop`], which is pure
//! like `decide`, what the operator's live choices say before it installs
//! anything.
//!
//! # Filesystem layout
//!
//! Root is [`crate::platform::state_dir`] (see [`UpdatePaths`]): under LEGACY
//! `$HOME/Library/Application Support/faber`, and under SYSTEM (ADR-0025) the
//! fixed `D` = `/Library/Application Support/faber` (`/var/lib/faber` on
//! Linux), so the swap target is `D/bin/faber`, the `_faber`-owned daemon copy
//! the service runs ([`crate::platform::daemon_bin_path`]). Root's own CLI copy
//! in `/usr/local/bin` is never touched from here. `bin/faber` already exists —
//! it is what the installer put there and what the service runs; nothing here
//! ever changes that path, only what it points at.
//!
//! ```text
//! $ROOT/bin/faber                   running binary (never renamed)
//! $ROOT/bin/faber.prev              previous binary, restored on rollback
//! $ROOT/update/staged/faber         verified daemon download, awaiting swap
//! $ROOT/update/staged/faber-app.tar.gz
//! $LAUNCHER/faber-vm                VM launcher (ADR-0026), macOS only; swapped with the daemon
//! $LAUNCHER/faber-vm.prev           previous launcher, restored with faber.prev on rollback
//! $LAUNCHER/faber-vm.staged         verified launcher download, awaiting swap
//! $ROOT/bin/faber-agent            guest agent for the tools disk (ADR-0026), macOS only
//! $ROOT/bin/faber-agent.prev       previous agent, restored with faber.prev on rollback
//! $ROOT/update/staged/faber-agent  verified agent download, awaiting swap
//! $ROOT/update/config.prev.toml     config snapshot paired with faber.prev
//! $ROOT/update/marker.json          boot-probation marker
//! $ROOT/update/clean-exit           one-shot "last exit was deliberate" flag
//! $ROOT/update/quarantine.json      versions that failed probation
//! $ROOT/update/rolled_back.json     one-shot breadcrumb for the boot after a restore
//! ```
//!
//! `$LAUNCHER` is the launcher's own directory ([`UpdatePaths::launcher_dir`]):
//! `$ROOT/bin` under LEGACY, and under SYSTEM the parent of
//! [`SystemLayout::launcher_bin`], `/var/db/faber/bin` on macOS, outside `D`
//! because `_faber_vm` executes the launcher and cannot traverse `D`. The
//! staged copy, the `.prev` and the live launcher share that one directory, so
//! every rename between them stays in it and stays atomic.
//!
//! [`SystemLayout::launcher_bin`]: crate::platform::layout::SystemLayout::launcher_bin
//!
//! # The launcher asset (ADR-0026)
//!
//! On macOS a release also carries `faber-launcher-<target>`, the `faber-vm`
//! launcher. It is staged and verified with the daemon (digest against the
//! attested `SHA256SUMS`, then `faber-vm --version` bound to the same tag and
//! commit), installed *before* the daemon swap so that swap stays the single
//! point of no return, restored with the daemon on rollback, and proven again
//! by [`prove_launcher`] during boot probation.
//!
//! # The guest agent asset (ADR-0026)
//!
//! Beside the launcher a macOS release carries `faber-agent-<target>`, the
//! guest agent that goes onto each per-version tools disk. It is a companion of
//! the launcher in every respect: digest against the attested `SHA256SUMS`,
//! `faber-agent --version` bound to the same tag and commit (the line also
//! carries `protocol=<n>`, see [`check_reported_agent_version`]), installed
//! before the daemon swap and restored with it. The daemon never runs it; it
//! only reads [`UpdatePaths::agent`] when it builds a tools disk, so the bytes
//! on a disk are always the verified release's.
//!
//! # Talking to the rest of the daemon
//!
//! The updater reaches into the orchestrator through `view()` (to read
//! `inflight`), `begin_drain()`, `await_drain()`, `end_drain()` and
//! `adoptable()`. Draining for an update is the same primitive the shutdown path
//! drains with, and the ADR-0011 preflight that lets it skip the drain entirely
//! is the same one the signal handler asks. One thing differs, and it is why
//! `end_drain()` exists: a stop's drain ends with the process, while an update's
//! must end without it when the install it drained for does not happen. The
//! updater drains as [`Drain::Update`] and lifts only that, never a `faber
//! drain` the operator started.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::sync::Notify;
use tracing::{debug, info, warn};

use crate::audit::now_ms;
use crate::config::{FileConfig, UpdatePolicy};
use crate::github::Github;
use crate::platform::Layout;
use crate::provenance::Verdict;
use crate::reconcile::{Drain, Orchestrator};
pub use faber_release::{
    check_reported_agent_version, check_reported_version, check_reported_version_for, is_newer,
    parse_sha256sums, parse_version, pin_problem, select_candidate, sha256_hex, verify_digest,
    Candidate, Release, ReleaseAsset,
};

/// The daemon binary asset name a release publishes (see `deploy/release.sh`),
/// derived from the target triple this binary was actually built for.
///
/// Derived rather than hardcoded because a release now carries an asset per
/// platform, and every host must pick its own. Since the triple is stamped at
/// build time ([`build.rs`]), a Linux binary structurally *cannot* ask for the
/// macOS asset — there is no runtime choice to get wrong, and no way for a
/// mixed fleet to cross-install. On aarch64 macOS this still renders exactly
/// `faber-aarch64-apple-darwin`, which is what hosts already running v0.3.x
/// look for, so the channel stays unbroken in both directions.
pub(crate) const DAEMON_ASSET_NAME: &str = concat!("faber-", env!("FABER_TARGET"));

/// The menu-bar app bundle asset, a `tar.gz` of `faber.app` — and `None` on
/// platforms that have no such bundle to refresh.
///
/// This being an `Option` is load-bearing rather than tidy. The app is a macOS
/// tray application; a Linux release publishes no app asset at all, and
/// requiring one would make every Linux host fail its update with "release has
/// no asset" and sit forever on the version it was installed with — precisely
/// the stranding ADR-0006 exists to prevent, and invisible until someone went
/// looking, because a host that never updates never reports anything.
#[cfg(target_os = "macos")]
const APP_ASSET_NAME: Option<&str> = Some(concat!("faber-app-", env!("FABER_TARGET"), ".tar.gz"));
#[cfg(not(target_os = "macos"))]
const APP_ASSET_NAME: Option<&str> = None;

/// The VM launcher asset (ADR-0026): the signed `faber-vm` binary. `None` off
/// macOS for the same reason as [`APP_ASSET_NAME`]: a Linux release has no
/// launcher, and requiring one would strand every Linux host.
#[cfg(target_os = "macos")]
const LAUNCHER_ASSET_NAME: Option<&str> = Some(concat!("faber-launcher-", env!("FABER_TARGET")));
#[cfg(not(target_os = "macos"))]
const LAUNCHER_ASSET_NAME: Option<&str> = None;

/// The program name `faber-vm --version` prints before its version.
const LAUNCHER_PROGRAM: &str = "faber-vm";

/// The program name `faber-agent --version` prints before its version.
const AGENT_PROGRAM: &str = "faber-agent";

/// The guest agent asset (ADR-0026): the `faber-agent` binary the tools disk
/// carries. `None` off macOS like [`LAUNCHER_ASSET_NAME`]: only macOS guests run
/// an agent, so a Linux release has none and requiring one would strand Linux hosts.
#[cfg(target_os = "macos")]
const AGENT_ASSET_NAME: Option<&str> = Some(concat!("faber-agent-", env!("FABER_TARGET")));
#[cfg(not(target_os = "macos"))]
const AGENT_ASSET_NAME: Option<&str> = None;

/// `shasum -a 256` output over whichever assets the release publishes.
pub(crate) const SUMS_ASSET_NAME: &str = "SHA256SUMS";

/// The Sigstore bundle over [`SUMS_ASSET_NAME`] (ADR-0031). Optional: its
/// absence is reported to the provenance seam, which decides what it means.
pub(crate) const SUMS_BUNDLE_ASSET_NAME: &str = "SHA256SUMS.sigstore.json";

/// The first release that knows the SYSTEM layout (ADR-0025).
///
/// A host set up by `sudo faber setup` runs its daemon as `_faber` from
/// `D/bin/faber`, with `HOME=D`. A release older than this reads no setup
/// record, so it would resolve every path from `$HOME` as a LEGACY daemon does:
/// a second, empty state tree inside `D`, none of the refusals that keep job
/// code off `_faber`, and the operator's Docker Desktop back in its reach. Such
/// a release is never staged or installed there ([`layout_refusal`]).
pub const MIN_SYSTEM_LAYOUT_VERSION: &str = "0.3.13";

/// Why a host in `layout` must not stage or install `version`, if it must not.
///
/// Only SYSTEM has a floor, [`MIN_SYSTEM_LAYOUT_VERSION`]. A version that does
/// not parse is refused there as well: this is a gate, so what it cannot read
/// it does not let through.
pub fn layout_refusal(layout: Layout, version: &str) -> Option<String> {
    if layout != Layout::System {
        return None;
    }
    let floor = parse_version(MIN_SYSTEM_LAYOUT_VERSION);
    match parse_version(version) {
        Some(v) if floor.is_some_and(|floor| v >= floor) => None,
        _ => Some(format!(
            "{version} predates the SYSTEM layout: a host set up by `sudo faber setup` \
             installs {MIN_SYSTEM_LAYOUT_VERSION} or newer only, because an older faber \
             reads no setup record and would run as _faber with LEGACY paths under its \
             HOME (ADR-0025). Not staging or installing it."
        )),
    }
}

/// What `faber check-config` holds a config to.
///
/// A human running it gets [`FileConfig::validate`]: structure and policy,
/// the same bar the settings save applies. The updater's gate
/// (`check-config --for-update`, run with the *staged* binary) gets
/// [`FileConfig::validate_structural`] only: can this release start on this
/// config. Policy only ever tightens across releases (the Linux dind wall,
/// ADR-0025, is the first), and an unattended update must never be held back,
/// or a host bricked, over a policy it did not choose (AGENTS.md). The boot
/// warns about policy instead.
pub fn config_errors(cfg: &FileConfig, for_update: bool) -> Vec<String> {
    if for_update {
        cfg.validate_structural()
    } else {
        cfg.validate()
    }
}

// ---------------------------------------------------------------------------
// Filesystem layout
// ---------------------------------------------------------------------------

/// Every on-disk location the updater reads or writes, derived from one root
/// so a test can point the whole module at a temp directory instead of the
/// real `$HOME`.
#[derive(Debug, Clone)]
pub struct UpdatePaths {
    pub root: PathBuf,
    /// The VM launcher's directory: the live launcher, its staged copy and its
    /// `.prev` all sit here (see the module docs).
    pub launcher_dir: PathBuf,
}

impl UpdatePaths {
    /// This process's state directory: `$HOME/Library/Application Support/faber`
    /// under LEGACY — `None` when `$HOME` isn't set, mirroring
    /// [`crate::config::FileConfig::default_path`]'s own fallback (the config and
    /// the update state live in the same directory) — and `D` under SYSTEM.
    pub fn from_home() -> Option<Self> {
        let paths = Self::with_root(crate::platform::state_dir()?);
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let paths = {
            let mut paths = paths;
            if crate::platform::layout() == crate::platform::layout::Layout::System {
                paths.launcher_dir = Self::launcher_dir_of(&crate::platform::system());
            }
            paths
        };
        Some(paths)
    }

    /// The SYSTEM launcher directory: [`SystemLayout::launcher_bin`]'s parent,
    /// never derived from `D`.
    ///
    /// [`SystemLayout::launcher_bin`]: crate::platform::layout::SystemLayout::launcher_bin
    pub fn launcher_dir_of(layout: &crate::platform::layout::SystemLayout) -> PathBuf {
        layout
            .launcher_bin
            .parent()
            .expect("the launcher path has a parent")
            .to_path_buf()
    }

    /// An explicit root, with the launcher in `root/bin` as under LEGACY: how
    /// tests (and nothing else) construct one.
    pub fn with_root(root: PathBuf) -> Self {
        let launcher_dir = root.join("bin");
        Self { root, launcher_dir }
    }

    /// The running binary. Never renamed — this exact path is what
    /// `deploy/install.sh` installs and what the LaunchAgent execs.
    pub fn bin(&self) -> PathBuf {
        self.root.join("bin/faber")
    }

    /// The binary this host ran before its most recent install, restored by
    /// [`rollback_if_needed`] on a crash-loop.
    pub fn prev_bin(&self) -> PathBuf {
        self.root.join("bin/faber.prev")
    }

    /// A verified, chmod'd download awaiting a safe moment to swap in.
    pub fn staged_bin(&self) -> PathBuf {
        self.root.join("update/staged/faber")
    }

    /// The staged menu-bar app tarball, paired with [`Self::staged_bin`].
    pub fn staged_app(&self) -> PathBuf {
        self.root.join("update/staged/faber-app.tar.gz")
    }

    /// The VM launcher (ADR-0026), swapped by the updater beside the daemon.
    pub fn launcher(&self) -> PathBuf {
        self.launcher_dir.join("faber-vm")
    }

    /// The launcher this host ran before its most recent install, restored with
    /// [`Self::prev_bin`]. Absent on the first release that ships a launcher.
    pub fn prev_launcher(&self) -> PathBuf {
        self.launcher_dir.join("faber-vm.prev")
    }

    /// The staged launcher, paired with [`Self::staged_bin`]. Beside the live
    /// launcher, not in `update/staged/`: under SYSTEM the launcher's directory
    /// is outside `D`, and the swap must be a rename within one directory.
    pub fn staged_launcher(&self) -> PathBuf {
        self.launcher_dir.join("faber-vm.staged")
    }

    /// The guest agent (ADR-0026), in `D/bin` beside the daemon copy, where
    /// `sudo faber setup` first installs it. The tools disk is built from this
    /// file and nothing else (`tools_disk::for_this_host`).
    pub fn agent(&self) -> PathBuf {
        self.root.join("bin/faber-agent")
    }

    /// The agent this host ran before its most recent install.
    pub fn prev_agent(&self) -> PathBuf {
        self.root.join("bin/faber-agent.prev")
    }

    /// The staged agent, paired with [`Self::staged_bin`].
    pub fn staged_agent(&self) -> PathBuf {
        self.root.join("update/staged/faber-agent")
    }

    /// The config snapshot taken alongside [`Self::prev_bin`], restored to
    /// the live config path together with it.
    pub fn prev_config(&self) -> PathBuf {
        self.root.join("update/config.prev.toml")
    }

    /// The boot-probation marker. See [`rollback_if_needed`].
    pub fn marker(&self) -> PathBuf {
        self.root.join("update/marker.json")
    }

    /// The one-shot "the last exit was on purpose" breadcrumb (ADR-0011 §4).
    pub fn clean_exit(&self) -> PathBuf {
        self.root.join("update/clean-exit")
    }

    /// Versions that crash-looped here (or were manually skipped) and will
    /// never be offered again.
    pub fn quarantine(&self) -> PathBuf {
        self.root.join("update/quarantine.json")
    }

    /// First-seen times per candidate tag (ADR-0031 maturity clock). Persisted so
    /// a restart mid-wait keeps the clock.
    pub fn first_seen(&self) -> PathBuf {
        self.root.join("update/first_seen.json")
    }

    /// A one-shot breadcrumb written by a successful restore, so the boot
    /// that follows it — which has no marker of its own — can still report
    /// "this host just rolled back". See [`take_rollback_breadcrumb`].
    pub fn rolled_back(&self) -> PathBuf {
        self.root.join("update/rolled_back.json")
    }

    /// The result of the running binary's own end-to-end re-verification of the
    /// release it was installed from (#92). Survives restarts so `/api/state`
    /// keeps saying what was proven.
    pub fn self_check(&self) -> PathBuf {
        self.root.join("update/self_check.json")
    }

    /// Sigstore TUF metadata, re-verified on every refresh (ADR-0031). A cache
    /// for the online fetch only: an unreachable TUF repository pauses the
    /// update, and nothing is ever served from here instead.
    pub fn tuf_cache(&self) -> PathBuf {
        self.root.join("update/sigstore-tuf")
    }
}

// ---------------------------------------------------------------------------
// Version comparison
// ---------------------------------------------------------------------------

/// Whether a release-list page of `len` entries means another page may exist.
pub(crate) fn more_release_pages(len: usize) -> bool {
    len >= crate::github::RELEASES_PER_PAGE
}

// ---------------------------------------------------------------------------
// Decision core
// ---------------------------------------------------------------------------

/// A release downloaded, SHA-256-verified, chmod'd, and confirmed compatible
/// with the live config — everything short of actually being swapped in.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Staged {
    pub version: String,
    /// SHA-256 of the staged binary as verified at stage time; `do_commit`
    /// re-hashes the file against it (the stage-to-commit gap).
    pub binary_sha256: String,
    /// SHA-256 of the staged launcher, where this platform ships one;
    /// re-hashed at commit like `binary_sha256`.
    pub launcher_sha256: Option<String>,
    /// SHA-256 of the staged guest agent, where this platform ships one.
    pub agent_sha256: Option<String>,
    /// The release tag and the commit it points to, for the commit-time
    /// `--version` probe.
    pub tag: String,
    pub commit: String,
    /// When this entered `WaitingIdle` (ms) — the anchor `escalate_after_secs`
    /// counts from.
    pub since_ms: u64,
}

/// Everything [`decide`] reasons about. Nothing here is read from the wall
/// clock, the filesystem, or the network — every timestamp arrives as a
/// parameter to `decide` itself. This is the entire surface the update
/// loop's *sequencing* depends on, which is what makes `decide` exhaustively
/// unit-testable without a Tokio runtime, a temp dir, or a mock HTTP server.
#[derive(Debug, Clone, Default)]
pub(crate) struct UpdateState {
    /// When [`Action::Check`] last ran (successfully or not) — `decide` uses
    /// this against `policy.poll_interval_secs` to know when the next one is
    /// due.
    pub last_check_ms: Option<u64>,
    /// One-shot: set by `UpdateHandle::check_now`, consumed by the tick that
    /// acts on it.
    pub check_requested: bool,
    /// A release GitHub has advertised as newer than the running version and
    /// not (yet) known to be quarantined. Set once `Action::Check`'s I/O
    /// confirms both; cleared once staging begins.
    pub available: Option<String>,
    /// Why `available` was chosen, e.g. "v0.4.2: highest published above
    /// v0.4.1". Set and cleared together with `available`.
    pub available_reason: Option<String>,
    /// Where `available`'s maturity clock starts (ms): the later of its
    /// `published_at` and when this host first saw it as its candidate. `None`
    /// means no clock is known, which `decide` treats as not yet mature.
    pub available_since_ms: Option<u64>,
    /// Downloaded, verified, and config-checked — waiting for an idle window
    /// or an operator's `apply_now`.
    pub staged: Option<Staged>,
    /// One-shot: set by `UpdateHandle::apply_now`, consumed by the tick that
    /// acts on it. Skips the idle wait and escalates immediately.
    pub apply_requested: bool,
    /// Versions this host refuses to (re-)stage or install: releases that
    /// already crash-looped here, plus anything an operator explicitly skipped.
    pub quarantined: Vec<String>,
    /// The last failed stage attempt and when the next is allowed (#137).
    /// Cleared on success; ignored by `decide` once the candidate tag differs.
    pub stage_retry: Option<StageRetry>,
}

/// Why a stage attempt failed, which decides how long to wait before the next
/// one (#137).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailClass {
    /// A definite verdict on the release itself (provenance, version binding,
    /// digest mismatch). Retrying cannot change it: wait out a full poll
    /// interval, or until the candidate tag changes.
    Refused,
    /// Nothing is known about the release (network, TUF, 5xx, rate limit, or a
    /// refusal that rests on GitHub's current tag state). Exponential backoff.
    Unavailable,
}

/// A failed stage attempt: its class and the operator-facing reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StageFailure {
    pub class: FailClass,
    pub reason: String,
}

impl std::fmt::Display for StageFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reason)
    }
}

impl StageFailure {
    pub(crate) fn refused(reason: impl Into<String>) -> Self {
        Self {
            class: FailClass::Refused,
            reason: reason.into(),
        }
    }
    pub(crate) fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            class: FailClass::Unavailable,
            reason: reason.into(),
        }
    }
}

/// Where the stage retry schedule stands for one candidate tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StageRetry {
    pub tag: String,
    pub class: FailClass,
    /// Consecutive failed attempts for this tag.
    pub failures: u32,
    pub next_attempt_ms: u64,
}

/// First backoff step for an Unavailable failure; doubles per failure.
pub(crate) const STAGE_BACKOFF_BASE_SECS: u64 = 60;

/// The schedule after a failed attempt on `tag` at `now_ms`. Refused waits one
/// full `poll_interval_secs`; Unavailable waits 60 s, 120 s, 240 s... capped at
/// `poll_interval_secs`. A different tag than `prev` starts counting afresh.
pub(crate) fn stage_retry_after(
    prev: Option<&StageRetry>,
    tag: &str,
    class: FailClass,
    now_ms: u64,
    poll_interval_secs: u64,
) -> StageRetry {
    let failures = prev
        .filter(|p| p.tag == tag)
        .map_or(1, |p| p.failures.saturating_add(1));
    let cap = poll_interval_secs.max(1);
    let wait_secs = match class {
        FailClass::Refused => cap,
        FailClass::Unavailable => STAGE_BACKOFF_BASE_SECS
            .saturating_mul(1u64 << (failures - 1).min(32))
            .min(cap),
    };
    StageRetry {
        tag: tag.to_string(),
        class,
        failures,
        next_attempt_ms: now_ms.saturating_add(wait_secs.saturating_mul(1000)),
    }
}

/// When staging `candidate` is next allowed, if it is being held back.
pub(crate) fn stage_held_until_ms(
    state: &UpdateState,
    candidate: &str,
    now_ms: u64,
) -> Option<u64> {
    state
        .stage_retry
        .as_ref()
        .filter(|r| r.tag == candidate && now_ms < r.next_attempt_ms)
        .map(|r| r.next_attempt_ms)
}

/// When the available release may install (ms since epoch): its maturity clock
/// plus the policy's effective wait. `None` with nothing available or no clock.
pub(crate) fn install_after_ms(state: &UpdateState, policy: &UpdatePolicy) -> Option<u64> {
    state.available.as_ref()?;
    let since = state.available_since_ms?;
    Some(since.saturating_add(policy.effective_maturity_secs().saturating_mul(1000)))
}

/// The maturity clock's start: the later of the release's `published_at` and
/// this host's first sight of it (ADR-0031). A release published quietly and
/// flipped visible later is therefore timed from the flip, not the publish.
pub(crate) fn maturity_clock_ms(published_ms: Option<u64>, first_seen_ms: u64) -> u64 {
    published_ms.map_or(first_seen_ms, |p| p.max(first_seen_ms))
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct FirstSeenFile {
    #[serde(default)]
    tags: std::collections::BTreeMap<String, u64>,
}

/// Return when this host first saw `tag` as its candidate, recording `now_ms`
/// if it is new. Only the current candidate is kept, so a candidate change
/// starts the new release's own clock. Written atomically (temp + rename); a
/// write failure is logged and costs only the restart-survival, not correctness.
pub(crate) fn record_first_seen(path: &Path, tag: &str, now_ms: u64) -> u64 {
    let mut file: FirstSeenFile = std::fs::read_to_string(path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    // A recorded time in the future (clock skew, a hand edit) would stall the
    // wait indefinitely, so it is clamped to now.
    let seen = file.tags.get(tag).map(|t| (*t).min(now_ms));
    if file.tags.len() == 1 && file.tags.get(tag).copied() == seen {
        if let Some(s) = seen {
            return s;
        }
    }
    let first = seen.unwrap_or(now_ms);
    file.tags.clear();
    file.tags.insert(tag.to_string(), first);
    let write = || -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(&file).map_err(std::io::Error::other)?;
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)
    };
    if let Err(e) = write() {
        warn!("could not persist first-seen time for {tag} ({e}); a restart will restart its wait");
    }
    first
}

/// Forget every recorded first-seen time, on disk. Called when there is no
/// candidate, so a tag that drops out and returns starts a fresh wait.
pub(crate) fn clear_first_seen(path: &Path) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            warn!("could not clear first-seen times ({e})");
        }
    }
}

/// Select the candidate and compute its maturity clock, keeping the persisted
/// first-seen file in step: no candidate clears it.
pub(crate) fn resolve_candidate(
    releases: &[Release],
    current: &str,
    pin: Option<&str>,
    first_seen_path: &Path,
    now_ms: u64,
) -> Option<(Candidate, u64)> {
    let Some(c) = select_candidate(releases, current, pin) else {
        clear_first_seen(first_seen_path);
        return None;
    };
    let tag = format!("v{}", c.version);
    let published_ms = releases
        .iter()
        .find(|r| r.tag_name == tag)
        .and_then(|r| r.published_at.as_deref())
        .and_then(crate::retry::parse_rfc3339_to_ms);
    let first_seen = record_first_seen(first_seen_path, &tag, now_ms);
    let clock = maturity_clock_ms(published_ms, first_seen);
    Some((c, clock))
}

/// The final check made immediately before any download: the fresh release
/// list must still choose `version`, and its recomputed clock must be mature.
pub(crate) fn stage_still_valid(
    releases: &[Release],
    policy: &UpdatePolicy,
    current: &str,
    first_seen_path: &Path,
    now_ms: u64,
    version: &str,
) -> bool {
    match resolve_candidate(
        releases,
        current,
        policy.pin.as_deref(),
        first_seen_path,
        now_ms,
    ) {
        Some((c, clock)) => {
            c.version == version
                && now_ms
                    >= clock.saturating_add(policy.effective_maturity_secs().saturating_mul(1000))
        }
        None => false,
    }
}

/// The final check made immediately before the swap: why the staged `tag` must
/// not be installed, given what GitHub serves under it now (`served` is `None`
/// when it serves nothing). `None` means it may go in.
///
/// GitHub serves neither a deleted release nor a draft by tag, so the two read
/// alike. A pre-release is refused unless `pin` names this very tag, the one
/// case [`select_candidate`] takes whatever GitHub marks it as.
pub(crate) fn release_withdrawn(
    served: Option<&Release>,
    tag: &str,
    pin: Option<&str>,
) -> Option<String> {
    let Some(release) = served else {
        return Some(format!(
            "release {tag} was deleted or turned back into a draft after it was staged"
        ));
    };
    if release.draft {
        return Some(format!(
            "release {tag} was turned back into a draft after it was staged"
        ));
    }
    let pinned = pin
        .and_then(parse_version)
        .is_some_and(|p| parse_version(tag) == Some(p));
    if release.prerelease && !pinned {
        return Some(format!(
            "release {tag} was marked a pre-release after it was staged"
        ));
    }
    None
}

/// What the operator's live choices do to a staged update they stop from
/// installing ([`staged_stop`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum StagedStop {
    /// Install nothing and keep it staged: `[update] enabled` is off. [`decide`]
    /// sleeps while updates are off and takes it up again once they are on.
    Hold,
    /// Discard it, for the reason given: the pin names another version, or the
    /// version was skipped. The next poll chooses afresh.
    Discard(String),
}

/// Whether the operator's live choices stop the staged `version` from
/// installing, and how. `None` means it may go in.
///
/// [`decide`] asks on every tick, so a pin moved off the staged version, or a
/// skip of it, discards it at once, not after a wait for idle or a drain that
/// would install nothing. `do_commit` asks again before it installs anything:
/// an escalated install has just waited out a drain that can take hours, and
/// the operator may have paused updates, moved the pin or skipped the version
/// meanwhile. The first answer to a release the owner did not make is to pause
/// the followers, and the pause must hold on one that is already draining.
///
/// A pin that names no version selects nothing in [`select_candidate`], so it
/// installs nothing here either.
pub(crate) fn staged_stop(
    policy: &UpdatePolicy,
    quarantined: &[String],
    version: &str,
) -> Option<StagedStop> {
    if !policy.enabled {
        return Some(StagedStop::Hold);
    }
    // Only a skip quarantines a version while it is staged: boot probation
    // quarantines at boot, and nothing staged survives a restart.
    if quarantined.iter().any(|q| q == version) {
        return Some(StagedStop::Discard(format!(
            "v{version} was skipped on this host"
        )));
    }
    let pin = policy.pin.as_deref()?;
    if parse_version(pin).is_some_and(|p| parse_version(version) == Some(p)) {
        return None;
    }
    Some(StagedStop::Discard(format!(
        "update.pin names {pin}, not the staged v{version}"
    )))
}

/// What the update loop should do on this tick. The *entire* sequencing
/// decision lives in [`decide`]; the async loop's job is to execute exactly
/// the action it returns and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    /// Nothing to do until the next tick.
    Sleep,
    /// Ask GitHub whether a newer release exists.
    Check,
    /// Download, verify, and config-check the named version.
    Stage(String),
    /// Install the staged update and restart.
    Commit,
    /// Stop admitting new jobs, wait for admitted ones to finish, then
    /// commit.
    Escalate,
    /// Discard the staged update without installing it: the operator's live
    /// choices no longer allow it ([`staged_stop`]).
    Discard { version: String, reason: String },
}

/// The whole update loop's sequencing decision, in one pure function: given
/// what time it is, the live policy, and what the updater currently knows,
/// what should it do next?
///
/// The order of the checks below *is* the priority order: a staged update
/// takes precedence over discovering another one (finish what's on disk
/// before asking for more), and a quarantined candidate never reaches
/// `Action::Stage` even if something upstream of this function failed to
/// filter it out first — `decide` is the last word on what gets staged, not
/// merely one of several places that check.
pub(crate) fn decide(
    now_ms: u64,
    policy: &UpdatePolicy,
    state: &UpdateState,
    inflight: usize,
    adoptable: bool,
) -> Action {
    if !policy.enabled {
        return Action::Sleep;
    }

    if let Some(staged) = &state.staged {
        // A pin moved off what is staged, or a skip of it (updates being off
        // returned above). Discarding it now spares the host a wait for idle,
        // and perhaps a drain, that would install nothing.
        if let Some(StagedStop::Discard(reason)) =
            staged_stop(policy, &state.quarantined, &staged.version)
        {
            return Action::Discard {
                version: staged.version.clone(),
                reason,
            };
        }
        // ADR-0011: an adoptable host may commit *while busy*. The next process
        // rebuilds these claims from their own containers, so waiting for idle
        // buys nothing — and the wait it replaces was up to `escalate_after_secs`
        // (six hours by default) followed by a drain that refuses every new job
        // for as long as the longest-lived runner takes to finish.
        if inflight == 0 || adoptable {
            return Action::Commit;
        }
        if state.apply_requested {
            return Action::Escalate;
        }
        if policy.escalate_after_secs > 0 {
            let escalate_after_ms = policy.escalate_after_secs.saturating_mul(1000);
            if now_ms.saturating_sub(staged.since_ms) >= escalate_after_ms {
                return Action::Escalate;
            }
        }
        // Busy, not yet due to escalate (or escalation is disabled outright)
        // — wait for a natural idle window.
        return Action::Sleep;
    }

    if let Some(candidate) = &state.available {
        if state.quarantined.iter().any(|q| q == candidate) {
            return Action::Sleep;
        }
        // Maturity gates before staging, so nothing is downloaded early
        // (ADR-0031). Until then fall through to the poll: a newer release, or
        // this one being withdrawn, must still be noticed during the wait.
        // A failed attempt holds the same tag back (#137); a new tag does not.
        if install_after_ms(state, policy).is_some_and(|t| now_ms >= t)
            && stage_held_until_ms(state, candidate, now_ms).is_none()
        {
            return Action::Stage(candidate.clone());
        }
    }

    if state.check_requested {
        return Action::Check;
    }
    let interval_ms = policy.poll_interval_secs.saturating_mul(1000);
    let due = match state.last_check_ms {
        Some(last) => now_ms.saturating_sub(last) >= interval_ms,
        None => true,
    };
    if due {
        Action::Check
    } else {
        Action::Sleep
    }
}

// ---------------------------------------------------------------------------
// Dashboard-facing status
// ---------------------------------------------------------------------------

/// What the updater is doing right now, for the dashboard.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "phase")]
pub enum UpdatePhase {
    /// `update.enabled` is false. [`UpdateHandle::view`] forces this
    /// regardless of whatever the loop last wrote here — see its own doc.
    Disabled,
    /// Enabled, nothing in progress.
    Idle,
    /// Asking GitHub whether a newer release exists.
    Checking,
    /// Downloading and verifying a release's assets.
    Downloading { version: String },
    /// Verified and on disk, waiting for the host to go quiet.
    WaitingIdle { version: String, since_ms: u64 },
    /// Escalated: refusing new jobs while admitted runners finish.
    Draining { version: String },
    /// Held back on purpose — config incompatible with this release, this
    /// exact version is quarantined, or similar. Not retried until something
    /// relevant changes (a newer release, an edited config).
    Deferred { version: String, reason: String },
    /// A check, download, verify, or install attempt failed. Retried
    /// automatically on the next tick — see the call sites of `fail_stage`
    /// and `abort_commit` for exactly what stays in place across the retry.
    Failed { reason: String },
}

/// A snapshot of autoupdate status for `/api/state` and the settings panel.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UpdateView {
    /// This host's running version, e.g. `"0.2.0+g1a2b3c4"`.
    pub current: String,
    pub enabled: bool,
    /// A newer, non-quarantined release this host has seen, if any —
    /// populated as soon as a check finds one, independent of whether it has
    /// been downloaded yet.
    pub available: Option<String>,
    /// Why `available` was chosen (see [`select_candidate`]).
    #[serde(default)]
    pub candidate_reason: Option<String>,
    pub phase: UpdatePhase,
    pub last_check_ms: Option<u64>,
    pub last_error: Option<String>,
    /// Versions this host will never install on its own: releases that
    /// failed boot probation here, plus anything an operator skipped.
    pub quarantined: Vec<String>,
    /// Set for one boot only: the version this host just rolled back *from*.
    /// See [`take_rollback_breadcrumb`].
    pub rolled_back_from: Option<String>,
    /// This host's `update.pin`, if set.
    pub pin: Option<String>,
    /// While `available` is still maturing (ADR-0031): when it may install, in
    /// ms since epoch. `None` once matured, staged, or when nothing is waiting.
    #[serde(default)]
    pub install_after_ms: Option<u64>,
    /// Seconds left of the maturity wait, as of this snapshot.
    #[serde(default)]
    pub install_in_secs: Option<u64>,
    /// Whether this binary has proven its own updater by re-verifying the
    /// release it was installed from (#92). `None` on a hand-installed binary.
    #[serde(default)]
    pub self_check: Option<crate::self_check::SelfCheckView>,
    /// While a failed stage attempt is being held back (#137): when the next
    /// attempt is due, in ms since epoch. `None` when nothing is held.
    #[serde(default)]
    pub next_attempt_ms: Option<u64>,
    /// How the last supervisor restart after a launcher swap ended, when it did
    /// not end well (ADR-0026). Running VMs are unaffected; the launcher is not
    /// rolled back. `None` when none ran or the last one succeeded.
    #[serde(default)]
    pub supervisor_restart: Option<SupervisorRestartView>,
}

/// A supervisor restart that did not reach the expected version.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SupervisorRestartView {
    /// `timed_out`, `wrong_version`, `not_asked` or `too_soon`.
    pub status: String,
    pub detail: String,
    pub at_ms: u64,
}

impl Default for UpdateView {
    /// The zero value `Orchestrator::view()` fills `StateView.update` with
    /// before the web layer overwrites it from the real [`UpdateHandle`] —
    /// the orchestrator itself knows nothing about updates (see that field's
    /// own doc comment in `src/reconcile.rs`).
    fn default() -> Self {
        Self {
            // The build-stamped version, matching what every other surface
            // reports. A bare `CARGO_PKG_VERSION` here would drop the commit
            // suffix — and on the machine releases are cut from, where the
            // running binary is routinely ahead of the last version bump, that
            // suffix is the only thing distinguishing two different builds
            // calling themselves 0.1.0.
            current: env!("FABER_VERSION").to_string(),
            enabled: false,
            available: None,
            candidate_reason: None,
            phase: UpdatePhase::Disabled,
            last_check_ms: None,
            last_error: None,
            quarantined: Vec::new(),
            rolled_back_from: None,
            pin: None,
            install_after_ms: None,
            install_in_secs: None,
            self_check: None,
            next_attempt_ms: None,
            supervisor_restart: None,
        }
    }
}

// ---------------------------------------------------------------------------
// On-disk bookkeeping: marker, quarantine, rollback breadcrumb
// ---------------------------------------------------------------------------

/// The boot-probation marker (`update/marker.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Marker {
    from: String,
    to: String,
    attempts: u32,
    written_ms: u64,
}

/// `update/quarantine.json`: `{"versions": ["0.3.0"]}`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct QuarantineFile {
    #[serde(default)]
    versions: Vec<String>,
}

/// `update/rolled_back.json`, written by a successful restore and consumed
/// once by [`take_rollback_breadcrumb`] on the very next boot.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RolledBackFile {
    /// The version restored *to* (`marker.from`).
    restored: String,
    /// The version that crash-looped (`marker.to`) — this is what
    /// `UpdateView.rolled_back_from` reports, since "rolled back from X" means
    /// X is the version this host just moved away from.
    failed: String,
    at_ms: u64,
}

fn read_quarantine(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<QuarantineFile>(&text).ok())
        .map(|f| f.versions)
        .unwrap_or_default()
}

/// Append `version` to `quarantine.json` if it isn't already there. Best
/// effort: a write failure is logged, not returned, since no caller can turn a
/// durability problem into anything actionable mid-request. Written atomically,
/// because a torn file reads as an empty list and would offer a crash-looped
/// release to this host again.
fn quarantine_add(path: &Path, version: &str) {
    let mut versions = read_quarantine(path);
    if versions.iter().any(|v| v == version) {
        return;
    }
    versions.push(version.to_string());
    let file = QuarantineFile { versions };
    let written = serde_json::to_string_pretty(&file)
        .map_err(std::io::Error::other)
        .and_then(|text| write_atomic(path, text.as_bytes()));
    if let Err(e) = written {
        warn!("autoupdate: could not record {version} in quarantine.json: {e}");
    }
}

/// Written atomically: a torn marker does not parse, and [`rollback_if_needed`]
/// clears one it cannot parse, so a torn write would end probation unseen.
fn write_marker(paths: &UpdatePaths, marker: &Marker) -> std::io::Result<()> {
    let text = serde_json::to_string_pretty(marker).map_err(std::io::Error::other)?;
    write_atomic(&paths.marker(), text.as_bytes())
}

/// Replace `path` with `bytes` by temp file and rename, so a crash at any point
/// leaves either the old file whole or the new one, never a torn one: the boot
/// after a crash trusts what it finds here. An existing file keeps its
/// permission bits, as it did when it was rewritten in place.
pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let tmp = write_beside(path, bytes)?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })?;
    sync_parent(path);
    Ok(())
}

/// The first half of [`write_atomic`]: `bytes`, synced to disk, in a temp file
/// beside `path` (the same directory, so the rename is atomic). `path` is not
/// touched, so a crash after this leaves it as it was. The name is unique to
/// this process and call; a temp a crashed process left under the same name is
/// replaced, never written through.
fn write_beside(path: &Path, bytes: &[u8]) -> std::io::Result<PathBuf> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return Err(std::io::Error::other(format!(
            "{} names no file in a directory",
            path.display()
        )));
    };
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.tmp.{}.{}",
        name.to_string_lossy(),
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    // The permission bits only: setuid, setgid and sticky never carry over.
    let mode = std::fs::metadata(path)
        .ok()
        .map(|m| m.permissions().mode() & 0o777);
    let written = (|| {
        match std::fs::remove_file(&tmp) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
            _ => {}
        }
        // `create_new` is O_CREAT|O_EXCL: it follows nothing at the temp path.
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        if let Some(mode) = mode {
            opts.mode(mode);
        }
        let mut file = opts.open(&tmp)?;
        if let Some(mode) = mode {
            // The umask narrowed the mode `open` was given; state it exactly.
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        file.write_all(bytes)?;
        file.sync_all()
    })();
    match written {
        Ok(()) => Ok(tmp),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Make a rename into `path`'s directory durable. Best effort: a directory
/// that cannot be opened for sync was still written.
fn sync_parent(path: &Path) {
    if let Some(dir) = path.parent() {
        if let Ok(d) = std::fs::File::open(dir) {
            let _ = d.sync_all();
        }
    }
}

/// Best-effort breadcrumb for the boot that follows a restore: the process
/// that resumes after the caller's `exit(0)` has no marker of its own (it was
/// just deleted) and so, on its own, has no way to know it is running because
/// a rollback happened. Written just before the marker is deleted; consumed
/// once by [`take_rollback_breadcrumb`].
fn write_rollback_breadcrumb(paths: &UpdatePaths, restored: &str, failed: &str) {
    let file = RolledBackFile {
        restored: restored.to_string(),
        failed: failed.to_string(),
        at_ms: now_ms(),
    };
    let path = paths.rolled_back();
    let Ok(text) = serde_json::to_string_pretty(&file) else {
        return;
    };
    if let Err(e) = write_atomic(&path, text.as_bytes()) {
        // The rollback itself already succeeded (the binary is restored) —
        // losing this breadcrumb only costs the dashboard's "why did this
        // version change" explanation on the next boot, never the recovery.
        warn!("autoupdate: could not write rolled_back.json breadcrumb: {e}");
    }
}

/// Consume the breadcrumb [`rollback_if_needed`] leaves behind on a
/// successful restore, returning the version that crash-looped (what
/// `UpdateView.rolled_back_from` reports). One-shot by design: the file is
/// deleted as part of reading it — the "this host just rolled back" fact is
/// only news on the very next boot, so a second read (a later, unrelated
/// restart) must see `None`, or the dashboard would keep reporting a
/// rollback that is old history. Missing or unparseable -> `None`, never an
/// error.
pub fn take_rollback_breadcrumb(paths: &UpdatePaths) -> Option<String> {
    let path = paths.rolled_back();
    let text = std::fs::read_to_string(&path).ok();
    // Delete regardless of whether the read/parse below succeeds: a corrupt
    // breadcrumb must not linger and be retried forever any more than a
    // missing one should be treated as an error.
    let _ = std::fs::remove_file(&path);
    let parsed: RolledBackFile = serde_json::from_str(&text?).ok()?;
    Some(parsed.failed)
}

/// Record that this process is exiting *on purpose*, so the boot that follows is
/// not counted against probation (ADR-0011 §4).
///
/// Best-effort and silent on failure: the cost of not writing it is one probation
/// attempt, never a failed shutdown. Deliberately **not** called by `do_commit` —
/// that exit is the one probation exists to watch, and excusing it would spend
/// the first of the two attempts a crash-looping binary is allowed.
pub fn note_clean_exit(paths: &UpdatePaths) {
    let path = paths.clean_exit();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Err(e) = std::fs::write(&path, b"deliberate\n") {
        debug!("clean-exit breadcrumb not written ({e}) — this boot may count against probation");
    }
}

/// Delete the boot-probation marker, ending probation and making an update
/// stick. Called from a task `main()` spawns after a successful boot, once
/// `probation_secs` has elapsed with no crash — [`rollback_if_needed`] itself
/// only ever *increments* the attempt counter on a clean boot (see its
/// "otherwise" branch); something has to clear the marker on the *success*
/// path, and that is this function.
///
/// Treats an already-missing marker as success: this runs on a timer racing
/// nothing in particular, so it may fire after the marker was already
/// cleared with nothing wrong (e.g. a concurrent `rollback_if_needed` call on
/// a fast-crashing boot got there first).
pub fn clear_marker(paths: &UpdatePaths) {
    match std::fs::remove_file(paths.marker()) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => warn!("autoupdate: failed to clear the boot marker: {e}"),
    }
}

/// Make the next boot roll this version back: raise the marker's attempt count
/// to the crash-loop threshold, so [`rollback_if_needed`] restores `faber.prev`,
/// quarantines the version and leaves the breadcrumb, exactly as for a
/// crash-loop. Used when the boot self-check (#92) gets a definite refusal.
/// Returns false when there is no marker to raise (nothing to roll back).
pub fn force_rollback(paths: &UpdatePaths) -> bool {
    let Ok(text) = std::fs::read_to_string(paths.marker()) else {
        return false;
    };
    let Ok(mut marker) = serde_json::from_str::<Marker>(&text) else {
        return false;
    };
    marker.attempts = marker.attempts.max(2);
    write_marker(paths, &marker).is_ok()
}

fn config_mtime_ms(path: &Path) -> Option<u64> {
    let meta = std::fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let dur = modified.duration_since(std::time::UNIX_EPOCH).ok()?;
    Some(dur.as_millis() as u64)
}

// ---------------------------------------------------------------------------
// Rollback
// ---------------------------------------------------------------------------

/// What [`rollback_if_needed`] found and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackReport {
    pub from: String,
    pub to: String,
    pub restored: bool,
    pub note: Option<String>,
}

/// Inspect the boot-probation marker and act on it. Called as the *very
/// first thing* in the `run` command, before the config is even loaded — a
/// binary that dies on config load must still be able to rescue itself, so
/// this cannot depend on anything a config parse could fail on.
///
/// Must never panic and must never propagate an error that would stop the
/// daemon booting: every fallible step below is best-effort, and the
/// function always produces *some* answer rather than aborting partway
/// through. Returns `Some(report)` exactly when this boot follows an update
/// attempt (a marker existed); `None` means "nothing to do here, proceed to
/// a normal boot".
///
/// Rules, in order:
/// - No marker (or an unreadable/corrupt one) -> `None`.
/// - `marker.to` doesn't match `current_version` (ignoring build metadata) ->
///   the binary was replaced by something else since the marker was written
///   (a manual install) — delete the marker, report `restored: false`.
/// - `attempts >= 2` -> crash-loop: restore `bin/faber.prev` and the config
///   snapshot, each by temp file and rename (the running binary's file is
///   never rewritten), quarantine the bad version, leave a rollback breadcrumb for
///   the next boot, delete the marker, report `restored: true`. If
///   `faber.prev` is missing, delete the marker anyway (so this can't loop
///   forever retrying an impossible restore) and report `restored: false`.
/// - A `clean-exit` breadcrumb is present -> consume it and leave `attempts`
///   alone. The previous exit was deliberate (a signal, `faber drain`), so this
///   boot is not evidence of anything (ADR-0011 §4).
/// - Otherwise -> increment `attempts`, rewrite the marker, report
///   `restored: false`. The caller is expected to clear the marker itself
///   after `probation_secs` of a clean boot (see [`clear_marker`]).
pub fn rollback_if_needed(
    paths: &UpdatePaths,
    current_version: &str,
    live_config: &Path,
) -> Option<RollbackReport> {
    // Consumed *before* anything can return early, so it is genuinely one-shot:
    // it says "the exit immediately before this boot was deliberate" and must not
    // survive to excuse some later, unrelated boot. Read here even when there is
    // no marker to apply it to — that is the common case, and leaving it on disk
    // would hand a free attempt to whatever update happens to come next.
    let deliberate = std::fs::remove_file(paths.clean_exit()).is_ok();

    let marker_path = paths.marker();
    let text = std::fs::read_to_string(&marker_path).ok()?;
    let marker: Marker = match serde_json::from_str(&text) {
        Ok(m) => m,
        Err(_) => {
            // A marker faber itself cannot parse gives no reliable "from" to
            // report — clear it and say nothing, rather than crash-loop over
            // faber's own bookkeeping.
            let _ = std::fs::remove_file(&marker_path);
            return None;
        }
    };

    if parse_version(&marker.to) != parse_version(current_version) {
        let _ = std::fs::remove_file(&marker_path);
        return Some(RollbackReport {
            from: marker.from,
            to: marker.to.clone(),
            restored: false,
            note: Some(format!(
                "boot marker expected {} but this binary is {current_version} — \
                 probably a manual install; marker cleared",
                marker.to
            )),
        });
    }

    if marker.attempts >= 2 {
        // By temp file and rename, never a copy over `bin/faber`: that is the
        // file this very process runs from. Rewriting it in place left a torn
        // daemon if the copy was cut short, and on Linux the open fails with
        // `ETXTBSY`, so no rollback happened at all.
        return Some(match atomic_install_sync(&paths.prev_bin(), &paths.bin()) {
            Ok(()) => {
                // Best-effort from here on: the binary is already back, which
                // is what makes the next boot succeed, so nothing past this
                // point may prevent the marker from being cleared below.
                //
                // The config goes back by rename too. A torn one is never
                // repaired: the next boot runs the restored binary, which the
                // marker does not name, and clears the marker as a manual install.
                // Its directory is the one target here faber may not own (a
                // `--config` elsewhere), so a failed rename still writes in place.
                if let Ok(config) = std::fs::read(paths.prev_config()) {
                    if let Err(e) = write_atomic(live_config, &config) {
                        warn!("autoupdate: restoring the previous config by rename: {e}");
                        if let Err(e) = std::fs::write(live_config, &config) {
                            warn!("autoupdate: could not restore the previous config: {e}");
                        }
                    }
                }
                // The launcher goes back with the daemon it was built with.
                // With no snapshot (the first release that shipped one) it is
                // left in place, never deleted.
                if let Err(e) = restore_launcher(paths) {
                    warn!("autoupdate: could not restore the previous launcher: {e}");
                }
                if let Err(e) = restore_agent(paths) {
                    warn!("autoupdate: could not restore the previous agent: {e}");
                }
                quarantine_add(&paths.quarantine(), &marker.to);
                write_rollback_breadcrumb(paths, &marker.from, &marker.to);
                let _ = std::fs::remove_file(&marker_path);
                RollbackReport {
                    from: marker.from.clone(),
                    to: marker.to.clone(),
                    restored: true,
                    note: Some(format!(
                        "{} crash-looped after {} boot(s) — restored {}",
                        marker.to, marker.attempts, marker.from
                    )),
                }
            }
            Err(e) => {
                // Cannot restore — but the marker must still go, or every
                // future boot re-attempts the same impossible copy forever.
                let _ = std::fs::remove_file(&marker_path);
                RollbackReport {
                    from: marker.from.clone(),
                    to: marker.to.clone(),
                    restored: false,
                    note: Some(format!(
                        "{} crash-looped but bin/faber.prev could not be restored \
                         ({e}) — marker cleared so rollback does not loop",
                        marker.to
                    )),
                }
            }
        });
    }

    let mut marker = marker;
    // ADR-0011 §4. Probation counts *unexplained* boots. Before adoption every
    // restart was expensive, so a boot inside `probation_secs` was fair evidence
    // of a crash-loop; now that a restart is deliberate and cheap, an operator
    // iterating with bootout/install/bootstrap would otherwise roll back — and
    // quarantine — a perfectly good version in three boots inside two minutes.
    //
    // A crash writes no breadcrumb, so genuine crash-loops still count exactly as
    // they did.
    if !deliberate {
        marker.attempts += 1;
    }
    marker.written_ms = now_ms();
    if let Err(e) = write_marker(paths, &marker) {
        // The boot still goes ahead, but uncounted: a crash-loop that cannot
        // count its boots never reaches the rollback.
        warn!("autoupdate: could not count this boot in the probation marker: {e}");
    }
    Some(RollbackReport {
        from: marker.from,
        to: marker.to,
        restored: false,
        note: None,
    })
}

// ---------------------------------------------------------------------------
// The updater
// ---------------------------------------------------------------------------

/// A version already known to fail `check-config` against the config as it
/// stood at that moment. Kept in memory only (not persisted): the point is
/// only to avoid hot-looping Check -> Stage -> Deferred within one daemon
/// lifetime, not to survive a restart the way `quarantine.json` does.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfigDeferred {
    version: String,
    config_mtime_ms: Option<u64>,
}

/// Live-mutable state shared between [`Updater`] (the loop) and
/// [`UpdateHandle`] (the thin view/control surface web handlers hold). Split
/// into one `Mutex` per independently-updated concern rather than one big
/// lock, and — a hard invariant, not an accident — no two of these are ever
/// held locked at once: every access locks, reads or writes, and drops
/// before the next lock or the next `.await`. That is what makes it safe to
/// mix them freely without reasoning about lock order.
struct Shared {
    state: Mutex<UpdateState>,
    /// Live-tunable via `set_policy` (`POST /api/settings`). Read fresh at
    /// the top of every tick, so a change takes effect on the very next loop
    /// iteration rather than needing a restart.
    policy: Mutex<UpdatePolicy>,
    /// The phase shown on the dashboard. Kept separate from `UpdateState`
    /// because it is a *reporting* projection (free-form reasons, an
    /// in-progress download's version) rather than something `decide` reads
    /// — folding it in would make the pure core carry UI concerns.
    phase: Mutex<UpdatePhase>,
    last_error: Mutex<Option<String>>,
    /// The boot self-check's progress and result (#92). See [`crate::self_check`].
    self_check: Mutex<Option<crate::self_check::SelfCheckView>>,
    /// The last supervisor restart that went wrong (ADR-0026).
    supervisor_restart: Mutex<Option<SupervisorRestartView>>,
    config_deferred: Mutex<Option<ConfigDeferred>>,
    /// Set once at construction from `take_rollback_breadcrumb`; never
    /// changes for the life of the process (a second rollback would be a
    /// second process).
    rolled_back_from: Option<String>,
    current_version: String,
    paths: UpdatePaths,
    /// Wakes the loop early for `check_now` / `apply_now` / `set_policy`,
    /// instead of it sleeping out the rest of a possibly-hours-long
    /// `poll_interval_secs`. `notify_one` (not `notify_waiters`): there is
    /// exactly one waiter — the loop itself — so a signal sent while it is
    /// mid-tick, not yet back at the `.notified()` await, is not lost the way
    /// it would be with the broadcast-style call.
    wake: Notify,
}

/// The thin, cloneable view/control surface for the updater: what
/// `GET /api/state` reads and what the three `POST /api/update/*` routes
/// call. Deliberately does not hold `Arc<Github>` / `Arc<Orchestrator>` — it
/// never does I/O itself, only reads and writes the state [`Updater::run_loop`]
/// also shares.
pub struct UpdateHandle {
    shared: Arc<Shared>,
}

impl UpdateHandle {
    /// A snapshot for `/api/state` and the settings panel's live status.
    pub fn view(&self) -> UpdateView {
        let policy = self.shared.policy.lock().unwrap().clone();
        let (available, candidate_reason, last_check_ms, quarantined, install_after, next_attempt) = {
            let state = self.shared.state.lock().unwrap();
            let next_attempt = state
                .available
                .as_deref()
                .and_then(|c| stage_held_until_ms(&state, c.trim_start_matches('v'), now_ms()));
            (
                state.available.clone(),
                state.available_reason.clone(),
                state.last_check_ms,
                state.quarantined.clone(),
                install_after_ms(&state, &policy),
                next_attempt,
            )
        };
        let now = now_ms();
        let pending = install_after.filter(|t| *t > now);
        let install_in_secs = pending.map(|t| (t - now).div_ceil(1000));
        // A disabled policy always reads as `Disabled`, regardless of
        // whatever the loop last wrote to `phase`: the loop stops acting the
        // moment `enabled` flips false, but nothing forces it to also reset
        // `phase` at that exact instant, and the dashboard must not show a
        // stale in-progress phase for a feature that is now off.
        let phase = if policy.enabled {
            self.shared.phase.lock().unwrap().clone()
        } else {
            UpdatePhase::Disabled
        };
        let last_error = self.shared.last_error.lock().unwrap().clone();
        UpdateView {
            current: self.shared.current_version.clone(),
            enabled: policy.enabled,
            available,
            candidate_reason,
            phase,
            last_check_ms,
            last_error,
            quarantined,
            rolled_back_from: self.shared.rolled_back_from.clone(),
            pin: policy.pin,
            install_after_ms: pending,
            install_in_secs,
            self_check: self.shared.self_check.lock().unwrap().clone(),
            next_attempt_ms: next_attempt,
            supervisor_restart: self.shared.supervisor_restart.lock().unwrap().clone(),
        }
    }

    /// Publish (or clear, with `None`) the last failed supervisor restart.
    pub fn set_supervisor_restart(&self, view: Option<SupervisorRestartView>) {
        *self.shared.supervisor_restart.lock().unwrap() = view;
    }

    /// Publish the boot self-check's state for `/api/state` (#92).
    pub fn set_self_check(&self, view: crate::self_check::SelfCheckView) {
        *self.shared.self_check.lock().unwrap() = Some(view);
    }

    /// Live-apply new `[update]` settings (`POST /api/settings`). Takes
    /// effect on the very next tick, the same apply-immediately shape
    /// `Limiter::set_tuning` uses — which is what makes it honest for the
    /// settings panel to call every `update.*` key "live".
    pub fn set_policy(&self, p: UpdatePolicy) {
        *self.shared.policy.lock().unwrap() = p;
        self.shared.wake.notify_one();
    }

    /// Poll for a release right now, instead of waiting out the rest of
    /// `poll_interval_secs`.
    pub fn check_now(&self) {
        self.shared.state.lock().unwrap().check_requested = true;
        self.shared.wake.notify_one();
    }

    /// Stop waiting for a natural idle window: drain and install the staged
    /// update as soon as currently-admitted jobs finish. A no-op if nothing
    /// is staged yet — the flag is simply consumed, harmlessly, whenever
    /// something later is.
    pub fn apply_now(&self) {
        self.shared.state.lock().unwrap().apply_requested = true;
        self.shared.wake.notify_one();
    }

    /// Quarantine the currently-available version, if any, and stop offering
    /// it — the operator's manual override for a release the updater would
    /// otherwise install on its own. A staged copy is discarded on the next
    /// tick ([`staged_stop`]). Persisted to `quarantine.json` so a
    /// restart does not re-offer the very version just turned down; a
    /// failure to persist only costs that durability, since the in-memory
    /// refusal takes effect immediately either way.
    pub fn skip_available(&self) {
        let skipped = {
            let mut state = self.shared.state.lock().unwrap();
            state.available_reason = None;
            state.available_since_ms = None;
            state.available.take().inspect(|v| {
                if !state.quarantined.iter().any(|q| q == v) {
                    state.quarantined.push(v.clone());
                }
            })
        };
        if let Some(v) = skipped {
            quarantine_add(&self.shared.paths.quarantine(), &v);
            *self.shared.phase.lock().unwrap() = UpdatePhase::Idle;
        }
        self.shared.wake.notify_one();
    }
}

/// A bare [`UpdateHandle`] over a scratch root, for tests of code that only
/// reports through the handle.
#[cfg(test)]
pub(crate) fn test_handle(paths: UpdatePaths, version: &str) -> Arc<UpdateHandle> {
    Arc::new(UpdateHandle {
        shared: Arc::new(Shared {
            state: Mutex::new(UpdateState::default()),
            policy: Mutex::new(UpdatePolicy::default()),
            phase: Mutex::new(UpdatePhase::Idle),
            last_error: Mutex::new(None),
            self_check: Mutex::new(crate::self_check::read_record(&paths, version)),
            supervisor_restart: Mutex::new(None),
            config_deferred: Mutex::new(None),
            rolled_back_from: None,
            current_version: version.to_string(),
            paths,
            wake: Notify::new(),
        }),
    })
}

/// The update loop itself: owns the GitHub client and orchestrator it needs
/// to do I/O, and the config path it validates staged releases against.
/// Built once in `main()`; [`Self::run_loop`] is spawned as its own task and
/// never returns.
pub struct Updater {
    shared: Arc<Shared>,
    gh: Arc<Github>,
    orch: Arc<Orchestrator>,
    config_path: PathBuf,
}

impl Updater {
    /// Build the updater and its handle, sharing one state behind two thin
    /// wrappers (see [`Shared`]'s own doc comment for why it is split this
    /// way). `rolled_back_from` is normally the result of a `main()` call to
    /// [`take_rollback_breadcrumb`] made just before this — the breadcrumb is
    /// consumed once, here, and lives for the rest of the process as
    /// `UpdateView.rolled_back_from`.
    pub fn new(
        gh: Arc<Github>,
        orch: Arc<Orchestrator>,
        policy: UpdatePolicy,
        paths: UpdatePaths,
        config_path: PathBuf,
        current_version: String,
        rolled_back_from: Option<String>,
    ) -> (Arc<Self>, Arc<UpdateHandle>) {
        let quarantined = read_quarantine(&paths.quarantine());
        let shared = Arc::new(Shared {
            state: Mutex::new(UpdateState {
                quarantined,
                ..UpdateState::default()
            }),
            policy: Mutex::new(policy),
            phase: Mutex::new(UpdatePhase::Idle),
            last_error: Mutex::new(None),
            self_check: Mutex::new(crate::self_check::read_record(&paths, &current_version)),
            supervisor_restart: Mutex::new(None),
            config_deferred: Mutex::new(None),
            rolled_back_from,
            current_version,
            paths,
            wake: Notify::new(),
        });
        let updater = Arc::new(Self {
            shared: shared.clone(),
            gh,
            orch,
            config_path,
        });
        let handle = Arc::new(UpdateHandle { shared });
        (updater, handle)
    }

    /// Drive the updater forever: tick, then sleep until either the next
    /// scheduled tick or an early wake. Never returns on its own — the only
    /// exits are a successful [`Self::do_commit`] (`process::exit(0)`) or the
    /// process dying some other way.
    pub async fn run_loop(self: Arc<Self>) {
        loop {
            self.tick().await;
            let woken = self.shared.wake.notified();
            tokio::select! {
                _ = tokio::time::sleep(self.next_sleep()) => {}
                _ = woken => {}
            }
        }
    }

    /// One iteration: snapshot the current inputs, ask the pure core what to
    /// do, and do exactly that. All the *sequencing* — whether to check,
    /// wait, stage, escalate, or commit — was decided in [`decide`];
    /// everything below is I/O with no branching of its own beyond "which
    /// action did I get".
    async fn tick(&self) {
        let now = now_ms();
        let policy = self.shared.policy.lock().unwrap().clone();
        let snapshot = self.shared.state.lock().unwrap().clone();
        // The updater talks to the orchestrator through `view()`, the drain
        // methods and — since ADR-0011 — `adoptable()` (see the module doc).
        let inflight = self.orch.view().await.inflight.len();
        // Only asked when it can change the answer: the preflight shells out to
        // Docker, and an idle host commits either way.
        let adoptable = inflight > 0 && snapshot.staged.is_some() && self.orch.adoptable().await;

        match decide(now, &policy, &snapshot, inflight, adoptable) {
            Action::Sleep => {}
            Action::Check => self.do_check(&policy).await,
            Action::Stage(version) => self.do_stage(&policy, &version).await,
            Action::Commit => self.do_commit().await,
            Action::Escalate => self.do_escalate(&snapshot).await,
            Action::Discard { version, reason } => self.do_discard(&version, reason).await,
        }
    }

    /// How long to sleep before the next tick, absent an early wake. A
    /// staged-or-available update ticks at `min(poll_interval_secs, 30)` so a
    /// natural idle window or an `escalate_after_secs` deadline is noticed
    /// within half a minute; a fully idle updater ticks at the full
    /// `poll_interval_secs` — there is nothing to gain from polling GitHub
    /// more often than that.
    fn next_sleep(&self) -> Duration {
        let policy = self.shared.policy.lock().unwrap();
        let secs = {
            let state = self.shared.state.lock().unwrap();
            if state.available.is_some() || state.staged.is_some() {
                policy.poll_interval_secs.min(30)
            } else {
                policy.poll_interval_secs
            }
        };
        Duration::from_secs(secs.max(1))
    }

    /// Execute [`Action::Check`]: ask GitHub for the release this policy
    /// tracks, and record what was found. Never downloads a byte of the
    /// release itself — that is [`Self::do_stage`]'s job, kept on a separate
    /// tick so a slow or large download never blocks the cheap "is there
    /// anything new" poll.
    async fn fetch_releases(
        &self,
        policy: &UpdatePolicy,
    ) -> Result<Vec<Release>, crate::github::GithubError> {
        match &policy.pin {
            Some(pin) => self
                .gh
                .release_by_tag(&policy.repo, &format!("v{pin}"))
                .await
                .map(|r| r.into_iter().collect::<Vec<_>>()),
            None => self.gh.list_releases(&policy.repo).await,
        }
    }

    async fn do_check(&self, policy: &UpdatePolicy) {
        *self.shared.phase.lock().unwrap() = UpdatePhase::Checking;

        let fetched = self.fetch_releases(policy).await;

        let now = now_ms();
        {
            let mut state = self.shared.state.lock().unwrap();
            state.last_check_ms = Some(now);
            state.check_requested = false;
        }

        let releases = match fetched {
            Ok(r) => r,
            Err(e) => {
                // A failed check still counts as "checked" for cadence
                // purposes (`last_check_ms` was already set above) --
                // otherwise a persistent GitHub outage would make `decide`
                // see this as perpetually due and hammer the API every tick
                // instead of waiting out the normal poll interval.
                *self.shared.last_error.lock().unwrap() = Some(e.to_string());
                *self.shared.phase.lock().unwrap() = UpdatePhase::Idle;
                return;
            }
        };
        *self.shared.last_error.lock().unwrap() = None;

        let Some((Candidate { version, reason }, clock)) = resolve_candidate(
            &releases,
            &self.shared.current_version,
            policy.pin.as_deref(),
            &self.shared.paths.first_seen(),
            now,
        ) else {
            // The candidate is gone (withdrawn, or flipped to pre-release):
            // drop its stale availability and clock rather than let it mature.
            {
                let mut state = self.shared.state.lock().unwrap();
                state.available = None;
                state.available_reason = None;
                state.available_since_ms = None;
            }
            if let Some(pin) = &policy.pin {
                if let Some(problem) = pin_problem(&releases, &self.shared.current_version, pin) {
                    warn!("{problem}");
                    *self.shared.last_error.lock().unwrap() = Some(problem);
                }
            }
            *self.shared.phase.lock().unwrap() = UpdatePhase::Idle;
            return;
        };

        // Never offered at all, rather than offered and then refused at stage
        // time: a release this host must not run is not "available" to it.
        if let Some(reason) = layout_refusal(crate::platform::layout(), &version) {
            self.refuse_for_layout(&version, reason).await;
            return;
        }

        let already_quarantined = self
            .shared
            .state
            .lock()
            .unwrap()
            .quarantined
            .iter()
            .any(|q| q == &version);
        if already_quarantined {
            *self.shared.phase.lock().unwrap() = UpdatePhase::Deferred {
                version,
                reason: "previously failed boot probation on this host".to_string(),
            };
            return;
        }

        // A release that already failed `check-config` against the config as
        // it stood is not retried verbatim every poll — only once the
        // release changes (a newer tag superseding this one) or the operator
        // edits the config, either of which invalidates this memory.
        let cfg_mtime = config_mtime_ms(&self.config_path);
        let still_config_deferred = self
            .shared
            .config_deferred
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|d| d.version == version && d.config_mtime_ms == cfg_mtime);
        if still_config_deferred {
            *self.shared.phase.lock().unwrap() = UpdatePhase::Deferred {
                version,
                reason: "config incompatible with this release".to_string(),
            };
            return;
        }

        {
            let mut state = self.shared.state.lock().unwrap();
            state.available = Some(version);
            state.available_reason = Some(reason);
            state.available_since_ms = Some(clock);
        }
        *self.shared.phase.lock().unwrap() = UpdatePhase::Idle;
    }

    /// Execute `Action::Stage(version)`: re-fetch the release (cheaply — the
    /// ETag-aware `get` behind it makes this a 304 in the common case) to get
    /// its current asset list, download and SHA-256-verify both assets,
    /// `chmod` the binary, and run it through `check-config` against the
    /// live config. Only on all of that succeeding does `state.staged`
    /// become set — the thing the next tick's `decide` will try to commit.
    async fn do_stage(&self, policy: &UpdatePolicy, version: &str) {
        // `do_check` already never offers such a version; this is the gate on
        // the path that writes bytes, whatever put the version here.
        if let Some(reason) = layout_refusal(crate::platform::layout(), version) {
            self.refuse_for_layout(version, reason).await;
            return;
        }
        // Final check before any download (ADR-0031): the candidate may have
        // been flipped to pre-release or withdrawn since the last poll, and the
        // clock is recomputed from fresh data. Failing it stages nothing.
        match self.fetch_releases(policy).await {
            Ok(releases) => {
                if !stage_still_valid(
                    &releases,
                    policy,
                    &self.shared.current_version,
                    &self.shared.paths.first_seen(),
                    now_ms(),
                    version,
                ) {
                    warn!("v{version} is no longer the matured candidate; not staging");
                    let mut state = self.shared.state.lock().unwrap();
                    state.available = None;
                    state.available_reason = None;
                    state.available_since_ms = None;
                    drop(state);
                    *self.shared.phase.lock().unwrap() = UpdatePhase::Idle;
                    return;
                }
            }
            Err(e) => {
                self.fail_stage(
                    version,
                    StageFailure::unavailable(format!(
                        "re-checking v{version} before staging: {e}"
                    )),
                )
                .await;
                return;
            }
        }

        *self.shared.phase.lock().unwrap() = UpdatePhase::Downloading {
            version: version.to_string(),
        };

        let tag = format!("v{version}");
        let release = match self.gh.release_by_tag(&policy.repo, &tag).await {
            Ok(Some(r)) => r,
            Ok(None) => {
                self.fail_stage(
                    version,
                    StageFailure::unavailable(format!(
                        "release {tag} disappeared from {}",
                        policy.repo
                    )),
                )
                .await;
                return;
            }
            Err(e) => {
                self.fail_stage(
                    version,
                    StageFailure::unavailable(format!("fetching release {tag}: {e}")),
                )
                .await;
                return;
            }
        };

        let Downloaded {
            commit,
            binary_sha256,
            launcher_sha256,
            agent_sha256,
        } = match self.download_and_verify(&policy.repo, &release).await {
            Ok(v) => v,
            Err(e) => {
                self.fail_stage(version, e).await;
                return;
            }
        };

        let staged_bin = self.shared.paths.staged_bin();
        if let Err(e) = chmod_executable(&staged_bin) {
            self.fail_stage(
                version,
                StageFailure::unavailable(format!("chmod {}: {e}", staged_bin.display())),
            )
            .await;
            return;
        }

        // The version binding (ADR-0031, step 3; issue #91): an older genuine
        // build republished under a newer tag verifies and digest-matches, so
        // only what the binary itself reports can refuse it.
        if let Err(e) = verify_binary_version(&staged_bin, &release.tag_name, &commit).await {
            self.fail_stage(version, StageFailure::refused(e)).await;
            return;
        }

        // The launcher is bound to the same tag and commit (ADR-0026).
        if launcher_sha256.is_some() {
            let staged_launcher = self.shared.paths.staged_launcher();
            if let Err(e) = chmod_executable(&staged_launcher) {
                self.fail_stage(
                    version,
                    StageFailure::unavailable(format!("chmod {}: {e}", staged_launcher.display())),
                )
                .await;
                return;
            }
            if let Err(e) =
                verify_launcher_version(&staged_launcher, &release.tag_name, &commit).await
            {
                self.fail_stage(version, StageFailure::refused(e)).await;
                return;
            }
        }

        // The guest agent likewise (ADR-0026).
        if agent_sha256.is_some() {
            let staged_agent = self.shared.paths.staged_agent();
            if let Err(e) = chmod_executable(&staged_agent) {
                self.fail_stage(
                    version,
                    StageFailure::unavailable(format!("chmod {}: {e}", staged_agent.display())),
                )
                .await;
                return;
            }
            if let Err(e) = verify_agent_version(&staged_agent, &release.tag_name, &commit).await {
                self.fail_stage(version, StageFailure::refused(e)).await;
                return;
            }
        }

        match run_check_config(&staged_bin, &self.config_path).await {
            Ok(true) => {}
            Ok(false) => {
                self.defer_for_config(version, "config incompatible with this release".to_string())
                    .await;
                return;
            }
            Err(e) => {
                self.fail_stage(
                    version,
                    StageFailure::unavailable(format!("running check-config: {e}")),
                )
                .await;
                return;
            }
        }

        let since_ms = now_ms();
        self.shared.state.lock().unwrap().stage_retry = None;
        self.shared.state.lock().unwrap().staged = Some(Staged {
            version: version.to_string(),
            since_ms,
            binary_sha256,
            launcher_sha256,
            agent_sha256,
            tag: release.tag_name.clone(),
            commit,
        });
        *self.shared.phase.lock().unwrap() = UpdatePhase::WaitingIdle {
            version: version.to_string(),
            since_ms,
        };
    }

    /// Download both release assets named in `SHA256SUMS`, verify each
    /// against its published digest, and only then write them to their
    /// staged paths — so a valid binary is never left paired with a corrupt
    /// or missing app bundle (or vice versa) for `do_commit` to trip over
    /// later. Nothing is written to `update/staged/` until everything has
    /// verified.
    ///
    /// Before `SHA256SUMS` is trusted at all, its *provenance* is checked
    /// (issue #41). A digest only proves the bytes were not corrupted in
    /// transit — it says nothing about who produced them, so anyone who can
    /// publish a release to this repo could otherwise own every host on the
    /// channel. `SHA256SUMS` is the only thing that needs this check: every
    /// asset's integrity already chains from it through [`verify_digest`]
    /// below, so verifying the anchor carries the guarantee to everything it
    /// signs for. Doing it immediately after `SHA256SUMS` downloads and
    /// before either binary does is also what makes a refusal cheap to
    /// retry: the failed attempt cost one small text file, not a
    /// multi-megabyte download.
    ///
    /// Returns the commit the tag points to, as the provenance check resolved
    /// it, so the version binding reuses it rather than asking GitHub twice,
    /// with the digests recorded for the commit-time re-check.
    async fn download_and_verify(
        &self,
        repo: &str,
        release: &Release,
    ) -> Result<Downloaded, StageFailure> {
        // A missing asset may be a release still mid-upload: Unavailable.
        let bin_asset = find_asset(release, DAEMON_ASSET_NAME).ok_or_else(|| {
            StageFailure::unavailable(format!(
                "release {} has no {DAEMON_ASSET_NAME} asset",
                release.tag_name
            ))
        })?;
        // Only look for an app bundle on a platform that has one. Where
        // `APP_ASSET_NAME` is `None` this stays `None` and the download and
        // digest check below are skipped entirely.
        let app_asset = match APP_ASSET_NAME {
            Some(name) => Some(find_asset(release, name).ok_or_else(|| {
                StageFailure::unavailable(format!(
                    "release {} has no {name} asset",
                    release.tag_name
                ))
            })?),
            None => None,
        };
        // Likewise the launcher, which only macOS ships (ADR-0026).
        let launcher_asset = match LAUNCHER_ASSET_NAME {
            Some(name) => Some(required_asset(release, name)?),
            None => None,
        };
        let agent_asset = match AGENT_ASSET_NAME {
            Some(name) => Some(required_asset(release, name)?),
            None => None,
        };
        let sums_asset = find_asset(release, SUMS_ASSET_NAME).ok_or_else(|| {
            StageFailure::unavailable(format!(
                "release {} has no {SUMS_ASSET_NAME} asset",
                release.tag_name
            ))
        })?;

        let sums_bytes = self
            .gh
            .download_release_asset(repo, sums_asset.id)
            .await
            .map_err(|e| {
                StageFailure::unavailable(format!("downloading {SUMS_ASSET_NAME}: {e}"))
            })?;
        let bundle_bytes = match find_asset(release, SUMS_BUNDLE_ASSET_NAME) {
            Some(asset) => Some(
                self.gh
                    .download_release_asset(repo, asset.id)
                    .await
                    .map_err(|e| {
                        StageFailure::unavailable(format!(
                            "downloading {SUMS_BUNDLE_ASSET_NAME}: {e}"
                        ))
                    })?,
            ),
            None => None,
        };

        // The trust anchor: one provenance call, before `SHA256SUMS` is parsed
        // and before either binary is fetched. Anything but `Verified` refuses.
        let commit = verify_sums_provenance(
            &self.gh,
            repo,
            &self.shared.paths.tuf_cache(),
            &release.tag_name,
            &sums_bytes,
            bundle_bytes.as_deref(),
        )
        .await?;

        let sums = parse_sha256sums(&String::from_utf8_lossy(&sums_bytes));

        let bin_bytes = self
            .gh
            .download_release_asset(repo, bin_asset.id)
            .await
            .map_err(|e| {
                StageFailure::unavailable(format!("downloading {DAEMON_ASSET_NAME}: {e}"))
            })?;
        verify_digest(&sums, DAEMON_ASSET_NAME, &bin_bytes).map_err(StageFailure::refused)?;
        let binary_sha256 = sha256_hex(&bin_bytes);

        let app_bytes = match (app_asset, APP_ASSET_NAME) {
            (Some(asset), Some(name)) => {
                let bytes = self
                    .gh
                    .download_release_asset(repo, asset.id)
                    .await
                    .map_err(|e| StageFailure::unavailable(format!("downloading {name}: {e}")))?;
                verify_digest(&sums, name, &bytes).map_err(StageFailure::refused)?;
                Some(bytes)
            }
            _ => None,
        };

        // Covered by the same attested `SHA256SUMS` as the daemon.
        let launcher = match (launcher_asset, LAUNCHER_ASSET_NAME) {
            (Some(asset), Some(name)) => {
                let bytes = self
                    .gh
                    .download_release_asset(repo, asset.id)
                    .await
                    .map_err(|e| StageFailure::unavailable(format!("downloading {name}: {e}")))?;
                let sha = verify_launcher_digest(&sums, name, &bytes)?;
                Some((bytes, sha))
            }
            _ => None,
        };

        // And the guest agent, under the same attested sums.
        let agent = match (agent_asset, AGENT_ASSET_NAME) {
            (Some(asset), Some(name)) => {
                let bytes = self
                    .gh
                    .download_release_asset(repo, asset.id)
                    .await
                    .map_err(|e| StageFailure::unavailable(format!("downloading {name}: {e}")))?;
                let sha = verify_launcher_digest(&sums, name, &bytes)?;
                Some((bytes, sha))
            }
            _ => None,
        };

        write_staged(
            &self.shared.paths,
            &bin_bytes,
            app_bytes.as_deref(),
            launcher.as_ref().map(|(bytes, _)| bytes.as_slice()),
            agent.as_ref().map(|(bytes, _)| bytes.as_slice()),
        )
        .await?;
        Ok(Downloaded {
            commit,
            binary_sha256,
            launcher_sha256: launcher.map(|(_, sha)| sha),
            agent_sha256: agent.map(|(_, sha)| sha),
        })
    }

    /// A stage attempt failed for a reason that might not recur (a network
    /// blip, a transient GitHub error, a checksum mismatch worth retrying
    /// once more in case the asset was mid-upload). Clears any partial
    /// download and records the error, but deliberately leaves
    /// `state.available` untouched: the candidate is still valid, so the
    /// *next* tick's `decide` returns `Stage` again rather than the release
    /// being silently dropped after one bad attempt. The failure is recorded in
    /// `state.stage_retry` (#137), which `decide` reads: a Refused tag waits a
    /// full `poll_interval_secs`, an Unavailable one backs off from 60 s,
    /// doubling, capped at `poll_interval_secs`; a different candidate tag is
    /// tried at once.
    async fn fail_stage(&self, version: &str, failure: StageFailure) {
        let StageFailure { class, reason } = failure;
        let poll = self.shared.policy.lock().unwrap().poll_interval_secs;
        let retry = {
            let mut state = self.shared.state.lock().unwrap();
            let retry =
                stage_retry_after(state.stage_retry.as_ref(), version, class, now_ms(), poll);
            state.stage_retry = Some(retry.clone());
            retry
        };
        // Logged, like every sibling failure path here (`abort_commit`,
        // `defer_for_config`) — and this one is the least optional of the three.
        //
        // It was silent: a follower could refuse the same release on tick after
        // tick and write **zero** lines to `stderr.log` and zero records to
        // `audit.jsonl`. The refusal existed only in a live `GET /api/state`.
        // That is the wrong shape for a fleet — a host that can never install a
        // release is a standing condition someone has to be able to find without
        // polling every dashboard — and it is the wrong shape for this path in
        // particular, which is where a **provenance refusal** lands (issue #41):
        // the one failure here that may mean a release was tampered with, reported
        // more quietly than a disk hiccup during install.
        //
        // Escalation does not cover it: `escalate_after_secs` governs the idle
        // wait for an already-*staged* update, and a refusal never stages.
        warn!(
            "autoupdate: staging v{version} failed ({class:?}, attempt {}), next attempt in {}s: {reason}",
            retry.failures,
            retry.next_attempt_ms.saturating_sub(now_ms()).div_ceil(1000)
        );
        remove_staged(&self.shared.paths).await;
        *self.shared.last_error.lock().unwrap() = Some(reason.clone());
        *self.shared.phase.lock().unwrap() = UpdatePhase::Failed { reason };
    }

    /// A version failed `check-config` — either while staging, or again at
    /// commit time if the config changed during the idle wait. Unlike
    /// [`Self::fail_stage`], this *is* deterministic (retrying without a
    /// changed input reproduces the same failure), so it clears both
    /// `staged` and `available`, remembers the (version, config) pair that
    /// failed, and reports `Deferred` — `do_check` skips re-offering the same
    /// pair until either changes.
    async fn defer_for_config(&self, version: &str, reason: String) {
        warn!("autoupdate: deferring {version}: {reason}");
        remove_staged(&self.shared.paths).await;
        *self.shared.config_deferred.lock().unwrap() = Some(ConfigDeferred {
            version: version.to_string(),
            config_mtime_ms: config_mtime_ms(&self.config_path),
        });
        {
            let mut state = self.shared.state.lock().unwrap();
            state.staged = None;
            state.available = None;
            state.available_reason = None;
            state.available_since_ms = None;
        }
        *self.shared.phase.lock().unwrap() = UpdatePhase::Deferred {
            version: version.to_string(),
            reason,
        };
    }

    /// A version this host's layout must never run ([`layout_refusal`]).
    /// Deterministic like [`Self::defer_for_config`], so it clears `staged`
    /// and `available` alike and reports `Deferred`. Nothing needs
    /// remembering: `do_check` applies the same refusal every time it sees the
    /// version, so it is never offered again.
    async fn refuse_for_layout(&self, version: &str, reason: String) {
        warn!("autoupdate: {reason}");
        remove_staged(&self.shared.paths).await;
        {
            let mut state = self.shared.state.lock().unwrap();
            state.staged = None;
            state.available = None;
        }
        *self.shared.phase.lock().unwrap() = UpdatePhase::Deferred {
            version: version.to_string(),
            reason,
        };
    }

    /// The staged release is no longer published as it was when it was staged
    /// ([`release_withdrawn`]). Deterministic like [`Self::defer_for_config`]:
    /// the staged files and the candidate both go, and the next poll decides
    /// afresh. Its first-seen time goes too, so a release that comes back is
    /// timed as a new one. Never quarantined: withdrawing is the owner's call, not
    /// proof the binary is bad.
    async fn refuse_withdrawn(&self, version: &str, reason: String) {
        warn!("autoupdate: not installing {version}: {reason}");
        remove_staged(&self.shared.paths).await;
        clear_first_seen(&self.shared.paths.first_seen());
        {
            let mut state = self.shared.state.lock().unwrap();
            state.staged = None;
            state.available = None;
            state.available_reason = None;
            state.available_since_ms = None;
        }
        *self.shared.phase.lock().unwrap() = UpdatePhase::Deferred {
            version: version.to_string(),
            reason,
        };
    }

    /// Execute `Action::Discard`: the operator's live choices no longer allow the
    /// staged update ([`staged_stop`]). The staged files and the candidate both
    /// go, as [`Self::refuse_withdrawn`] discards a withdrawn release, so the next
    /// poll chooses afresh. The first-seen time is left alone: a pin says nothing
    /// about the release, and a skipped one is never offered here again.
    async fn do_discard(&self, version: &str, reason: String) {
        info!("autoupdate: not installing {version}: {reason}");
        remove_staged(&self.shared.paths).await;
        {
            let mut state = self.shared.state.lock().unwrap();
            state.staged = None;
            state.available = None;
            state.available_reason = None;
            state.available_since_ms = None;
        }
        *self.shared.phase.lock().unwrap() = UpdatePhase::Deferred {
            version: version.to_string(),
            reason,
        };
    }

    /// A commit step failed for a reason that might not recur (a disk
    /// hiccup, a transient copy error). Leaves `staged`/`available`
    /// untouched — the downloaded, verified bytes are still good — so the
    /// next tick's `decide` simply tries `Commit` again instead of
    /// re-downloading from scratch.
    fn state_clear_staged(&self) {
        self.shared.state.lock().unwrap().staged = None;
    }

    async fn abort_commit(&self, version: &str, reason: String) {
        warn!("autoupdate: install attempt for {version} failed, will retry: {reason}");
        *self.shared.last_error.lock().unwrap() = Some(reason.clone());
        *self.shared.phase.lock().unwrap() = UpdatePhase::Failed { reason };
    }

    /// Execute `Action::Commit`: install the staged update and end the
    /// process so launchd restarts it on the new binary. This function does
    /// not return on success — the last thing it does is
    /// `std::process::exit(0)` — so every step before that must leave the
    /// filesystem in a state the *next* process (running the old or the new
    /// binary, depending on exactly when a crash lands) can recover from.
    /// That is why the binary swap is second-to-last: everything that can
    /// still fail cleanly (re-validating config, snapshotting for rollback,
    /// arming the boot marker) happens first, and only the (best-effort,
    /// non-fatal) app bundle refresh happens after — a failure there costs
    /// the app one missed update round, never the daemon's.
    async fn do_commit(&self) {
        let (version, staged) = {
            let state = self.shared.state.lock().unwrap();
            match state.staged.as_ref() {
                Some(s) => (s.version.clone(), s.clone()),
                // `decide` only returns `Commit` when something is staged;
                // never trust that blindly from an I/O shell that could in
                // principle be called out of order.
                None => return,
            }
        };

        // The operator's live choices, read again rather than taken from
        // `decide`: an escalated install has just waited out a drain that can
        // take hours, and a pause, a new pin or a skip made meanwhile must hold
        // (`staged_stop`). It comes first, so a paused host neither asks GitHub
        // nor runs a staged byte again.
        let policy = self.shared.policy.lock().unwrap().clone();
        let quarantined = self.shared.state.lock().unwrap().quarantined.clone();
        match staged_stop(&policy, &quarantined, &version) {
            None => {}
            Some(StagedStop::Hold) => {
                info!(
                    "autoupdate: updates are off, so {version} stays staged and is not installed"
                );
                // Not `Draining`: an escalation lifts its drain once this returns.
                *self.shared.phase.lock().unwrap() = UpdatePhase::WaitingIdle {
                    version,
                    since_ms: staged.since_ms,
                };
                return;
            }
            Some(StagedStop::Discard(reason)) => {
                self.do_discard(&version, reason).await;
                return;
            }
        }

        // The layout floor once more, right before the swap: this is the one
        // step that cannot be taken back by the next tick.
        let layout = crate::platform::layout();
        if let Some(reason) = layout_refusal(layout, &version) {
            self.refuse_for_layout(&version, reason).await;
            return;
        }

        // The release itself, read again from GitHub. The waiting period is how
        // a release the owner did not make gets caught (ADR-0031): the owner
        // deletes it. A host that staged it before then must not install it
        // now, so this runs before any staged byte is run again. An answer
        // GitHub cannot give installs nothing either; the next tick asks again.
        match self.gh.release_by_tag(&policy.repo, &staged.tag).await {
            Ok(served) => {
                if let Some(reason) =
                    release_withdrawn(served.as_ref(), &staged.tag, policy.pin.as_deref())
                {
                    self.refuse_withdrawn(&version, reason).await;
                    return;
                }
            }
            Err(e) => {
                self.abort_commit(
                    &version,
                    format!("re-reading release {} before installing: {e}", staged.tag),
                )
                .await;
                return;
            }
        }

        // (a) The staged config-check ran against whatever the config held
        // at staging time; re-run it now in case an operator edited the
        // config during the (possibly hours-long) wait for an idle window.
        let staged_bin = self.shared.paths.staged_bin();

        // (a0) The stage-to-commit gap: the file may have been replaced since
        // the digest and `--version` checks. Re-hash and re-probe; any
        // mismatch discards the staged update (it re-stages from scratch).
        if let Err(reason) = recheck_staged_binary(&staged_bin, &staged).await {
            self.state_clear_staged();
            self.fail_stage(
                &version,
                StageFailure::refused(format!("staged binary changed before install: {reason}")),
            )
            .await;
            return;
        }

        if LAUNCHER_ASSET_NAME.is_some() {
            if let Err(reason) =
                recheck_staged_launcher(&self.shared.paths.staged_launcher(), &staged).await
            {
                self.state_clear_staged();
                self.fail_stage(
                    &version,
                    StageFailure::refused(format!(
                        "staged launcher changed before install: {reason}"
                    )),
                )
                .await;
                return;
            }
        }

        if AGENT_ASSET_NAME.is_some() {
            if let Err(reason) =
                recheck_staged_agent(&self.shared.paths.staged_agent(), &staged).await
            {
                self.state_clear_staged();
                self.fail_stage(
                    &version,
                    StageFailure::refused(format!("staged agent changed before install: {reason}")),
                )
                .await;
                return;
            }
        }

        match run_check_config(&staged_bin, &self.config_path).await {
            Ok(true) => {}
            Ok(false) => {
                self.defer_for_config(
                    &version,
                    "config became incompatible while waiting to install".to_string(),
                )
                .await;
                return;
            }
            Err(e) => {
                self.abort_commit(&version, format!("re-running check-config: {e}"))
                    .await;
                return;
            }
        }

        // (b) Snapshot the binary and config this update is about to
        // replace, so a rollback has something to restore. The config comes
        // from `self.config_path` — the same file this process validates
        // against, and the same one `rollback_if_needed` is handed to restore
        // into. All three have to name one file: snapshotting one path and
        // restoring to another would leave the recovered binary paired with a
        // config that has moved on without it, which is the crash-loop this
        // whole snapshot exists to prevent.
        //
        // Each snapshot is written by temp file and rename, like the swap: a
        // marker armed by an earlier update may still be live, and a crash-loop
        // would then restore a snapshot cut short.
        if let Err(e) =
            atomic_install(&self.shared.paths.bin(), &self.shared.paths.prev_bin()).await
        {
            self.abort_commit(&version, format!("snapshotting the running binary: {e}"))
                .await;
            return;
        }
        if let Err(e) = snapshot_launcher(&self.shared.paths).await {
            self.abort_commit(&version, format!("snapshotting the running launcher: {e}"))
                .await;
            return;
        }
        if let Err(e) = snapshot_agent(&self.shared.paths).await {
            self.abort_commit(&version, format!("snapshotting the running agent: {e}"))
                .await;
            return;
        }
        let live_config = &self.config_path;
        if live_config.exists() {
            let snapshot = match tokio::fs::read(live_config).await {
                Ok(config) => write_atomic(&self.shared.paths.prev_config(), &config),
                Err(e) => Err(e),
            };
            if let Err(e) = snapshot {
                self.abort_commit(&version, format!("snapshotting the live config: {e}"))
                    .await;
                return;
            }
        }

        // (c) Arm boot probation *before* the swap: if the new binary dies
        // the instant it starts, the marker must already exist for
        // `rollback_if_needed` to find on the very next boot.
        let marker = Marker {
            from: self.shared.current_version.clone(),
            to: version.clone(),
            attempts: 0,
            written_ms: now_ms(),
        };
        if let Err(e) = write_marker(&self.shared.paths, &marker) {
            self.abort_commit(&version, format!("writing boot marker: {e}"))
                .await;
            return;
        }

        // (d0) The launcher goes in *before* the daemon, so the daemon swap
        // below stays the last, single point of no return (ADR-0026). A failure
        // here has changed nothing but the armed marker, which the daemon-swap
        // failure below leaves just the same: the next boot finds it naming a
        // version this binary is not, and clears it as a manual install.
        if LAUNCHER_ASSET_NAME.is_some() {
            if let Err(e) = atomic_install(
                &self.shared.paths.staged_launcher(),
                &self.shared.paths.launcher(),
            )
            .await
            {
                self.abort_commit(&version, format!("installing new launcher: {e}"))
                    .await;
                return;
            }
        }

        // The agent goes in beside the launcher, still before the daemon.
        if AGENT_ASSET_NAME.is_some() {
            if let Err(e) = atomic_install(
                &self.shared.paths.staged_agent(),
                &self.shared.paths.agent(),
            )
            .await
            {
                if let Err(re) = restore_launcher(&self.shared.paths) {
                    warn!("autoupdate: could not restore the previous launcher: {re}");
                }
                if let Err(re) = restore_agent(&self.shared.paths) {
                    warn!("autoupdate: could not restore the previous agent: {re}");
                }
                self.abort_commit(&version, format!("installing new agent: {e}"))
                    .await;
                return;
            }
        }

        // (d) The swap itself — see `atomic_install`'s own doc comment for
        // why this is never a truncate-in-place.
        if let Err(e) = atomic_install(&staged_bin, &self.shared.paths.bin()).await {
            // The old daemon keeps running, so it must keep the launcher it
            // was built with: put the snapshot back, best effort.
            if let Err(re) = restore_launcher(&self.shared.paths) {
                warn!("autoupdate: could not restore the previous launcher: {re}");
            }
            if let Err(re) = restore_agent(&self.shared.paths) {
                warn!("autoupdate: could not restore the previous agent: {re}");
            }
            self.abort_commit(&version, format!("installing new binary: {e}"))
                .await;
            return;
        }

        // The menu-bar app is the operator's and updates itself (ADR-0034): the
        // daemon never touches `faber.app`.

        // (f)
        warn!(
            "autoupdate: installed {version} (was {}) — restarting to run it",
            self.shared.current_version
        );
        std::process::exit(0);
    }

    /// Execute `Action::Escalate`: stop admitting new jobs, wait for whatever
    /// is already running to finish, then commit. Reached either because
    /// `escalate_after_secs` elapsed with the host never going idle on its
    /// own, or because an operator called `apply_now` — both cases want the
    /// same thing: install now, letting currently-admitted work finish first
    /// rather than killing it.
    async fn do_escalate(&self, snapshot: &UpdateState) {
        self.escalate_with(snapshot, self.orch.await_drain()).await;
    }

    /// [`Self::do_escalate`] over an injectable wait for the drain, so what
    /// follows a failed install is testable without a Docker daemon in the loop.
    ///
    /// A successful commit exits the process, so returning from it means nothing
    /// was installed. The drain is then lifted, or this follower would claim
    /// nothing until it next restarts, but only the updater's own: a `faber
    /// drain` the operator started stays. The escalation window starts again
    /// too, so an install that keeps failing costs one drain per
    /// `escalate_after_secs`, not one per tick. An idle host, or Apply now,
    /// still commits at once.
    async fn escalate_with(
        &self,
        snapshot: &UpdateState,
        drained: impl std::future::Future<Output = ()>,
    ) {
        let Some(staged) = &snapshot.staged else {
            return; // `decide` only returns `Escalate` with something staged.
        };
        *self.shared.phase.lock().unwrap() = UpdatePhase::Draining {
            version: staged.version.clone(),
        };
        self.shared.state.lock().unwrap().apply_requested = false;
        self.orch.begin_drain(Drain::Update).await;
        drained.await;
        self.do_commit().await;

        if let Some(still) = self.shared.state.lock().unwrap().staged.as_mut() {
            still.since_ms = now_ms();
        }
        if self.orch.end_drain(Drain::Update) {
            info!(
                "autoupdate: {} was not installed; its drain is lifted and claiming resumes",
                staged.version
            );
        } else {
            info!(
                "autoupdate: {} was not installed; its drain is lifted, and faber stays \
                 drained for `faber drain`",
                staged.version
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Provenance (issue #41)
// ---------------------------------------------------------------------------
//
// A SHA-256 digest (below) proves a download matches what `SHA256SUMS`
// says — it proves nothing about who *wrote* `SHA256SUMS`. Anyone who could
// publish a release owned every host on the channel. ADR-0031 answers that
// with a keyless Sigstore bundle over `SHA256SUMS`; this section is the single
// seam that calls the verifier in `crate::provenance` (#119).
//
// The first draft of this design had a verification failure quarantine the
// release, the same way a crash-looped boot does. That was wrong, and the
// correction is worth keeping in mind while reading this section:
// `quarantine.json` is for a version *proven bad* by crash-looping in boot
// probation. A provenance refusal is a policy verdict about who published
// the bytes, not evidence the binary is broken — and a verdict can hinge on
// something transient (GitHub or Sigstore's TUF unreachable), so quarantining
// on a transient miss would permanently poison a
// perfectly good release for one unlucky host. Both outcomes below refuse
// and let the next poll retry instead — cheap, since (see
// `Updater::download_and_verify`) this check runs before either binary is
// downloaded.

/// What to do about a [`Verdict`] classification.
///
/// Only two variants, deliberately: there is no `Quarantine` action — see
/// the section doc above for why reusing that mechanism here would be
/// wrong. A `Refuse` here reaches [`Updater::download_and_verify`] as a
/// plain `Err(fault)`, which flows into the same [`Updater::fail_stage`]
/// path every other stage-verification failure already uses (leaves
/// `state.available` alone, so the next tick simply tries again).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProvenanceAction {
    /// Proceed exactly as if no provenance check existed.
    Proceed,
    /// Refuse this update round. `fault` is operator-facing text explaining
    /// what to do next — which differs by outcome (see [`decide_provenance`]),
    /// because the operator's fix differs: investigate the release, or fix
    /// what stops this host reaching GitHub or Sigstore.
    Refuse { fault: String },
}

/// Decide what to do about `tag`'s `SHA256SUMS` given how its provenance
/// check came back. Pure — no I/O, no clock — so the three-way branch (the
/// interesting part of this feature) is exhaustively unit-tested without
/// ever running a verifier.
///
/// `Refused` and `Unavailable` both refuse, unconditionally — there is no
/// policy knob that downgrades either to a warning. A flag that could
/// turn "I cannot verify" into "install anyway" is exactly the anti-pattern
/// a fail-closed control must not have: unknown has to mean deny, or the
/// guarantee it provides is unreasonable to reason about. It would also get
/// set "temporarily" on some host and never unset, silently dropping that
/// host out of the security model. The two outcomes stay distinct only so
/// `fault`'s wording tells the operator the right thing: `Refused` says
/// investigate the release; `Unavailable` names the fix on this host (and
/// nothing about the release itself is in question).
fn decide_provenance(p: &Verdict, tag: &str) -> ProvenanceAction {
    match p {
        Verdict::Verified => ProvenanceAction::Proceed,
        Verdict::Refused(reason) => ProvenanceAction::Refuse {
            fault: format!(
                "provenance could not be established for {tag}: {reason} — investigate the release before retrying"
            ),
        },
        Verdict::Unavailable(reason) => ProvenanceAction::Refuse { fault: reason.clone() },
    }
}

/// The provenance seam: the one function the update path calls, before any
/// binary is downloaded. Inputs are the downloaded `SHA256SUMS` bytes, the
/// Sigstore bundle bytes (`SHA256SUMS.sigstore.json`, `None` when the release
/// has no such asset) and the release tag. Only [`Verdict::Verified`] lets the
/// update proceed.
///
/// The checks themselves are [`crate::provenance::check`]'s, under the policy
/// compiled into this binary. This function supplies what they need from the
/// outside world, in the order that costs least when it refuses:
///
/// 1. the bundle, which a release without one fails on before any request;
/// 2. the commit the release's tag points to, read from GitHub now rather than
///    from the release object (whose `target_commitish` may be a branch name),
///    annotated tags peeled. Immutable releases mean a published tag never moves;
/// 3. Sigstore's trusted root, refreshed through TUF. Unreachable means
///    [`Verdict::Unavailable`], never a cached or embedded root.
#[cfg(test)]
async fn verify_provenance(
    gh: &Github,
    repo: &str,
    tuf_cache: &Path,
    sums_bytes: &[u8],
    bundle_bytes: Option<&[u8]>,
    tag: &str,
) -> Verdict {
    verify_provenance_at(gh, repo, tuf_cache, sums_bytes, bundle_bytes, tag)
        .await
        .0
}

/// [`verify_provenance`] plus the tag's commit when it was resolved, so the
/// caller can reuse it for the version binding instead of fetching it again.
#[cfg(test)]
pub(crate) async fn verify_provenance_at(
    gh: &Github,
    repo: &str,
    tuf_cache: &Path,
    sums_bytes: &[u8],
    bundle_bytes: Option<&[u8]>,
    tag: &str,
) -> (Verdict, Option<String>) {
    let (verdict, commit, _) =
        verify_provenance_detail(gh, repo, tuf_cache, sums_bytes, bundle_bytes, tag).await;
    (verdict, commit)
}

/// [`verify_provenance_at`] plus whether a `Refused` verdict rests on **GitHub's
/// current state** (the tag lookup) rather than on the bytes and the bundle.
///
/// The update path treats both the same: nothing installs. The boot self-check
/// (#92) must not: a deleted tag, or a transient 404 on `git/ref/tags`, is not
/// evidence against the running binary, so it maps the flagged refusals to
/// Unavailable instead of rolling back. Flagged: the tag does not exist, a tag
/// name or object sha faber will not use, and a certificate whose source commit
/// differs from the commit the tag resolved to *now*.
pub(crate) async fn verify_provenance_detail(
    gh: &Github,
    repo: &str,
    tuf_cache: &Path,
    sums_bytes: &[u8],
    bundle_bytes: Option<&[u8]>,
    tag: &str,
) -> (Verdict, Option<String>, bool) {
    let Some(bundle_bytes) = bundle_bytes else {
        return (
            Verdict::Refused(format!("release has no {SUMS_BUNDLE_ASSET_NAME}")),
            None,
            false,
        );
    };
    let commit = match gh.tag_commit(repo, tag).await {
        Ok(Some(commit)) => commit,
        Ok(None) => {
            return (
                Verdict::Refused(format!(
                "source commit: tag {tag} does not exist in {repo}, so there is no commit to pin"
            )),
                None,
                true,
            )
        }
        // A tag name or an object sha faber will not use is a verdict on what
        // the release published, not an outage.
        Err(e @ crate::github::GithubError::Refused(_)) => {
            return (Verdict::Refused(format!("source commit: {e}")), None, true)
        }
        Err(e) => {
            return (
                Verdict::Unavailable(format!(
                    "resolving tag {tag} to its commit, to verify the release: {e}"
                )),
                None,
                false,
            )
        }
    };
    let root = match crate::provenance::fetch_trusted_root(tuf_cache).await {
        Ok(root) => root,
        Err(e) => {
            return (
                Verdict::Unavailable(format!(
                    "{e} -- updates pause until it is reachable; no cached root is used"
                )),
                Some(commit),
                false,
            )
        }
    };
    let verdict = crate::provenance::check(
        crate::provenance::active_policy(),
        &root,
        sums_bytes,
        bundle_bytes,
        &commit,
    );
    // The certificate's source-commit field is compared with the commit the tag
    // resolved to just now, so a mismatch there depends on the tag's state.
    let tag_state = matches!(
        &verdict,
        Verdict::Refused(r) if r.starts_with(crate::provenance::SOURCE_COMMIT_FIELD)
    );
    (verdict, Some(commit), tag_state)
}

/// Run the seam and translate the verdict into proceed (`Ok`) or a fault
/// [`Updater::fail_stage`] records and retries next poll (`Err`).
async fn verify_sums_provenance(
    gh: &Github,
    repo: &str,
    tuf_cache: &Path,
    tag: &str,
    sums_bytes: &[u8],
    bundle_bytes: Option<&[u8]>,
) -> Result<String, StageFailure> {
    let (verdict, commit, tag_state) =
        verify_provenance_detail(gh, repo, tuf_cache, sums_bytes, bundle_bytes, tag).await;
    match decide_provenance(&verdict, tag) {
        // `Verified` is only ever produced after the commit was resolved.
        ProvenanceAction::Proceed => commit.ok_or_else(|| {
            StageFailure::unavailable(format!(
                "internal: {tag} verified without a resolved commit"
            ))
        }),
        // A refusal resting on GitHub's current tag state is not a verdict on
        // the release's bytes (#137): back off like an outage.
        ProvenanceAction::Refuse { fault } => Err(StageFailure {
            class: if matches!(verdict, Verdict::Refused(_)) && !tag_state {
                FailClass::Refused
            } else {
                FailClass::Unavailable
            },
            reason: fault,
        }),
    }
}

// ---------------------------------------------------------------------------
// SHA-256
// ---------------------------------------------------------------------------

pub(crate) fn find_asset<'a>(release: &'a Release, name: &str) -> Option<&'a ReleaseAsset> {
    release.assets.iter().find(|a| a.name == name)
}

// ---------------------------------------------------------------------------
// Install mechanics
// ---------------------------------------------------------------------------

/// `chmod 0755` a file. Every staged binary needs this before it can be
/// executed at all (`check-config`, or later being run as `bin/faber`
/// itself) — a plain download has no execute bit.
fn chmod_executable(path: &Path) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}

/// Atomically replace `dest` with `staged`'s bytes: write into a temp file in
/// `dest`'s own directory (so the rename below is same-filesystem, which is
/// what makes it atomic), set the executable bit, then `rename` over `dest`.
///
/// `dest` is `bin/faber` — the binary this very process may be running from
/// — so it is never truncated-in-place: a `rename` swaps the directory entry
/// in one syscall, with no window in which a concurrent reader (launchd
/// re-execing, a dashboard request mid-flight) could observe a partially
/// written or non-executable file, which a copy or an open-and-overwrite
/// cannot promise.
async fn atomic_install(staged: &Path, dest: &Path) -> std::io::Result<()> {
    let (staged, dest) = (staged.to_path_buf(), dest.to_path_buf());
    tokio::task::spawn_blocking(move || atomic_install_sync(&staged, &dest))
        .await
        .map_err(std::io::Error::other)?
}

/// The blocking body of [`atomic_install`], and the one way a binary faber runs
/// is replaced: the swap, its snapshots and the rollback all go through it. The
/// temp name carries `dest`'s own file name, so the launcher's temp file never
/// collides with the daemon's. The bytes are synced before the rename, so a
/// power cut after it never leaves `dest` naming a file whose data never landed.
fn atomic_install_sync(staged: &Path, dest: &Path) -> std::io::Result<()> {
    let dir = dest.parent().ok_or_else(|| {
        std::io::Error::other(format!("{} has no parent directory", dest.display()))
    })?;
    std::fs::create_dir_all(dir)?;
    let name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let tmp = dir.join(format!(".{name}.tmp.{}", std::process::id()));
    let install = (|| {
        std::fs::copy(staged, &tmp)?;
        chmod_executable(&tmp)?;
        std::fs::File::open(&tmp)?.sync_all()?;
        std::fs::rename(&tmp, dest)
    })();
    match install {
        Ok(()) => {
            sync_parent(dest);
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Pauses before each retry of a spawn that hit `ETXTBSY`: 50, 100, 200, 400 ms.
const TEXT_FILE_BUSY_BACKOFF_MS: [u64; 4] = [50, 100, 200, 400];

/// Whether `e` is `ETXTBSY` ("Text file busy") from `execve`.
pub(crate) fn is_text_file_busy(e: &std::io::Error) -> bool {
    e.kind() == std::io::ErrorKind::ExecutableFileBusy
}

/// Run `spawn`, retrying while it fails with `ETXTBSY`.
///
/// A binary is written, then executed. `fork()` copies every open descriptor,
/// so while another thread is mid-spawn its child holds a copy of our write
/// descriptor until its own `exec` closes it. `execve` of a file that any
/// descriptor still has open for writing fails with `ETXTBSY`. The window is
/// short and the file is fine, so retry with a bounded backoff (5 attempts in
/// all); any other error is returned at once.
pub(crate) async fn retry_text_file_busy<T, F, Fut>(mut spawn: F) -> std::io::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::io::Result<T>>,
{
    for ms in TEXT_FILE_BUSY_BACKOFF_MS {
        match spawn().await {
            Err(e) if is_text_file_busy(&e) => {
                tokio::time::sleep(Duration::from_millis(ms)).await;
            }
            other => return other,
        }
    }
    spawn().await
}

/// [`retry_text_file_busy`] for blocking callers.
pub(crate) fn retry_text_file_busy_blocking<T>(
    mut spawn: impl FnMut() -> std::io::Result<T>,
) -> std::io::Result<T> {
    for ms in TEXT_FILE_BUSY_BACKOFF_MS {
        match spawn() {
            Err(e) if is_text_file_busy(&e) => std::thread::sleep(Duration::from_millis(ms)),
            other => return other,
        }
    }
    spawn()
}

/// Run `<binary> check-config --for-update --config <config_path>` and report
/// whether it exited zero. `Ok(false)` (not an `Err`) is a *successful* check
/// that came back negative — the binary itself ran fine, it just rejects this
/// host's config — which is exactly the distinction callers need to choose
/// between retrying (`Err`, something went wrong running the check at all) and
/// deferring (`Ok(false)`, the check ran and said no).
///
/// `--for-update` holds the config to what the staged release needs to start,
/// never to policy: see [`config_errors`]. A staged binary is always newer than
/// this one, and every release from [`MIN_SYSTEM_LAYOUT_VERSION`] on knows the
/// flag.
async fn run_check_config(binary: &Path, config_path: &Path) -> std::io::Result<bool> {
    let status = retry_text_file_busy(|| {
        Command::new(binary)
            .args(check_config_args(config_path))
            .status()
    })
    .await?;
    Ok(status.success())
}

/// The argv after the binary for [`run_check_config`]. Public so the CLI's own
/// tests prove this exact argv parses as `check-config --for-update`.
pub fn check_config_args(config_path: &Path) -> Vec<std::ffi::OsString> {
    vec![
        "check-config".into(),
        "--for-update".into(),
        "--config".into(),
        config_path.as_os_str().to_owned(),
    ]
}

/// How long the staged binary gets to print its version. It does no I/O to
/// speak of; this only bounds a binary that hangs.
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// The version binding (ADR-0031 step 3, issue #91): run the staged binary's
/// `--version` and require [`check_reported_version`] to accept it. Runs
/// untrusted code, so with a cleared environment (no tokens), no stdin, and a
/// bounded wait; the child is killed if the wait expires.
async fn verify_binary_version(binary: &Path, tag: &str, commit: &str) -> Result<(), String> {
    probe_reported_version("faber", binary, tag, commit).await
}

/// [`verify_binary_version`] for the staged launcher: `faber-vm <v>+g<sha>`,
/// bound to the same tag and commit as the daemon.
async fn verify_launcher_version(binary: &Path, tag: &str, commit: &str) -> Result<(), String> {
    probe_reported_version(LAUNCHER_PROGRAM, binary, tag, commit).await
}

/// [`verify_binary_version`] for the staged guest agent: `faber-agent
/// <v>+g<sha> protocol=<n>`, bound to the same tag and commit as the daemon.
async fn verify_agent_version(binary: &Path, tag: &str, commit: &str) -> Result<(), String> {
    let stdout = run_version(AGENT_PROGRAM, binary, VERSION_PROBE_TIMEOUT).await?;
    check_reported_agent_version(&stdout, tag, commit)
}

async fn probe_reported_version(
    program: &str,
    binary: &Path,
    tag: &str,
    commit: &str,
) -> Result<(), String> {
    let stdout = run_version(program, binary, VERSION_PROBE_TIMEOUT).await?;
    check_reported_version_for(program, &stdout, tag, commit)
}

/// Run `<binary> --version` the way a staged binary must be run (see
/// [`verify_binary_version`]) and return its stdout.
async fn run_version(program: &str, binary: &Path, timeout: Duration) -> Result<String, String> {
    let mut cmd = Command::new(binary);
    cmd.arg("--version")
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let out = match tokio::time::timeout(timeout, retry_text_file_busy(|| cmd.output())).await {
        Err(_) => {
            return Err(format!(
                "version binding: the staged {program} did not print its version within {}s",
                timeout.as_secs()
            ))
        }
        Ok(Err(e)) => return Err(format!("version binding: running {program} --version: {e}")),
        Ok(Ok(out)) => out,
    };
    if !out.status.success() {
        return Err(format!(
            "version binding: {program} --version exited with {}",
            out.status
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// What stage time learned and commit time re-checks.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Downloaded {
    commit: String,
    binary_sha256: String,
    launcher_sha256: Option<String>,
    agent_sha256: Option<String>,
}

/// An asset the release must carry on this platform; missing may be a release
/// still mid-upload, so Unavailable.
fn required_asset<'a>(release: &'a Release, name: &str) -> Result<&'a ReleaseAsset, StageFailure> {
    find_asset(release, name).ok_or_else(|| {
        StageFailure::unavailable(format!("release {} has no {name} asset", release.tag_name))
    })
}

/// Check launcher bytes against the attested `SHA256SUMS`; the digest is what
/// the commit-time re-hash compares against. A mismatch is a verdict: Refused.
fn verify_launcher_digest(
    sums: &std::collections::HashMap<String, String>,
    name: &str,
    bytes: &[u8],
) -> Result<String, StageFailure> {
    verify_digest(sums, name, bytes).map_err(StageFailure::refused)?;
    Ok(sha256_hex(bytes))
}

/// Write everything verified to `update/staged/`. Called only once every asset
/// has verified, so a valid daemon is never paired with a bad companion.
async fn write_staged(
    paths: &UpdatePaths,
    bin: &[u8],
    app: Option<&[u8]>,
    launcher: Option<&[u8]>,
    agent: Option<&[u8]>,
) -> Result<(), StageFailure> {
    let staged_dir = paths.root.join("update/staged");
    tokio::fs::create_dir_all(&staged_dir).await.map_err(|e| {
        StageFailure::unavailable(format!("creating {}: {e}", staged_dir.display()))
    })?;
    tokio::fs::write(paths.staged_bin(), bin)
        .await
        .map_err(|e| StageFailure::unavailable(format!("writing staged binary: {e}")))?;
    if let Some(bytes) = app {
        tokio::fs::write(paths.staged_app(), bytes)
            .await
            .map_err(|e| StageFailure::unavailable(format!("writing staged app bundle: {e}")))?;
    }
    if let Some(bytes) = launcher {
        // Under SYSTEM setup made this directory, and `_faber` cannot create
        // it (its parent is root's): a missing one fails here, as unavailable.
        tokio::fs::create_dir_all(&paths.launcher_dir)
            .await
            .map_err(|e| {
                StageFailure::unavailable(format!("creating {}: {e}", paths.launcher_dir.display()))
            })?;
        tokio::fs::write(paths.staged_launcher(), bytes)
            .await
            .map_err(|e| StageFailure::unavailable(format!("writing staged launcher: {e}")))?;
    }
    if let Some(bytes) = agent {
        tokio::fs::write(paths.staged_agent(), bytes)
            .await
            .map_err(|e| StageFailure::unavailable(format!("writing staged agent: {e}")))?;
    }
    Ok(())
}

/// Remove every staged file: the one place a failed or deferred stage cleans up.
async fn remove_staged(paths: &UpdatePaths) {
    let _ = tokio::fs::remove_file(paths.staged_bin()).await;
    let _ = tokio::fs::remove_file(paths.staged_app()).await;
    let _ = tokio::fs::remove_file(paths.staged_launcher()).await;
    let _ = tokio::fs::remove_file(paths.staged_agent()).await;
}

/// Snapshot the running launcher to `faber-vm.prev`. Absent is fine (the first
/// release that ships one); then a stale snapshot from an older release is
/// removed, so a later rollback cannot restore the wrong launcher.
async fn snapshot_launcher(paths: &UpdatePaths) -> std::io::Result<()> {
    if tokio::fs::symlink_metadata(paths.launcher()).await.is_ok() {
        atomic_install(&paths.launcher(), &paths.prev_launcher()).await?;
    } else {
        let _ = tokio::fs::remove_file(paths.prev_launcher()).await;
    }
    Ok(())
}

/// Put `faber-vm.prev` back over the launcher, by temp file and rename (the
/// launcher may be running VMs: never written in place). `Ok(false)` when there
/// is no snapshot; the current launcher is then left as it is, never deleted.
fn restore_launcher(paths: &UpdatePaths) -> std::io::Result<bool> {
    if !paths.prev_launcher().exists() {
        return Ok(false);
    }
    atomic_install_sync(&paths.prev_launcher(), &paths.launcher())?;
    Ok(true)
}

/// Snapshot the running agent to `faber-agent.prev`, like [`snapshot_launcher`].
async fn snapshot_agent(paths: &UpdatePaths) -> std::io::Result<()> {
    if tokio::fs::symlink_metadata(paths.agent()).await.is_ok() {
        atomic_install(&paths.agent(), &paths.prev_agent()).await?;
    } else {
        let _ = tokio::fs::remove_file(paths.prev_agent()).await;
    }
    Ok(())
}

/// Put `faber-agent.prev` back over the agent, like [`restore_launcher`].
fn restore_agent(paths: &UpdatePaths) -> std::io::Result<bool> {
    if !paths.prev_agent().exists() {
        return Ok(false);
    }
    atomic_install_sync(&paths.prev_agent(), &paths.agent())?;
    Ok(true)
}

/// Commit-time re-check of the staged agent: re-hash and re-probe.
async fn recheck_staged_agent(staged_agent: &Path, staged: &Staged) -> Result<(), String> {
    let want = staged
        .agent_sha256
        .as_deref()
        .ok_or_else(|| "no agent digest was recorded at stage time".to_string())?;
    let bytes = tokio::fs::read(staged_agent)
        .await
        .map_err(|e| format!("reading {}: {e}", staged_agent.display()))?;
    let got = sha256_hex(&bytes);
    if !got.eq_ignore_ascii_case(want) {
        return Err(format!(
            "sha256 is {got}, but {} verified {want}",
            staged.tag
        ));
    }
    verify_agent_version(staged_agent, &staged.tag, &staged.commit).await
}

/// Commit-time re-check of the staged launcher: re-hash and re-probe.
async fn recheck_staged_launcher(staged_launcher: &Path, staged: &Staged) -> Result<(), String> {
    let want = staged
        .launcher_sha256
        .as_deref()
        .ok_or_else(|| "no launcher digest was recorded at stage time".to_string())?;
    let bytes = tokio::fs::read(staged_launcher)
        .await
        .map_err(|e| format!("reading {}: {e}", staged_launcher.display()))?;
    let got = sha256_hex(&bytes);
    if !got.eq_ignore_ascii_case(want) {
        return Err(format!(
            "sha256 is {got}, but {} verified {want}",
            staged.tag
        ));
    }
    verify_launcher_version(staged_launcher, &staged.tag, &staged.commit).await
}

/// Commit-time re-check of the staged binary against what stage time verified.
async fn recheck_staged_binary(staged_bin: &Path, staged: &Staged) -> Result<(), String> {
    let bytes = tokio::fs::read(staged_bin)
        .await
        .map_err(|e| format!("reading {}: {e}", staged_bin.display()))?;
    let got = sha256_hex(&bytes);
    if !got.eq_ignore_ascii_case(&staged.binary_sha256) {
        return Err(format!(
            "sha256 is {got}, but {} verified {}",
            staged.tag, staged.binary_sha256
        ));
    }
    verify_binary_version(staged_bin, &staged.tag, &staged.commit).await
}

// ---------------------------------------------------------------------------
// Launcher probation (ADR-0026)
// ---------------------------------------------------------------------------

/// How long `faber-vm validate --wall-only` gets. It builds a configuration and
/// checks it against the wall; it never boots anything.
const LAUNCHER_VALIDATE_TIMEOUT: Duration = Duration::from_secs(30);

/// What the launcher's boot-probation check concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LauncherProbation {
    /// Same build as the daemon, and its wall check passes.
    Passed,
    /// A definite failure of the launcher itself: roll the release back.
    Failed(String),
    /// The check could not be made (the fixture could not be created): not
    /// evidence against the launcher, so nothing is rolled back.
    Unproven(String),
}

/// Remove a throwaway directory on drop.
struct ScratchDir(PathBuf);

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A private `0700` directory under `parent`, holding a `0700` fixture bundle
/// with a sparse 64 MiB `disk.raw` (as `launcher/ci.sh` builds one). `validate`
/// refuses a bundle, or a parent, that is not this account's or is group/other
/// writable, hence the modes and the nesting. Returns `(scratch, bundle)`.
fn make_probe_bundle(parent: &Path) -> std::io::Result<(ScratchDir, PathBuf)> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::create_dir_all(parent)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let dir = parent.join(format!("launcher-probe-{}-{nanos}", std::process::id()));
    let mut builder = std::fs::DirBuilder::new();
    builder.mode(0o700);
    builder.create(&dir)?;
    let scratch = ScratchDir(dir);
    let bundle = scratch.0.join("bundle");
    builder.create(&bundle)?;
    let disk = std::fs::File::create(bundle.join("disk.raw"))?;
    disk.set_len(64 * 1024 * 1024)?;
    Ok((scratch, bundle))
}

/// The probation check for a freshly booted daemon that has a launcher
/// (ADR-0026 amendment). The launcher must be:
///
/// 1. the same build as this daemon: `<launcher> --version` is exactly
///    `faber-vm <daemon_version>` (the part of the daemon's own `--version`
///    after `faber `), and
/// 2. able to build a configuration and hold it to the wall: `validate` over a
///    throwaway bundle exits 0 and prints a `wall=ok` line.
///
/// `scratch_parent` is where the fixture is made, a directory the daemon's
/// account owns. Runs the launcher with a cleared environment, no stdin and
/// bounded waits.
pub async fn launcher_probation(
    launcher: &Path,
    daemon_version: &str,
    scratch_parent: &Path,
) -> LauncherProbation {
    launcher_probation_with(
        launcher,
        daemon_version,
        scratch_parent,
        VERSION_PROBE_TIMEOUT,
        LAUNCHER_VALIDATE_TIMEOUT,
    )
    .await
}

async fn launcher_probation_with(
    launcher: &Path,
    daemon_version: &str,
    scratch_parent: &Path,
    version_timeout: Duration,
    validate_timeout: Duration,
) -> LauncherProbation {
    use LauncherProbation::{Failed, Passed, Unproven};
    if !launcher.is_file() {
        return Failed(format!(
            "this release ships a launcher, but {} is missing",
            launcher.display()
        ));
    }
    let reported = match run_version(LAUNCHER_PROGRAM, launcher, version_timeout).await {
        Ok(out) => out,
        Err(e) => return Failed(e),
    };
    let want = format!("{LAUNCHER_PROGRAM} {daemon_version}");
    if reported.trim() != want {
        return Failed(format!(
            "the launcher reports {:?}, but it must be the daemon's own build: {want:?}",
            reported.trim()
        ));
    }

    let (scratch, bundle) = match make_probe_bundle(scratch_parent) {
        Ok(v) => v,
        Err(e) => {
            return Unproven(format!(
                "could not create the validate fixture under {}: {e}",
                scratch_parent.display()
            ))
        }
    };
    let mut cmd = Command::new(launcher);
    cmd.arg("validate")
        .arg(&bundle)
        .args(["--slot", "0", "--network", "shared-nat", "--wall-only"])
        .env_clear()
        .env("TMPDIR", &scratch.0)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    let out =
        match tokio::time::timeout(validate_timeout, retry_text_file_busy(|| cmd.output())).await {
            Err(_) => {
                return Failed(format!(
                    "launcher validate did not finish within {}s",
                    validate_timeout.as_secs()
                ))
            }
            Ok(Err(e)) => return Failed(format!("running launcher validate: {e}")),
            Ok(Ok(out)) => out,
        };
    if !out.status.success() {
        return Failed(format!(
            "launcher validate exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    if !String::from_utf8_lossy(&out.stdout)
        .lines()
        // The real line is `wall=ok mac=<mac>` (launcher/Sources/Commands.swift),
        // so match the token, not the whole line.
        .any(|l| l.split_whitespace().next() == Some("wall=ok"))
    {
        return Failed("launcher validate exited 0 but printed no wall=ok line".to_string());
    }
    Passed
}

/// Ask the VM supervisor to exit so launchd restarts it on the new launcher
/// (running VMs are adopted), and wait for one reporting `version`. Called where
/// probation's launcher check has passed. Whatever the outcome the launcher
/// stays: running VMs are unaffected, so a failure is shown, not rolled back.
async fn request_supervisor_restart<R: crate::vm::restart::SupervisorRestart>(
    restart: &R,
    version: &str,
    handle: &UpdateHandle,
) {
    use crate::vm::restart::{RestartError, RestartOutcome};
    let failed = |status: &str, detail: String| {
        warn!("VM supervisor restart onto launcher {version}: {status}: {detail}");
        handle.set_supervisor_restart(Some(SupervisorRestartView {
            status: status.to_string(),
            detail,
            at_ms: now_ms(),
        }));
    };
    match restart.restart_supervisor(version).await {
        Ok(RestartOutcome::Restarted { version: v }) => {
            info!(
                "VM supervisor restarted onto launcher {version} (reports {})",
                v.as_str()
            );
            handle.set_supervisor_restart(None);
        }
        Ok(RestartOutcome::TimedOut) => failed(
            "timed_out",
            format!("no supervisor answered as {version} before the deadline"),
        ),
        Ok(RestartOutcome::WrongVersion { seen }) => failed(
            "wrong_version",
            format!(
                "the supervisor still reports {}, not {version}",
                seen.as_str()
            ),
        ),
        Err(e @ RestartError::TooSoon { .. }) => failed("too_soon", e.to_string()),
        Err(e @ RestartError::NotAsked(_)) => failed("not_asked", e.to_string()),
    }
}

/// The production hook: the process-wide supervisor client, when this host has a
/// `_faber_vm` supervisor at all, then ADR-0051's session supervisor where the
/// record names the session account. Each [`ClientRestart`](crate::vm::restart::ClientRestart)
/// is process-wide too, so its `min_interval` really holds across calls.
async fn request_production_supervisor_restart(version: &str, handle: &UpdateHandle) {
    static RESTART: std::sync::OnceLock<crate::vm::restart::ClientRestart<'static>> =
        std::sync::OnceLock::new();
    static SESSION: std::sync::OnceLock<crate::vm::restart::ClientRestart<'static>> =
        std::sync::OnceLock::new();
    let Some(client) = crate::vm::job::process_client() else {
        info!("no VM supervisor on this host; nothing to restart onto launcher {version}");
        return;
    };
    let restart = RESTART.get_or_init(|| {
        crate::vm::restart::ClientRestart::new(client, crate::vm::restart::RestartPolicy::default())
    });
    request_supervisor_restart(restart, version, handle).await;
    if let Some(session) = crate::vm::job::session_client() {
        let restart = SESSION.get_or_init(|| {
            crate::vm::restart::ClientRestart::new(
                session,
                crate::vm::restart::RestartPolicy::default(),
            )
        });
        request_session_supervisor_restart(session, restart, version, handle).await;
    }
}

/// The session supervisor's restart onto the new launcher (ADR-0051). The
/// daemon is `_faber`, not root, so it cannot `launchctl kickstart` another
/// account's GUI domain: it asks the supervisor to `exit` over the protocol, as
/// for `_faber_vm`'s, and the agent's `KeepAlive` relaunches it on the new
/// binary. A session that is down needs nothing: the agent starts on the new
/// launcher at the next login, so a failed handshake is not a failed restart.
async fn request_session_supervisor_restart<C, R>(
    target: &C,
    restart: &R,
    version: &str,
    handle: &UpdateHandle,
) where
    C: crate::vm::session::Handshake,
    R: crate::vm::restart::SupervisorRestart,
{
    match target.reach().await {
        Ok(()) => {
            // One view for both supervisors: the session's success must not
            // clear a failure `_faber_vm`'s restart just reported.
            let before = handle.view().supervisor_restart;
            request_supervisor_restart(restart, version, handle).await;
            if handle.view().supervisor_restart.is_none() {
                handle.set_supervisor_restart(before);
            }
        }
        Err(why) => info!(
            "no macOS VM session to restart onto launcher {version} ({why}): its supervisor \
             starts on it at the next login"
        ),
    }
}

/// What boot probation concluded about the launcher.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LauncherProof {
    /// A definite failure armed a rollback: the caller must exit.
    Armed,
    /// The launcher is this build and holds the wall.
    Passed,
    /// Not on probation, no launcher shipped, or the check could not finish.
    NotChecked,
}

/// Boot-probation proof of the launcher. [`LauncherProof::Armed`] means a
/// rollback is armed, so the caller must exit and let the supervisor restart
/// into the restore.
///
/// Only while a marker exists for `version` (this boot is on probation) and only
/// on a platform whose release ships a launcher. A definite failure calls
/// [`force_rollback`] exactly as a refused self-check does.
pub async fn prove_launcher(paths: &UpdatePaths, version: &str) -> LauncherProof {
    prove_launcher_with(paths, version, LAUNCHER_ASSET_NAME.is_some()).await
}

async fn prove_launcher_with(
    paths: &UpdatePaths,
    version: &str,
    ships_launcher: bool,
) -> LauncherProof {
    if !ships_launcher {
        return LauncherProof::NotChecked;
    }
    let on_probation = std::fs::read_to_string(paths.marker())
        .ok()
        .and_then(|t| serde_json::from_str::<Marker>(&t).ok())
        .is_some_and(|m| parse_version(&m.to) == parse_version(version));
    if !on_probation {
        return LauncherProof::NotChecked;
    }
    match launcher_probation(&paths.launcher(), version, &paths.root.join("update")).await {
        LauncherProbation::Passed => LauncherProof::Passed,
        LauncherProbation::Unproven(reason) => {
            warn!("launcher probation for {version} could not be completed, keeping it: {reason}");
            LauncherProof::NotChecked
        }
        LauncherProbation::Failed(reason) => {
            warn!("launcher probation for {version} failed, rolling back: {reason}");
            if !force_rollback(paths) {
                warn!("launcher probation: no boot marker to roll back from; staying on {version}");
                return LauncherProof::NotChecked;
            }
            LauncherProof::Armed
        }
    }
}

/// The last step of boot probation: ask the VM supervisor to restart onto the
/// new launcher. Call it only once the whole probation has settled (the updater
/// self-check too), so a later rollback cannot leave the supervisor running a
/// launcher the daemon has just been rolled back from. Does nothing unless the
/// launcher passed.
pub async fn restart_supervisor_after_proof(
    proof: LauncherProof,
    version: &str,
    handle: &UpdateHandle,
) {
    restart_after_proof_with(
        proof,
        request_production_supervisor_restart(version, handle),
    )
    .await;
}

/// `restart` is a future so tests can substitute a fake
/// [`SupervisorRestart`](crate::vm::restart::SupervisorRestart); it is awaited
/// only for [`LauncherProof::Passed`].
async fn restart_after_proof_with(
    proof: LauncherProof,
    restart: impl std::future::Future<Output = ()>,
) {
    if proof == LauncherProof::Passed {
        restart.await;
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    /// The asset a host asks a release for must match what `deploy/release.sh`
    /// publishes for that host's platform — a mismatch does not fail loudly, it
    /// just means the host never updates again, which is invisible until
    /// someone audits versions. On aarch64 macOS it must still be byte-identical
    /// to the name v0.3.x hosts have hardcoded, or cutting the next release
    /// silently strands the existing fleet.
    #[test]
    fn daemon_asset_name_is_derived_from_the_build_target() {
        assert!(
            DAEMON_ASSET_NAME.starts_with("faber-"),
            "got {DAEMON_ASSET_NAME}"
        );
        // Guards build.rs's `unwrap_or_else(|| "unknown")` fallback, and only
        // that. It cannot be a `contains` check: the vendor field of a perfectly
        // ordinary Linux triple is literally `unknown`
        // (`x86_64-unknown-linux-gnu`), which this test previously rejected.
        assert_ne!(
            DAEMON_ASSET_NAME, "faber-unknown",
            "build.rs did not stamp a target triple"
        );

        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert_eq!(
            DAEMON_ASSET_NAME, "faber-aarch64-apple-darwin",
            "changing this strands every already-installed macOS host"
        );

        #[cfg(target_os = "linux")]
        assert!(
            DAEMON_ASSET_NAME.contains("linux"),
            "got {DAEMON_ASSET_NAME}"
        );
    }

    /// The menu-bar app is a macOS bundle. Requiring its asset on a platform
    /// that publishes none would fail every update with "release has no asset"
    /// and pin the host to its installed version forever.
    #[test]
    fn app_asset_is_expected_only_where_an_app_exists() {
        #[cfg(target_os = "macos")]
        {
            let name = APP_ASSET_NAME.expect("macOS ships a menu-bar app bundle");
            assert!(name.starts_with("faber-app-"), "got {name}");
            assert!(name.ends_with(".tar.gz"), "got {name}");
        }
        #[cfg(not(target_os = "macos"))]
        assert!(
            APP_ASSET_NAME.is_none(),
            "only macOS has a menu-bar app bundle to refresh"
        );
    }

    /// Like the app asset, the launcher is macOS-only: requiring it on Linux
    /// would strand every Linux host on "release has no asset".
    #[test]
    fn launcher_asset_is_expected_only_where_a_launcher_exists() {
        #[cfg(target_os = "macos")]
        {
            let name = LAUNCHER_ASSET_NAME.expect("macOS ships the VM launcher");
            assert!(name.starts_with("faber-launcher-"), "got {name}");
            // The attest job checksums `faber-*`.
            assert!(name.starts_with("faber-"));
        }
        #[cfg(not(target_os = "macos"))]
        assert!(
            LAUNCHER_ASSET_NAME.is_none(),
            "only macOS has a VM launcher to refresh"
        );
        #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
        assert_eq!(
            LAUNCHER_ASSET_NAME,
            Some("faber-launcher-aarch64-apple-darwin"),
            "release.yml stages exactly this name"
        );
    }

    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    // -- test helpers --------------------------------------------------

    /// A private scratch directory under the OS temp dir, unique per call and
    /// removed on drop — the stand-in for `$ROOT` that makes
    /// `rollback_if_needed` and friends exercisable without a real `$HOME`.
    /// No `tempfile` dependency exists in this crate, so this is hand-rolled.
    struct TempRoot {
        path: PathBuf,
    }

    impl TempRoot {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let n = COUNTER.fetch_add(1, Ordering::Relaxed);
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let path = std::env::temp_dir().join(format!("faber-update-test-{tag}-{nanos}-{n}"));
            std::fs::create_dir_all(&path).unwrap();
            Self { path }
        }

        fn paths(&self) -> UpdatePaths {
            UpdatePaths::with_root(self.path.clone())
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn write_file(path: &Path, contents: &str) {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(path, contents).unwrap();
    }

    fn policy(enabled: bool, escalate_after_secs: u64) -> UpdatePolicy {
        UpdatePolicy {
            enabled,
            escalate_after_secs,
            ..UpdatePolicy::default()
        }
    }

    #[test]
    fn decide_disabled_sleeps() {
        let state = UpdateState::default();
        assert_eq!(
            decide(1_000_000, &policy(false, 3600), &state, 5, false),
            Action::Sleep
        );
    }

    #[test]
    fn decide_staged_and_idle_commits() {
        let state = UpdateState {
            staged: Some(Staged {
                version: "0.3.0".into(),
                since_ms: 0,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            decide(1_000_000, &policy(true, 3600), &state, 0, false),
            Action::Commit
        );
    }

    #[test]
    fn decide_staged_busy_inside_escalate_window_sleeps() {
        let state = UpdateState {
            staged: Some(Staged {
                version: "0.3.0".into(),
                since_ms: 1_000_000,
                ..Default::default()
            }),
            ..Default::default()
        };
        // 100s elapsed against a 1h escalate window — nowhere near due.
        assert_eq!(
            decide(1_100_000, &policy(true, 3600), &state, 3, false),
            Action::Sleep
        );
    }

    #[test]
    fn decide_staged_busy_past_escalate_window_escalates() {
        let state = UpdateState {
            staged: Some(Staged {
                version: "0.3.0".into(),
                since_ms: 1_000_000,
                ..Default::default()
            }),
            ..Default::default()
        };
        // escalate_after_secs = 3600s = 3_600_000ms; elapsed exactly that.
        assert_eq!(
            decide(1_000_000 + 3_600_000, &policy(true, 3600), &state, 3, false),
            Action::Escalate
        );
    }

    #[test]
    fn decide_zero_escalate_secs_never_escalates() {
        let state = UpdateState {
            staged: Some(Staged {
                version: "0.3.0".into(),
                since_ms: 0,
                ..Default::default()
            }),
            ..Default::default()
        };
        // An enormous elapsed time still never escalates when the knob is 0.
        assert_eq!(
            decide(u64::MAX / 2, &policy(true, 0), &state, 1, false),
            Action::Sleep
        );
    }

    #[test]
    fn decide_apply_now_escalates_even_inside_the_window() {
        let state = UpdateState {
            staged: Some(Staged {
                version: "0.3.0".into(),
                since_ms: 1_000_000,
                ..Default::default()
            }),
            apply_requested: true,
            ..Default::default()
        };
        // Only 1s elapsed — nowhere near escalate_after_secs — but apply_now
        // was called.
        assert_eq!(
            decide(1_001_000, &policy(true, 3600), &state, 3, false),
            Action::Escalate
        );
    }

    /// A pin moved off what is staged, or a skip of it, discards it on the next
    /// tick, busy or idle and ahead of Apply now: waiting for idle, or draining,
    /// would install nothing. Updates being off only sleep, which keeps it.
    #[test]
    fn decide_discards_a_staged_update_the_operator_turned_down() {
        let state = UpdateState {
            staged: Some(Staged {
                version: "0.3.0".into(),
                since_ms: 0,
                ..Default::default()
            }),
            apply_requested: true,
            ..Default::default()
        };
        let skipped = UpdateState {
            quarantined: vec!["0.3.0".into()],
            ..state.clone()
        };
        let pinned = |pin: &str, enabled| UpdatePolicy {
            pin: Some(pin.into()),
            ..policy(enabled, 3600)
        };
        let cases = [
            (pinned("0.3.1", true), &state, "0.3.1"),
            (policy(true, 3600), &skipped, "skipped"),
        ];
        for (live, state, why) in &cases {
            for (inflight, adoptable) in [(0, false), (3, false), (3, true)] {
                let action = decide(1_000_000, live, state, inflight, adoptable);
                assert!(
                    matches!(
                        &action,
                        Action::Discard { version, reason } if version == "0.3.0" && reason.contains(why)
                    ),
                    "{why}, {inflight} in flight, adoptable {adoptable}: {action:?}"
                );
            }
        }
        assert_eq!(
            decide(1_000_000, &pinned("v0.3.0", true), &state, 0, false),
            Action::Commit
        );
        assert_eq!(
            decide(1_000_000, &pinned("0.3.1", false), &state, 0, false),
            Action::Sleep
        );
    }

    #[test]
    fn decide_never_stages_a_quarantined_version() {
        let state = UpdateState {
            available: Some("0.3.0".into()),
            quarantined: vec!["0.3.0".into()],
            ..Default::default()
        };
        assert_eq!(
            decide(1_000_000, &policy(true, 3600), &state, 0, false),
            Action::Sleep
        );
    }

    #[test]
    fn decide_stages_a_non_quarantined_available_version() {
        let state = UpdateState {
            available: Some("0.3.0".into()),
            available_since_ms: Some(0),
            quarantined: vec!["0.2.5".into()],
            ..Default::default()
        };
        let day_ms = 86_400_000;
        assert_eq!(
            decide(day_ms, &policy(true, 3600), &state, 0, false),
            Action::Stage("0.3.0".into())
        );
    }

    // -- maturity (ADR-0031, #121) -----------------------------------------

    #[test]
    fn absent_maturity_key_is_24_hours() {
        let p: UpdatePolicy = toml::from_str("enabled = true").unwrap();
        assert_eq!(p.maturity_secs, 86_400);
        assert_eq!(UpdatePolicy::default().effective_maturity_secs(), 86_400);
    }

    #[test]
    fn a_sub_floor_maturity_falls_back_to_24h_not_zero() {
        let p = UpdatePolicy {
            maturity_secs: 600,
            ..UpdatePolicy::default()
        };
        assert_eq!(p.effective_maturity_secs(), 86_400);
        let ok = UpdatePolicy {
            maturity_secs: 3600,
            ..UpdatePolicy::default()
        };
        assert_eq!(ok.effective_maturity_secs(), 3600);
    }

    #[test]
    fn an_immature_candidate_is_not_staged_and_keeps_polling() {
        let state = UpdateState {
            available: Some("0.3.0".into()),
            available_since_ms: Some(1_000),
            last_check_ms: Some(1_000 + 86_400_000 - 5_000),
            ..Default::default()
        };
        let p = policy(true, 3600);
        let due = 1_000 + 86_400_000;
        assert_eq!(install_after_ms(&state, &p), Some(due));
        // Inside the wait: never Stage; the poll still runs once due.
        assert_eq!(decide(due - 1, &p, &state, 0, false), Action::Sleep);
        let stale = UpdateState {
            last_check_ms: Some(1_000),
            ..state.clone()
        };
        assert_eq!(
            decide(1_000 + 1_800_000, &p, &stale, 0, false),
            Action::Check
        );
        // Matured: stage.
        assert_eq!(
            decide(due, &p, &state, 0, false),
            Action::Stage("0.3.0".into())
        );
    }

    #[test]
    fn a_candidate_with_no_known_clock_is_not_staged() {
        let state = UpdateState {
            available: Some("0.3.0".into()),
            last_check_ms: Some(u64::MAX / 2),
            ..Default::default()
        };
        assert_eq!(
            decide(u64::MAX / 2, &policy(true, 3600), &state, 0, false),
            Action::Sleep
        );
    }

    #[test]
    fn a_pinned_policy_waits_too() {
        let state = UpdateState {
            available: Some("0.3.0".into()),
            available_since_ms: Some(0),
            last_check_ms: Some(10),
            ..Default::default()
        };
        let p = UpdatePolicy {
            pin: Some("0.3.0".into()),
            ..policy(true, 3600)
        };
        assert_eq!(decide(10, &p, &state, 0, false), Action::Sleep);
    }

    #[test]
    fn clock_is_the_later_of_published_and_first_seen() {
        // Published long ago, flipped visible (first seen) just now: from first seen.
        assert_eq!(maturity_clock_ms(Some(1_000), 5_000_000), 5_000_000);
        // Published after first sight (re-published): from published.
        assert_eq!(maturity_clock_ms(Some(9_000), 5_000), 9_000);
        // Unparsable/missing published_at: first seen.
        assert_eq!(maturity_clock_ms(None, 5_000), 5_000);
    }

    #[test]
    fn first_seen_survives_a_restart_and_resets_on_candidate_change() {
        let root = TempRoot::new("first-seen");
        let path = root.paths().first_seen();
        assert_eq!(record_first_seen(&path, "v0.4.2", 1_000), 1_000);
        // "Restart": a later call (fresh process, same file) keeps the clock.
        assert_eq!(record_first_seen(&path, "v0.4.2", 9_999_999), 1_000);
        // A newer candidate starts its own clock.
        assert_eq!(record_first_seen(&path, "v0.4.3", 50_000), 50_000);
        // The old tag is forgotten: it would start over if it returned.
        assert_eq!(record_first_seen(&path, "v0.4.2", 70_000), 70_000);
        assert!(!root.path.join("update/first_seen.json.tmp").exists());
    }

    fn published(tag: &str, at: &str, prerelease: bool) -> Release {
        Release {
            published_at: Some(at.into()),
            prerelease,
            ..rel(tag)
        }
    }

    #[test]
    fn a_candidate_flipped_to_prerelease_fails_the_final_check() {
        let root = TempRoot::new("final-check");
        let path = root.paths().first_seen();
        let p = policy(true, 3600);
        let long_ago = "2020-01-01T00:00:00Z";
        let t0 = 1_700_000_000_000u64;
        let ok = [published("v0.9.0", long_ago, false)];
        assert!(!stage_still_valid(&ok, &p, "0.1.0", &path, t0, "0.9.0"));
        let later = t0 + 86_400_000;
        assert!(stage_still_valid(&ok, &p, "0.1.0", &path, later, "0.9.0"));
        // Flipped to pre-release between check and stage: nothing to stage.
        let flipped = [published("v0.9.0", long_ago, true)];
        assert!(!stage_still_valid(
            &flipped, &p, "0.1.0", &path, later, "0.9.0"
        ));
        // A different tag is not the one that matured.
        let newer = [
            published("v0.9.0", long_ago, false),
            published("v1.0.0", long_ago, false),
        ];
        assert!(!stage_still_valid(
            &newer, &p, "0.1.0", &path, later, "0.9.0"
        ));
    }

    #[test]
    fn a_withdrawn_tag_that_returns_starts_a_fresh_wait() {
        let root = TempRoot::new("withdrawn");
        let path = root.paths().first_seen();
        let rs = [rel("v0.9.0")];
        let (_, c1) = resolve_candidate(&rs, "0.1.0", None, &path, 1_000).unwrap();
        assert_eq!(c1, 1_000);
        assert!(resolve_candidate(&[], "0.1.0", None, &path, 5_000).is_none());
        assert!(!path.exists());
        let (_, c2) = resolve_candidate(&rs, "0.1.0", None, &path, 9_000).unwrap();
        assert_eq!(c2, 9_000);
    }

    #[test]
    fn a_future_first_seen_is_clamped_and_a_corrupt_file_means_now() {
        let root = TempRoot::new("clamp");
        let path = root.paths().first_seen();
        write_file(&path, r#"{"tags":{"v0.4.2":99999999999999}}"#);
        assert_eq!(record_first_seen(&path, "v0.4.2", 5_000), 5_000);
        write_file(&path, "not json");
        assert_eq!(record_first_seen(&path, "v0.4.2", 7_000), 7_000);
    }

    #[test]
    fn first_seen_written_by_one_updater_is_read_by_the_next() {
        let root = TempRoot::new("two-updaters");
        let a = root.paths();
        record_first_seen(&a.first_seen(), "v0.4.2", 1_000);
        // A new process builds fresh paths over the same root.
        let b = UpdatePaths::with_root(root.path.clone());
        assert_eq!(record_first_seen(&b.first_seen(), "v0.4.2", 50_000), 1_000);
    }

    #[test]
    fn view_reports_the_pending_countdown() {
        let state = UpdateState {
            available: Some("0.3.0".into()),
            available_since_ms: Some(now_ms()),
            ..Default::default()
        };
        let after = install_after_ms(&state, &policy(true, 3600)).unwrap();
        assert!(after > now_ms() + 86_000_000);
    }

    // -- select_candidate (ADR-0031, #120) --------------------------------

    fn rel(tag: &str) -> Release {
        Release {
            tag_name: tag.to_string(),
            draft: false,
            prerelease: false,
            published_at: None,
            assets: Vec::new(),
        }
    }

    #[test]
    fn release_paging_stops_on_a_short_page() {
        assert!(more_release_pages(100));
        assert!(!more_release_pages(99));
        assert!(!more_release_pages(0));
    }

    // -- the wire format the panel reads ---------------------------------

    #[test]
    fn the_view_serializes_the_shape_both_front_ends_read() {
        // `UpdateView` crosses a process boundary twice — into the daemon's own
        // dashboard and into the separately-deployed menu-bar app, which mirrors
        // this type by hand. Both read the phase as `phase.phase` plus whatever
        // that tag carries, so the internal tagging is a wire format, not an
        // implementation detail: changing it silently blanks the update row in
        // an app built against the old shape.
        let view = UpdateView {
            phase: UpdatePhase::WaitingIdle {
                version: "0.3.0".into(),
                since_ms: 1_700_000_000_000,
            },
            available: Some("0.3.0".into()),
            ..UpdateView::default()
        };
        let json = serde_json::to_value(&view).expect("view serializes");

        assert_eq!(json["phase"]["phase"], "waiting_idle");
        assert_eq!(json["phase"]["version"], "0.3.0");
        assert_eq!(json["available"], "0.3.0");
        for key in [
            "current",
            "enabled",
            "available",
            "candidate_reason",
            "phase",
            "last_check_ms",
            "last_error",
            "quarantined",
            "rolled_back_from",
            "pin",
        ] {
            assert!(json.get(key).is_some(), "the panel reads `{key}`");
        }

        // A default view still names the running binary: the version is the one
        // thing this feature must always be able to answer, even before the
        // first release check has happened.
        let idle = serde_json::to_value(UpdateView::default()).unwrap();
        assert_eq!(idle["current"], env!("FABER_VERSION"));
    }

    // -- stage retry schedule (#137) -------------------------------------

    const T0: u64 = 1_700_000_000_000;

    fn staging_policy() -> UpdatePolicy {
        UpdatePolicy {
            enabled: true,
            poll_interval_secs: 1800,
            ..UpdatePolicy::default()
        }
    }

    fn candidate_state(tag: &str) -> UpdateState {
        UpdateState {
            available: Some(tag.to_string()),
            // Long matured: the policy floors a short wait to 24 h.
            available_since_ms: Some(T0 - 2 * 86_400_000),
            last_check_ms: Some(T0),
            ..UpdateState::default()
        }
    }

    /// Walk `ticks` one-second ticks from T0, failing every stage attempt with
    /// `class`; returns the stage attempt times (not log lines).
    fn run_failing(
        state: &mut UpdateState,
        policy: &UpdatePolicy,
        tag: &str,
        class: FailClass,
        from_ms: u64,
        to_ms: u64,
    ) -> Vec<u64> {
        let mut attempts = Vec::new();
        let mut now = from_ms;
        while now <= to_ms {
            if let Action::Stage(v) = decide(now, policy, state, 0, false) {
                assert_eq!(v, tag);
                attempts.push(now);
                state.stage_retry = Some(stage_retry_after(
                    state.stage_retry.as_ref(),
                    &v,
                    class,
                    now,
                    policy.poll_interval_secs,
                ));
            }
            now += 1000;
        }
        attempts
    }

    #[test]
    fn a_refused_tag_is_not_restaged_within_the_poll_interval_but_is_after() {
        let policy = staging_policy();
        let mut state = candidate_state("0.4.2");
        let attempts = run_failing(
            &mut state,
            &policy,
            "0.4.2",
            FailClass::Refused,
            T0,
            T0 + 1_799_000,
        );
        assert_eq!(
            attempts,
            vec![T0],
            "one attempt, then held for the whole interval"
        );
        // At exactly the interval the tag is tried again.
        let attempts = run_failing(
            &mut state,
            &policy,
            "0.4.2",
            FailClass::Refused,
            T0 + 1_800_000,
            T0 + 1_800_000,
        );
        assert_eq!(attempts, vec![T0 + 1_800_000]);
        // And again held for another full interval.
        let attempts = run_failing(
            &mut state,
            &policy,
            "0.4.2",
            FailClass::Refused,
            T0 + 1_801_000,
            T0 + 3_599_000,
        );
        assert!(attempts.is_empty());
    }

    #[test]
    fn a_held_tag_still_polls_for_new_releases() {
        let policy = staging_policy();
        let mut state = candidate_state("0.4.2");
        state.stage_retry = Some(stage_retry_after(
            None,
            "0.4.2",
            FailClass::Refused,
            T0,
            1800,
        ));
        state.last_check_ms = Some(T0);
        assert_eq!(decide(T0 + 1000, &policy, &state, 0, false), Action::Sleep);
        assert_eq!(
            decide(T0 + 1_800_000, &policy, &state, 0, false),
            Action::Stage("0.4.2".into())
        );
        state.last_check_ms = Some(T0 - 1_800_000);
        assert_eq!(decide(T0 + 1000, &policy, &state, 0, false), Action::Check);
    }

    #[test]
    fn a_new_candidate_tag_is_tried_immediately() {
        let policy = staging_policy();
        let mut state = candidate_state("0.4.2");
        run_failing(&mut state, &policy, "0.4.2", FailClass::Refused, T0, T0);
        state.available = Some("0.4.3".into());
        assert_eq!(
            decide(T0 + 1000, &policy, &state, 0, false),
            Action::Stage("0.4.3".into())
        );
        // A failure on the new tag starts its own count.
        let r = stage_retry_after(
            state.stage_retry.as_ref(),
            "0.4.3",
            FailClass::Unavailable,
            T0,
            1800,
        );
        assert_eq!(r.failures, 1);
        assert_eq!(r.next_attempt_ms, T0 + 60_000);
    }

    #[test]
    fn unavailable_backs_off_60_120_240_and_caps_at_the_poll_interval() {
        let policy = staging_policy();
        let mut state = candidate_state("0.4.2");
        let attempts = run_failing(
            &mut state,
            &policy,
            "0.4.2",
            FailClass::Unavailable,
            T0,
            T0 + 8_000_000,
        );
        let gaps: Vec<u64> = attempts.windows(2).map(|w| (w[1] - w[0]) / 1000).collect();
        assert_eq!(gaps, vec![60, 120, 240, 480, 960, 1800, 1800, 1800]);
    }

    #[test]
    fn success_or_a_new_tag_resets_the_backoff() {
        let mut r = stage_retry_after(None, "0.4.2", FailClass::Unavailable, T0, 1800);
        for _ in 0..3 {
            r = stage_retry_after(Some(&r), "0.4.2", FailClass::Unavailable, T0, 1800);
        }
        assert_eq!(r.failures, 4);
        // New tag: counting restarts at the first step.
        let fresh = stage_retry_after(Some(&r), "0.4.3", FailClass::Unavailable, T0, 1800);
        assert_eq!((fresh.failures, fresh.next_attempt_ms), (1, T0 + 60_000));
        // Success clears the record, so nothing holds a later stage.
        let mut state = candidate_state("0.4.2");
        state.stage_retry = Some(r);
        state.stage_retry = None;
        assert_eq!(stage_held_until_ms(&state, "0.4.2", T0), None);
        assert_eq!(
            decide(T0, &staging_policy(), &state, 0, false),
            Action::Stage("0.4.2".into())
        );
    }

    #[test]
    fn a_small_poll_interval_caps_the_backoff_below_sixty() {
        let r = stage_retry_after(None, "0.4.2", FailClass::Unavailable, T0, 30);
        assert_eq!(r.next_attempt_ms, T0 + 30_000);
    }

    #[test]
    fn the_view_reports_when_the_next_attempt_is_due() {
        let root = TempRoot::new("retry-view");
        let handle = test_handle(root.paths(), "0.4.1");
        {
            let mut policy = handle.shared.policy.lock().unwrap();
            policy.enabled = true;
        }
        let due = now_ms() + 600_000;
        {
            let mut state = handle.shared.state.lock().unwrap();
            state.available = Some("0.4.2".into());
            state.stage_retry = Some(StageRetry {
                tag: "0.4.2".into(),
                class: FailClass::Refused,
                failures: 1,
                next_attempt_ms: due,
            });
        }
        assert_eq!(handle.view().next_attempt_ms, Some(due));
        handle.shared.state.lock().unwrap().stage_retry = None;
        assert_eq!(handle.view().next_attempt_ms, None);
    }

    // -- rollback_if_needed ----------------------------------------------

    // ADR-0011 §2 -----------------------------------------------------------

    #[test]
    fn decide_staged_and_busy_commits_when_the_host_is_adoptable() {
        // The whole point of the ADR: a busy host no longer has to wait six
        // hours and then drain, because the next process rebuilds these claims
        // from their own containers.
        let state = UpdateState {
            staged: Some(Staged {
                version: "0.3.0".into(),
                since_ms: 1_000_000,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            decide(1_100_000, &policy(true, 3600), &state, 7, true),
            Action::Commit
        );
    }

    #[test]
    fn decide_falls_back_to_the_old_wait_when_the_host_is_not_adoptable() {
        // A wedged Docker, a claim with no container, or a container minted before the
        // labels existed: the pre-ADR-0011 behaviour must survive intact, or the
        // escape hatch is not one.
        let state = UpdateState {
            staged: Some(Staged {
                version: "0.3.0".into(),
                since_ms: 1_000_000,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            decide(1_100_000, &policy(true, 3600), &state, 7, false),
            Action::Sleep
        );
        assert_eq!(
            decide(1_000_000 + 3_600_000, &policy(true, 3600), &state, 7, false),
            Action::Escalate
        );
    }

    // ADR-0011 §4 -----------------------------------------------------------

    #[test]
    fn a_deliberate_exit_does_not_count_against_probation() {
        let root = TempRoot::new("clean-exit");
        let paths = root.paths();
        let live_config = paths.root.join("config.toml");
        write_file(
            &paths.marker(),
            r#"{"from":"0.1.0","to":"0.2.0","attempts":1,"written_ms":1000}"#,
        );
        note_clean_exit(&paths);
        assert!(paths.clean_exit().exists());

        let report = rollback_if_needed(&paths, "0.2.0", &live_config).expect("marker present");
        // Consumed even though this boot had a marker to spend it on.
        assert!(!paths.clean_exit().exists());
        assert!(!report.restored, "a deliberate restart is not a crash-loop");

        let marker: Marker =
            serde_json::from_str(&std::fs::read_to_string(paths.marker()).unwrap()).unwrap();
        assert_eq!(marker.attempts, 1, "the attempt must not have been spent");
        // One-shot: the *next* boot has no excuse, so a real crash-loop still
        // reaches the rollback in the same number of unexplained boots as before.
        assert!(!paths.clean_exit().exists());
    }

    #[test]
    fn the_breadcrumb_never_survives_a_boot_that_had_no_marker() {
        // The staleness this guards: a deliberate restart with no update in
        // flight left the breadcrumb on disk, where it would have excused the
        // first boot of whatever update came next — silently handing a
        // crash-looping binary a third attempt.
        let root = TempRoot::new("stale-breadcrumb");
        let paths = root.paths();
        note_clean_exit(&paths);
        assert!(rollback_if_needed(&paths, "0.2.0", &paths.root.join("config.toml")).is_none());
        assert!(
            !paths.clean_exit().exists(),
            "a boot with nothing to excuse must still consume the excuse"
        );
    }

    #[test]
    fn a_crash_after_a_deliberate_exit_still_rolls_back() {
        // The regression this guards: an excuse that outlived its one boot would
        // make a broken binary un-rollbackable.
        let root = TempRoot::new("clean-then-crash");
        let paths = root.paths();
        let live_config = paths.root.join("config.toml");
        write_file(&paths.bin(), "new");
        write_file(&paths.prev_bin(), "old");
        write_file(
            &paths.marker(),
            r#"{"from":"0.1.0","to":"0.2.0","attempts":1,"written_ms":1000}"#,
        );

        note_clean_exit(&paths);
        rollback_if_needed(&paths, "0.2.0", &live_config).expect("boot 1: excused");
        // Boot 2: no breadcrumb, so this one counts -> attempts 2.
        rollback_if_needed(&paths, "0.2.0", &live_config).expect("boot 2: counted");
        // Boot 3: attempts >= 2 -> restore.
        let report = rollback_if_needed(&paths, "0.2.0", &live_config).expect("boot 3");
        assert!(report.restored, "a genuine crash-loop must still roll back");
        assert_eq!(std::fs::read_to_string(paths.bin()).unwrap(), "old");
    }

    #[test]
    fn rollback_no_marker_returns_none() {
        let root = TempRoot::new("no-marker");
        let paths = root.paths();
        let live_config = paths.root.join("config.toml");
        assert!(rollback_if_needed(&paths, "0.2.0", &live_config).is_none());
    }

    #[test]
    fn rollback_first_attempt_increments_without_restoring() {
        let root = TempRoot::new("first-attempt");
        let paths = root.paths();
        let live_config = paths.root.join("config.toml");
        write_file(
            &paths.marker(),
            r#"{"from":"0.1.0","to":"0.2.0","attempts":0,"written_ms":1000}"#,
        );

        let report = rollback_if_needed(&paths, "0.2.0", &live_config).expect("marker present");
        assert!(!report.restored);
        assert_eq!(report.from, "0.1.0");
        assert_eq!(report.to, "0.2.0");

        // Probation continues: the marker is still there, with the attempt
        // count bumped.
        assert!(paths.marker().exists());
        let marker: Marker =
            serde_json::from_str(&std::fs::read_to_string(paths.marker()).unwrap()).unwrap();
        assert_eq!(marker.attempts, 1);
    }

    #[test]
    fn rollback_crash_loop_restores_binary_and_config_and_quarantines() {
        let root = TempRoot::new("crash-loop");
        let paths = root.paths();
        // Deliberately *not* the default `$ROOT/config.toml`: a host started
        // with `--config elsewhere.toml` must have that file restored, so this
        // proves the restore uses the path it is given rather than one it
        // derives for itself.
        let live_config = paths.root.join("custom-location.toml");
        write_file(
            &paths.marker(),
            r#"{"from":"0.1.0","to":"0.2.0","attempts":2,"written_ms":1000}"#,
        );
        write_file(&paths.bin(), "bad-binary-bytes");
        write_file(&paths.prev_bin(), "old-binary-bytes");
        write_file(&paths.prev_config(), "old-config-toml");
        // The "live" config this host is actually running with.
        write_file(&live_config, "bad-config-toml");

        let report = rollback_if_needed(&paths, "0.2.0", &live_config).expect("marker present");
        assert!(report.restored);
        assert_eq!(report.from, "0.1.0");
        assert_eq!(report.to, "0.2.0");

        assert_eq!(
            std::fs::read_to_string(paths.bin()).unwrap(),
            "old-binary-bytes"
        );
        assert_eq!(
            std::fs::read_to_string(&live_config).unwrap(),
            "old-config-toml"
        );
        assert!(
            !paths.marker().exists(),
            "marker must be cleared on success"
        );
        assert_eq!(
            read_quarantine(&paths.quarantine()),
            vec!["0.2.0".to_string()]
        );
    }

    /// Rollback replaces by temp file and rename, never by a copy over the file
    /// the crash-looping process runs from: that file keeps its bytes, and
    /// `bin/faber` names a new one. The config goes back the same way.
    #[test]
    fn rollback_replaces_by_rename_so_the_old_file_survives() {
        use std::os::unix::fs::MetadataExt;
        let root = TempRoot::new("rollback-rename");
        let paths = root.paths();
        let live_config = paths.root.join("config.toml");
        write_file(&paths.bin(), "bad-binary-bytes");
        write_file(&paths.prev_bin(), "old-binary-bytes");
        write_file(&live_config, "bad-config-toml");
        write_file(&paths.prev_config(), "old-config-toml");
        std::fs::set_permissions(&live_config, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_file(
            &paths.marker(),
            r#"{"from":"0.1.0","to":"0.2.0","attempts":2,"written_ms":1000}"#,
        );
        // What the running process holds is the file, not the name.
        let running = root.path.join("running");
        std::fs::hard_link(paths.bin(), &running).unwrap();
        let running_config = root.path.join("running-config");
        std::fs::hard_link(&live_config, &running_config).unwrap();
        let inode = |p: &Path| std::fs::metadata(p).unwrap().ino();
        let (bin_before, config_before) = (inode(&paths.bin()), inode(&live_config));

        let report = rollback_if_needed(&paths, "0.2.0", &live_config).expect("marker present");
        assert!(report.restored);

        assert_eq!(
            std::fs::read_to_string(paths.bin()).unwrap(),
            "old-binary-bytes"
        );
        assert_ne!(inode(&paths.bin()), bin_before, "rewritten in place");
        assert_eq!(
            std::fs::read_to_string(&running).unwrap(),
            "bad-binary-bytes"
        );
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&paths.bin()), 0o755);
        assert_eq!(
            std::fs::read_to_string(&live_config).unwrap(),
            "old-config-toml"
        );
        assert_ne!(inode(&live_config), config_before, "rewritten in place");
        assert_eq!(
            std::fs::read_to_string(&running_config).unwrap(),
            "bad-config-toml"
        );
        assert_eq!(mode(&live_config), 0o640, "the config keeps its mode");
    }

    #[test]
    fn rollback_marker_naming_a_different_version_is_cleared() {
        let root = TempRoot::new("mismatched-version");
        let paths = root.paths();
        let live_config = paths.root.join("config.toml");
        write_file(
            &paths.marker(),
            r#"{"from":"0.1.0","to":"0.3.0","attempts":1,"written_ms":1000}"#,
        );

        // The running binary is 0.2.0, not the 0.3.0 the marker expected — as
        // if someone hand-installed a different build in between.
        let report = rollback_if_needed(&paths, "0.2.0", &live_config).expect("marker present");
        assert!(!report.restored);
        assert_eq!(report.to, "0.3.0");
        assert!(!paths.marker().exists());
    }

    #[test]
    fn rollback_missing_prev_binary_still_clears_marker() {
        let root = TempRoot::new("missing-prev-bin");
        let paths = root.paths();
        let live_config = paths.root.join("config.toml");
        write_file(
            &paths.marker(),
            r#"{"from":"0.1.0","to":"0.2.0","attempts":2,"written_ms":1000}"#,
        );
        write_file(&paths.bin(), "bad-binary-bytes");
        // No faber.prev written.

        let report = rollback_if_needed(&paths, "0.2.0", &live_config).expect("marker present");
        assert!(!report.restored);
        assert!(
            !paths.marker().exists(),
            "marker must be cleared so rollback does not loop forever"
        );
    }

    #[test]
    fn rollback_breadcrumb_is_consumed_once() {
        let root = TempRoot::new("breadcrumb");
        let paths = root.paths();
        let live_config = paths.root.join("config.toml");
        write_file(
            &paths.marker(),
            r#"{"from":"0.1.0","to":"0.2.0","attempts":2,"written_ms":1000}"#,
        );
        write_file(&paths.bin(), "bad-binary-bytes");
        write_file(&paths.prev_bin(), "old-binary-bytes");

        let report = rollback_if_needed(&paths, "0.2.0", &live_config).expect("marker present");
        assert!(report.restored);

        // The restored process reboots as 0.1.0 with no marker of its own —
        // the breadcrumb is the only thing that lets it report "I just
        // rolled back", and it must say which version it rolled back *from*.
        assert_eq!(take_rollback_breadcrumb(&paths), Some("0.2.0".to_string()));
        // One-shot: a second read (a later, unrelated restart) sees nothing.
        assert_eq!(take_rollback_breadcrumb(&paths), None);
    }

    #[test]
    fn clear_marker_deletes_and_tolerates_absence() {
        let root = TempRoot::new("clear-marker");
        let paths = root.paths();
        write_file(
            &paths.marker(),
            r#"{"from":"0.1.0","to":"0.2.0","attempts":1,"written_ms":1}"#,
        );
        assert!(paths.marker().exists());

        clear_marker(&paths);
        assert!(!paths.marker().exists());

        // Calling it again with nothing there must not panic.
        clear_marker(&paths);
    }

    /// A crash between writing the new marker and renaming it into place leaves
    /// the old marker whole. Probation then counts on from it, where a torn
    /// marker would be cleared as unparseable and probation end unseen. The temp
    /// file the crash left does not stop the next write.
    #[test]
    fn a_crash_between_write_and_rename_leaves_the_old_marker() {
        let root = TempRoot::new("marker-crash");
        let paths = root.paths();
        let live_config = paths.root.join("config.toml");
        let marker = |attempts| Marker {
            from: "0.1.0".into(),
            to: "0.2.0".into(),
            attempts,
            written_ms: 1000,
        };
        write_marker(&paths, &marker(1)).unwrap();
        // The crash: the next marker is on disk beside the old one, never renamed.
        let next = serde_json::to_string(&marker(5)).unwrap();
        let left = write_beside(&paths.marker(), next.as_bytes()).unwrap();
        assert!(left.exists());
        let read = || -> Marker {
            serde_json::from_str(&std::fs::read_to_string(paths.marker()).unwrap()).unwrap()
        };
        assert_eq!(read().attempts, 1);

        let report = rollback_if_needed(&paths, "0.2.0", &live_config).expect("the old marker");
        assert!(!report.restored);
        assert_eq!(read().attempts, 2, "counted on from the old marker");
    }

    /// The marker and the quarantine list are replaced whole, never rewritten in
    /// place, and keep the permission bits they had (setup creates the
    /// quarantine list 0640).
    #[test]
    fn the_marker_and_the_quarantine_list_are_replaced_by_rename() {
        use std::os::unix::fs::MetadataExt;
        let root = TempRoot::new("replace-by-rename");
        let paths = root.paths();
        let marker = Marker {
            from: "0.1.0".into(),
            to: "0.2.0".into(),
            attempts: 0,
            written_ms: 1,
        };
        write_marker(&paths, &marker).unwrap();
        quarantine_add(&paths.quarantine(), "0.1.9");
        for path in [paths.marker(), paths.quarantine()] {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        }
        let inode = |p: &Path| std::fs::metadata(p).unwrap().ino();
        let before = (inode(&paths.marker()), inode(&paths.quarantine()));

        write_marker(
            &paths,
            &Marker {
                attempts: 1,
                ..marker
            },
        )
        .unwrap();
        quarantine_add(&paths.quarantine(), "0.2.0");

        assert_ne!(
            inode(&paths.marker()),
            before.0,
            "marker rewritten in place"
        );
        assert_ne!(
            inode(&paths.quarantine()),
            before.1,
            "quarantine rewritten in place"
        );
        assert_eq!(read_quarantine(&paths.quarantine()), ["0.1.9", "0.2.0"]);
        for path in [paths.marker(), paths.quarantine()] {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o640, "{}", path.display());
        }
        let leftovers: Vec<_> = std::fs::read_dir(paths.root.join("update"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    // -- Provenance (issue #41) ---------------------------------------------

    /// The success path: proceed exactly as if the
    /// check did not exist.
    #[test]
    fn decide_provenance_verified_proceeds() {
        assert_eq!(
            decide_provenance(&Verdict::Verified, "v0.4.0"),
            ProvenanceAction::Proceed
        );
    }

    /// A verification that ran to completion and said no: the fault names
    /// the tag and points the operator at the release, not at this host.
    #[test]
    fn decide_provenance_failed_refuses_naming_the_tag() {
        let action = decide_provenance(
            &Verdict::Refused("no matching attestations found".to_string()),
            "v0.4.0",
        );
        let ProvenanceAction::Refuse { fault } = action else {
            panic!("a failed verification must refuse");
        };
        assert!(
            fault.contains("v0.4.0"),
            "fault should name the tag: {fault}"
        );
        assert!(
            fault.contains("no matching attestations found"),
            "fault should carry gh's own reason: {fault}"
        );
        assert!(
            fault.contains("investigate"),
            "fault should point at the release, not this host: {fault}"
        );
    }

    /// A verification that could not run at all: the fault is exactly the
    /// host-fix text the classifier produced — no tag-shaped wrapping is
    /// added, because nothing about the release is in question.
    #[test]
    fn decide_provenance_unavailable_refuses_naming_the_host_fix() {
        let reason = "the release's attestation cannot be checked on this host".to_string();
        let action = decide_provenance(&Verdict::Unavailable(reason.clone()), "v0.4.0");
        assert_eq!(action, ProvenanceAction::Refuse { fault: reason });
    }

    /// The property the two tests above establish individually, stated once
    /// as its own regression guard: *every* non-`Verified` outcome refuses.
    /// A future edit that sneaks in a third action (a warn-and-proceed,
    /// say) for either variant fails this test's `matches!`.
    #[test]
    fn decide_provenance_every_non_verified_outcome_refuses() {
        for p in [
            Verdict::Refused("x".to_string()),
            Verdict::Unavailable("y".to_string()),
        ] {
            let action = decide_provenance(&p, "v0.4.0");
            assert!(
                matches!(action, ProvenanceAction::Refuse { .. }),
                "{p:?} must refuse, got {action:?}"
            );
        }
    }

    /// Issue #41's design initially had a failed verification quarantine
    /// the release, the same as a crash-looped boot — then corrected that,
    /// because quarantine is for *proven-bad bytes*, not a policy refusal,
    /// and because it is permanent, which is the wrong failure mode for
    /// what might be a transient attestations-API lookup miss. This test
    /// pins that correction: the match below is exhaustive with no wildcard
    /// arm, so a future `ProvenanceAction::Quarantine` variant fails this
    /// file to compile rather than silently doing the wrong thing, and each
    /// arm asserts something tied to its input rather than merely existing.
    #[test]
    fn decide_provenance_never_quarantines() {
        for p in [
            Verdict::Verified,
            Verdict::Refused("x".to_string()),
            Verdict::Unavailable("y".to_string()),
        ] {
            let action = decide_provenance(&p, "v0.4.0");
            match action {
                ProvenanceAction::Proceed => assert_eq!(p, Verdict::Verified),
                ProvenanceAction::Refuse { fault } => {
                    assert_ne!(p, Verdict::Verified);
                    assert!(
                        !fault.to_ascii_lowercase().contains("quarantine"),
                        "a refusal fault must never mention quarantine: {fault}"
                    );
                }
            }
        }
    }

    /// A GitHub API stand-in that refuses every connection, so a test can prove
    /// a refusal happened before any request, or that an unreachable API pauses
    /// rather than refuses.
    fn unreachable_github() -> Github {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        Github::at(format!("http://{addr}"))
    }

    /// A release with no bundle is refused, by name, before any request is made
    /// (the API here is unreachable, so a request would have been `Unavailable`).
    #[tokio::test]
    async fn a_release_without_a_bundle_is_refused_before_any_request() {
        let gh = unreachable_github();
        let tmp = TempRoot::new("provenance-no-bundle");
        let v = verify_provenance(&gh, "owner/demo", &tmp.path, b"sums", None, "v9.9.9").await;
        assert_eq!(
            v,
            Verdict::Refused("release has no SHA256SUMS.sigstore.json".to_string())
        );
    }

    /// A stand-in GitHub API that answers each connection with the next
    /// scripted `(status, body)`.
    async fn scripted_github(script: Vec<(u16, &'static str)>) -> Github {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let mut script = script.into_iter();
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let Some((status, body)) = script.next() else {
                    continue;
                };
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });
        Github::at(base)
    }

    fn sandbox_fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/tests/fixtures/provenance/v0.0.2/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    /// Through the seam: a release whose tag does not exist is refused, naming
    /// the source commit it could not pin.
    #[tokio::test]
    async fn a_missing_tag_is_refused_through_the_seam() {
        let gh = scripted_github(vec![(404, r#"{"message":"Not Found"}"#)]).await;
        let tmp = TempRoot::new("provenance-missing-tag");
        let v =
            verify_provenance(&gh, "owner/demo", &tmp.path, b"sums", Some(b"{}"), "v9.9.9").await;
        let Verdict::Refused(reason) = v else {
            panic!("expected Refused, got {v:?}");
        };
        assert!(reason.starts_with("source commit"), "{reason}");
        assert!(reason.contains("v9.9.9"), "{reason}");
    }

    /// Through the seam: a GitHub error on the tag lookup pauses the update.
    #[tokio::test]
    async fn a_github_error_on_the_tag_lookup_pauses_through_the_seam() {
        let gh = scripted_github(vec![
            (500, r#"{"message":"Server Error"}"#),
            (500, r#"{"message":"Server Error"}"#),
        ])
        .await;
        let tmp = TempRoot::new("provenance-tag-500");
        let v =
            verify_provenance(&gh, "owner/demo", &tmp.path, b"sums", Some(b"{}"), "v9.9.9").await;
        let Verdict::Unavailable(reason) = v else {
            panic!("expected Unavailable, got {v:?}");
        };
        assert!(reason.contains("resolving tag v9.9.9"), "{reason}");
    }

    /// Through the seam: a malformed sha from the API is refused, not retried as
    /// an outage.
    #[tokio::test]
    async fn a_malformed_tag_sha_is_refused_through_the_seam() {
        let gh = scripted_github(vec![(
            200,
            r#"{"object":{"sha":"deadbeef","type":"commit"}}"#,
        )])
        .await;
        let tmp = TempRoot::new("provenance-bad-sha");
        let v =
            verify_provenance(&gh, "owner/demo", &tmp.path, b"sums", Some(b"{}"), "v9.9.9").await;
        let Verdict::Refused(reason) = v else {
            panic!("expected Refused, got {v:?}");
        };
        assert!(reason.starts_with("source commit"), "{reason}");
    }

    /// Through the seam: the tag resolves, but Sigstore's TUF is unreachable.
    /// The update pauses with the reason; no cached or embedded root is used.
    #[tokio::test]
    async fn an_unreachable_tuf_pauses_through_the_seam() {
        let gh = scripted_github(vec![(
            200,
            r#"{"object":{"sha":"f8e485b429bd328643a8d7fb1ee53910c55069a3","type":"commit"}}"#,
        )])
        .await;
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let tuf = format!("http://{}", dead.local_addr().unwrap());
        drop(dead);
        crate::provenance::TEST_TUF_URL.with(|u| *u.borrow_mut() = Some(tuf.clone()));
        let tmp = TempRoot::new("provenance-no-tuf");
        let sums = sandbox_fixture("SHA256SUMS");
        let bundle = sandbox_fixture("SHA256SUMS.sigstore.json");
        let v = verify_provenance(
            &gh,
            "owner/demo",
            &tmp.paths().tuf_cache(),
            &sums,
            Some(&bundle),
            "v0.0.2",
        )
        .await;
        crate::provenance::TEST_TUF_URL.with(|u| *u.borrow_mut() = None);
        let Verdict::Unavailable(reason) = v else {
            panic!("expected Unavailable, got {v:?}");
        };
        assert!(reason.contains("TUF"), "{reason}");
        assert!(reason.contains(&tuf), "{reason}");
        assert!(reason.contains("no cached root"), "{reason}");
    }

    /// The tag's commit is resolved from GitHub at verify time; when GitHub
    /// cannot be reached that is a pause (`Unavailable`), not a verdict on the
    /// release.
    #[tokio::test]
    async fn an_unresolvable_tag_commit_pauses_the_update() {
        let gh = unreachable_github();
        let tmp = TempRoot::new("provenance-no-api");
        let v =
            verify_provenance(&gh, "owner/demo", &tmp.path, b"sums", Some(b"{}"), "v9.9.9").await;
        let Verdict::Unavailable(reason) = v else {
            panic!("expected Unavailable, got {v:?}");
        };
        assert!(reason.contains("resolving tag v9.9.9"), "{reason}");
    }

    /// Fail closed through the function the update path calls: any verdict
    /// but `Verified` is an `Err` carrying the reason, which `fail_stage`
    /// surfaces in `/api/state`.
    #[tokio::test]
    async fn the_update_path_seam_fails_closed_with_the_reason() {
        let gh = unreachable_github();
        let tmp = TempRoot::new("provenance-seam");
        let err = verify_sums_provenance(&gh, "owner/demo", &tmp.path, "v9.9.9", b"sums", None)
            .await
            .expect_err("a release with no bundle must refuse the update");
        assert!(
            err.reason
                .contains("release has no SHA256SUMS.sigstore.json"),
            "{err}"
        );
        assert!(err.reason.contains("v9.9.9"), "{err}");
        // A definite refusal, so it is held for a full poll interval (#137).
        assert_eq!(err.class, FailClass::Refused);
    }

    /// An unreachable GitHub is an outage, not a verdict: it backs off.
    #[tokio::test]
    async fn an_unreachable_tag_lookup_is_classed_unavailable() {
        let gh = unreachable_github();
        let tmp = TempRoot::new("provenance-class");
        let err =
            verify_sums_provenance(&gh, "owner/demo", &tmp.path, "v9.9.9", b"sums", Some(b"{}"))
                .await
                .expect_err("unreachable");
        assert_eq!(err.class, FailClass::Unavailable);
    }

    // -- the commit --------------------------------------------------------

    /// An [`Updater`] over a scratch root and the drain tests' bare orchestrator,
    /// with `v1.2.3` staged long ago. Nothing about it is on disk.
    fn staged_updater(tmp: &TempRoot, gh: Github, orch: Arc<Orchestrator>) -> Arc<Updater> {
        let (updater, _handle) = Updater::new(
            Arc::new(gh),
            orch,
            policy(true, 3600),
            tmp.paths(),
            tmp.path.join("config.toml"),
            "1.0.0".to_string(),
            None,
        );
        updater.shared.state.lock().unwrap().staged = Some(Staged {
            version: "1.2.3".into(),
            tag: "v1.2.3".into(),
            commit: COMMIT.into(),
            since_ms: 1,
            ..Default::default()
        });
        updater
    }

    /// A successful commit exits the process, so an escalated commit that
    /// returns installed nothing. Its drain must then go, or the follower claims
    /// nothing until it next restarts; but only the updater's own drain, never a
    /// `faber drain` begun before the escalation or while it waited. The
    /// escalation window starts again, so a failure that recurs drains once per
    /// window, not once per tick.
    #[tokio::test]
    async fn a_failed_escalated_commit_lifts_only_the_drain_it_set() {
        // GitHub cannot be asked about the release, so nothing is installed and
        // the staged update is kept for the next try.
        let tmp = TempRoot::new("escalation-fails");
        let (orch, _orch_tmp) = crate::reconcile::tests::test_orch();
        let updater = staged_updater(&tmp, unreachable_github(), orch.clone());
        let snapshot = updater.shared.state.lock().unwrap().clone();

        updater
            .escalate_with(&snapshot, std::future::ready(()))
            .await;
        assert!(
            !orch.is_draining(),
            "the update's drain outlived the failure"
        );
        let kept = updater.shared.state.lock().unwrap().staged.clone();
        assert!(
            kept.expect("kept for the next try").since_ms > 1,
            "the escalation window starts again"
        );
        assert!(
            matches!(
                &*updater.shared.phase.lock().unwrap(),
                UpdatePhase::Failed { reason } if reason.contains("v1.2.3")
            ),
            "the dashboard says why nothing was installed"
        );

        orch.begin_drain(Drain::Stop).await;
        updater
            .escalate_with(&snapshot, std::future::ready(()))
            .await;
        assert!(orch.is_draining(), "lifted a `faber drain` begun before");

        let (orch, _orch_tmp) = crate::reconcile::tests::test_orch();
        let updater = staged_updater(&tmp, unreachable_github(), orch.clone());
        let operator = orch.clone();
        updater
            .escalate_with(&snapshot, async move {
                operator.begin_drain(Drain::Stop).await;
            })
            .await;
        assert!(orch.is_draining(), "lifted a `faber drain` begun mid-wait");
    }

    /// The owner stops a release by deleting it, or turning it back into a draft
    /// (ADR-0031). A host that staged it before then must not install it: no
    /// swap, no marker, and the staged bytes and the candidate both go. A
    /// pre-release flip refuses the same way on a host that is not pinned to it.
    #[tokio::test]
    async fn a_release_deleted_or_unpublished_after_staging_is_refused() {
        for (status, body) in [
            (404, r#"{"message":"Not Found"}"#),
            (
                200,
                r#"{"tag_name":"v1.2.3","draft":false,"prerelease":true}"#,
            ),
        ] {
            let tmp = TempRoot::new("withdrawn-at-commit");
            let paths = tmp.paths();
            write_file(&paths.bin(), "running daemon");
            write_file(&paths.staged_bin(), "staged daemon");
            let (orch, _orch_tmp) = crate::reconcile::tests::test_orch();
            let updater = staged_updater(&tmp, scripted_github(vec![(status, body)]).await, orch);
            updater.shared.state.lock().unwrap().available = Some("1.2.3".into());

            updater.do_commit().await;

            assert_eq!(
                std::fs::read_to_string(paths.bin()).unwrap(),
                "running daemon",
                "{status}"
            );
            assert!(!paths.marker().exists(), "{status}: probation armed");
            assert!(!paths.staged_bin().exists(), "{status}: still staged");
            let state = updater.shared.state.lock().unwrap().clone();
            assert!(state.staged.is_none(), "{status}");
            assert!(state.available.is_none(), "{status}: still offered");
            assert!(
                state.quarantined.is_empty(),
                "{status}: a withdrawal is no proof the binary is bad"
            );
            let phase = updater.shared.phase.lock().unwrap().clone();
            match phase {
                UpdatePhase::Deferred { version, reason } => {
                    assert_eq!(version, "1.2.3");
                    assert!(reason.contains("v1.2.3"), "{reason}");
                }
                other => panic!("{status}: {other:?}"),
            }
        }
    }

    #[test]
    fn only_a_release_still_published_as_staged_may_install() {
        let published = rel("v1.2.3");
        assert_eq!(release_withdrawn(Some(&published), "v1.2.3", None), None);
        let gone = release_withdrawn(None, "v1.2.3", None).unwrap();
        assert!(gone.contains("deleted") && gone.contains("draft"), "{gone}");
        let draft = Release {
            draft: true,
            ..rel("v1.2.3")
        };
        // A pin never brings back a draft.
        assert!(release_withdrawn(Some(&draft), "v1.2.3", Some("1.2.3")).is_some());
        let pre = Release {
            prerelease: true,
            ..rel("v1.2.3")
        };
        assert!(release_withdrawn(Some(&pre), "v1.2.3", None)
            .unwrap()
            .contains("pre-release"));
        // A pin takes its tag whatever GitHub marks it as, as `select_candidate` does.
        assert_eq!(release_withdrawn(Some(&pre), "v1.2.3", Some("1.2.3")), None);
        assert!(release_withdrawn(Some(&pre), "v1.2.3", Some("1.2.4")).is_some());
    }

    #[test]
    fn only_live_choices_that_still_allow_the_staged_version_let_it_in() {
        let pinned = |pin: &str, enabled| UpdatePolicy {
            pin: Some(pin.into()),
            ..policy(enabled, 3600)
        };
        let none: &[String] = &[];
        assert_eq!(staged_stop(&policy(true, 3600), none, "1.2.3"), None);
        assert_eq!(
            staged_stop(&policy(false, 3600), none, "1.2.3"),
            Some(StagedStop::Hold)
        );
        // A pin on the staged version lets it in, however it is written.
        assert_eq!(staged_stop(&pinned("1.2.3", true), none, "1.2.3"), None);
        assert_eq!(staged_stop(&pinned("v1.2.3", true), none, "1.2.3"), None);
        let Some(StagedStop::Discard(reason)) = staged_stop(&pinned("1.2.4", true), none, "1.2.3")
        else {
            panic!("a pin on another version let the staged one in");
        };
        assert!(
            reason.contains("1.2.4") && reason.contains("v1.2.3"),
            "{reason}"
        );
        // A pin that names no version selects nothing, so it installs nothing.
        assert!(matches!(
            staged_stop(&pinned("latest", true), none, "1.2.3"),
            Some(StagedStop::Discard(_))
        ));
        // A skip turns the version down, whatever the pin says.
        let skipped = ["1.2.3".to_string()];
        let Some(StagedStop::Discard(reason)) =
            staged_stop(&pinned("1.2.3", true), &skipped, "1.2.3")
        else {
            panic!("a skipped version was let in");
        };
        assert!(reason.contains("skipped"), "{reason}");
        assert_eq!(staged_stop(&policy(true, 3600), &skipped, "1.2.4"), None);
        // Off wins over the rest: the staged update is kept, never discarded.
        assert_eq!(
            staged_stop(&pinned("1.2.4", false), &skipped, "1.2.3"),
            Some(StagedStop::Hold)
        );
    }

    /// `v1.2.3` as GitHub serves a published release.
    const PUBLISHED: &str = r#"{"tag_name":"v1.2.3","draft":false,"prerelease":false}"#;

    /// Updates turned off while an escalated install waits out its drain, as
    /// followers are told to do on a release the owner did not make: nothing is
    /// installed and GitHub is not asked, the staged update stays for when
    /// updates are back on, and the drain goes.
    #[tokio::test]
    async fn updates_turned_off_during_the_drain_install_nothing() {
        let tmp = TempRoot::new("off-mid-drain");
        let paths = tmp.paths();
        write_file(&paths.bin(), "running daemon");
        write_file(&paths.staged_bin(), "staged daemon");
        let (orch, _orch_tmp) = crate::reconcile::tests::test_orch();
        // Anything asked of it fails, and says so in `last_error`.
        let updater = staged_updater(&tmp, unreachable_github(), orch.clone());
        let snapshot = updater.shared.state.lock().unwrap().clone();
        let operator = UpdateHandle {
            shared: updater.shared.clone(),
        };

        updater
            .escalate_with(&snapshot, async move {
                operator.set_policy(policy(false, 3600));
            })
            .await;

        assert_eq!(
            std::fs::read_to_string(paths.bin()).unwrap(),
            "running daemon"
        );
        assert!(!paths.marker().exists(), "probation armed");
        let asked = updater.shared.last_error.lock().unwrap().clone();
        assert_eq!(asked, None, "went on to install");
        assert!(paths.staged_bin().exists(), "the staged bytes went");
        let state = updater.shared.state.lock().unwrap().clone();
        let kept = state.staged.clone().expect("kept for when updates are on");
        assert_eq!(kept.version, "1.2.3");
        assert!(
            matches!(
                &*updater.shared.phase.lock().unwrap(),
                UpdatePhase::WaitingIdle { version, .. } if version == "1.2.3"
            ),
            "the dashboard says it is still installing"
        );
        assert!(!orch.is_draining(), "the update's drain outlived it");
        // Back on, it installs at the next idle moment as before.
        assert_eq!(
            decide(now_ms(), &policy(false, 3600), &state, 0, false),
            Action::Sleep
        );
        assert_eq!(
            decide(now_ms(), &policy(true, 3600), &state, 0, false),
            Action::Commit
        );
    }

    /// A pin moved to another version while an escalated install waits out its
    /// drain: the staged update is discarded the way a withdrawn one is, so the
    /// next poll finds the pinned release, and the drain goes.
    #[tokio::test]
    async fn a_pin_moved_during_the_drain_discards_the_staged_update() {
        let tmp = TempRoot::new("repinned-mid-drain");
        let paths = tmp.paths();
        write_file(&paths.bin(), "running daemon");
        write_file(&paths.staged_bin(), "staged daemon");
        let (orch, _orch_tmp) = crate::reconcile::tests::test_orch();
        let updater = staged_updater(&tmp, unreachable_github(), orch.clone());
        updater.shared.state.lock().unwrap().available = Some("1.2.3".into());
        let snapshot = updater.shared.state.lock().unwrap().clone();
        let operator = UpdateHandle {
            shared: updater.shared.clone(),
        };

        updater
            .escalate_with(&snapshot, async move {
                operator.set_policy(UpdatePolicy {
                    pin: Some("1.2.4".into()),
                    ..policy(true, 3600)
                });
            })
            .await;

        assert_eq!(
            std::fs::read_to_string(paths.bin()).unwrap(),
            "running daemon"
        );
        assert!(!paths.marker().exists(), "probation armed");
        assert!(!paths.staged_bin().exists(), "the staged bytes stayed");
        let state = updater.shared.state.lock().unwrap().clone();
        assert!(state.staged.is_none(), "still staged");
        assert!(state.available.is_none(), "still offered");
        assert!(
            state.quarantined.is_empty(),
            "a pin is no proof the binary is bad"
        );
        match updater.shared.phase.lock().unwrap().clone() {
            UpdatePhase::Deferred { version, reason } => {
                assert_eq!(version, "1.2.3");
                assert!(reason.contains("1.2.4"), "{reason}");
            }
            other => panic!("{other:?}"),
        }
        assert!(!orch.is_draining(), "the update's drain outlived it");
    }

    /// The dashboard offers Skip beside Install now on a staged update. A skip
    /// is the operator turning that version down on this host, so the next tick
    /// discards it rather than installing it at the next idle moment.
    #[tokio::test]
    async fn a_skip_on_a_staged_update_discards_it_at_the_next_tick() {
        let tmp = TempRoot::new("skipped-waiting");
        let paths = tmp.paths();
        write_file(&paths.bin(), "running daemon");
        write_file(&paths.staged_bin(), "staged daemon");
        let (orch, _orch_tmp) = crate::reconcile::tests::test_orch();
        // An idle host: the next tick would commit what is staged.
        let updater = staged_updater(&tmp, unreachable_github(), orch);
        updater.shared.state.lock().unwrap().available = Some("1.2.3".into());

        UpdateHandle {
            shared: updater.shared.clone(),
        }
        .skip_available();
        updater.tick().await;

        assert_eq!(
            std::fs::read_to_string(paths.bin()).unwrap(),
            "running daemon"
        );
        let asked = updater.shared.last_error.lock().unwrap().clone();
        assert_eq!(asked, None, "went on to install");
        assert!(!paths.staged_bin().exists(), "the staged bytes stayed");
        let state = updater.shared.state.lock().unwrap().clone();
        assert!(state.staged.is_none(), "still staged");
        assert_eq!(state.quarantined, ["1.2.3"]);
        assert!(
            matches!(
                &*updater.shared.phase.lock().unwrap(),
                UpdatePhase::Deferred { version, reason } if version == "1.2.3" && reason.contains("skipped")
            ),
            "the dashboard does not say why"
        );
    }

    /// A skip while an escalated install waits out its drain discards the staged
    /// update just the same, and the drain goes.
    #[tokio::test]
    async fn a_skip_during_the_drain_discards_the_staged_update() {
        let tmp = TempRoot::new("skipped-mid-drain");
        let paths = tmp.paths();
        write_file(&paths.bin(), "running daemon");
        write_file(&paths.staged_bin(), "staged daemon");
        let (orch, _orch_tmp) = crate::reconcile::tests::test_orch();
        let updater = staged_updater(&tmp, unreachable_github(), orch.clone());
        updater.shared.state.lock().unwrap().available = Some("1.2.3".into());
        let snapshot = updater.shared.state.lock().unwrap().clone();
        let operator = UpdateHandle {
            shared: updater.shared.clone(),
        };

        updater
            .escalate_with(&snapshot, async move { operator.skip_available() })
            .await;

        assert_eq!(
            std::fs::read_to_string(paths.bin()).unwrap(),
            "running daemon"
        );
        assert!(!paths.marker().exists(), "probation armed");
        assert!(!paths.staged_bin().exists(), "the staged bytes stayed");
        assert!(
            updater.shared.state.lock().unwrap().staged.is_none(),
            "still staged"
        );
        assert!(!orch.is_draining(), "the update's drain outlived it");
    }

    /// The check stops only what the policy changed. Unchanged, or pinned to the
    /// staged version itself, the install goes on past the release re-read to
    /// the staged bytes, which here never matched and stop it there.
    #[tokio::test]
    async fn an_unchanged_policy_lets_the_install_go_on() {
        for pin in [None, Some("1.2.3"), Some("v1.2.3")] {
            let tmp = TempRoot::new("policy-unchanged");
            write_file(&tmp.paths().staged_bin(), "staged daemon");
            let (orch, _orch_tmp) = crate::reconcile::tests::test_orch();
            let gh = scripted_github(vec![(200, PUBLISHED)]).await;
            let updater = staged_updater(&tmp, gh, orch);
            updater.shared.policy.lock().unwrap().pin = pin.map(String::from);

            updater.do_commit().await;

            let phase = updater.shared.phase.lock().unwrap().clone();
            match phase {
                UpdatePhase::Failed { reason } => assert!(
                    reason.contains("staged binary changed before install"),
                    "{pin:?}: {reason}"
                ),
                other => panic!("{pin:?}: {other:?}"),
            }
        }
    }

    const COMMIT: &str = "1a2b3c4d5e6f708192a3b4c5d6e7f8091a2b3c4d";

    #[cfg(unix)]
    fn fake_binary(tmp: &TempRoot, script: &str) -> PathBuf {
        let path = tmp.path.join("faber");
        write_file(&path, script);
        chmod_executable(&path).unwrap();
        path
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_staged_binary_is_asked_and_judged() {
        let tmp = TempRoot::new("version-binding");
        let ok = fake_binary(&tmp, "#!/bin/sh\necho 'faber 1.2.3+g1a2b3c4'\n");
        assert_eq!(verify_binary_version(&ok, "v1.2.3", COMMIT).await, Ok(()));
        let old = fake_binary(&tmp, "#!/bin/sh\necho 'faber 1.0.0+g1a2b3c4'\n");
        assert!(verify_binary_version(&old, "v1.2.3", COMMIT)
            .await
            .unwrap_err()
            .contains("1.0.0"));
        let bare = fake_binary(&tmp, "#!/bin/sh\necho 'faber 1.2.3'\n");
        assert!(verify_binary_version(&bare, "v1.2.3", COMMIT)
            .await
            .is_err());
        let fails = fake_binary(&tmp, "#!/bin/sh\nexit 3\n");
        assert!(verify_binary_version(&fails, "v1.2.3", COMMIT)
            .await
            .unwrap_err()
            .contains("exited"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_staged_binary_does_not_see_fabers_environment() {
        let tmp = TempRoot::new("version-binding-env");
        std::env::set_var("FABER_TEST_SECRET_91", "x");
        let bin = fake_binary(
            &tmp,
            "#!/bin/sh\n[ -z \"$FABER_TEST_SECRET_91\" ] || echo leaked\necho 'faber 1.2.3+g1a2b3c4'\n",
        );
        // A leak would print an extra line and fail the parse.
        assert_eq!(verify_binary_version(&bin, "v1.2.3", COMMIT).await, Ok(()));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_staged_file_replaced_after_staging_is_refused_at_commit() {
        let tmp = TempRoot::new("version-binding-gap");
        let good = "#!/bin/sh\necho 'faber 1.2.3+g1a2b3c4'\n";
        let bin = fake_binary(&tmp, good);
        let staged = Staged {
            version: "1.2.3".into(),
            binary_sha256: sha256_hex(good.as_bytes()),
            launcher_sha256: None,
            agent_sha256: None,
            tag: "v1.2.3".into(),
            commit: COMMIT.into(),
            since_ms: 0,
        };
        assert_eq!(recheck_staged_binary(&bin, &staged).await, Ok(()));
        // Swapped for an older genuine build: bytes differ.
        write_file(&bin, "#!/bin/sh\necho 'faber 1.0.0+g1a2b3c4'\n");
        let err = recheck_staged_binary(&bin, &staged).await.unwrap_err();
        assert!(err.contains("sha256"), "{err}");
        // Same digest recorded but the probe disagrees (e.g. wrong tag).
        let other = Staged {
            tag: "v9.9.9".into(),
            ..staged
        };
        write_file(&bin, good);
        assert!(recheck_staged_binary(&bin, &other).await.is_err());
    }

    // -- ADR-0025: the updater on a SYSTEM host --------------------------

    /// SYSTEM has a floor and LEGACY has none. Build metadata and a `v`
    /// prefix don't change the answer, and what doesn't parse is refused.
    #[test]
    fn a_system_host_refuses_releases_below_the_layout_floor() {
        assert_eq!(parse_version(MIN_SYSTEM_LAYOUT_VERSION), Some((0, 3, 13)));
        for below in ["0.3.12", "v0.3.12", "0.3.12+g1a2b3c4", "0.2.0", "0.0.1"] {
            let why = layout_refusal(Layout::System, below)
                .unwrap_or_else(|| panic!("{below} must be refused under SYSTEM"));
            assert!(
                why.starts_with(&format!("{below} predates the SYSTEM layout")),
                "{why}"
            );
            assert!(why.contains(MIN_SYSTEM_LAYOUT_VERSION), "{why}");
        }
        for at_or_above in [
            "0.3.13",
            "v0.3.13",
            "0.3.13+gabc",
            "0.3.14",
            "0.4.0",
            "1.0.0",
        ] {
            assert_eq!(
                layout_refusal(Layout::System, at_or_above),
                None,
                "{at_or_above}"
            );
        }
        for junk in ["", "latest", "0.3", "0.3.13.1"] {
            assert!(layout_refusal(Layout::System, junk).is_some(), "{junk:?}");
        }
        // LEGACY is unchanged: no floor at all, not even for junk (the
        // updater's own `is_newer` already refuses what it cannot compare).
        for any in ["0.0.1", "0.3.12", "0.4.0", "junk"] {
            assert_eq!(layout_refusal(Layout::Legacy, any), None, "{any}");
        }
    }

    /// The swap target under SYSTEM is `D/bin/faber`, the daemon copy the
    /// service runs, never root's CLI copy, and its backup sits in the same
    /// directory, so the install stays a same-directory rename (ADR-0006).
    #[test]
    fn the_system_swap_target_is_the_daemon_copy() {
        use crate::platform::{Os, SystemLayout};
        for os in [Os::MacOs, Os::Linux] {
            let system = SystemLayout::for_os(os);
            let paths = UpdatePaths::with_root(system.paths().state_dir.expect("SYSTEM has one"));
            assert_eq!(paths.bin(), system.daemon_bin, "{os:?}");
            assert_eq!(Some(paths.bin()), system.paths().daemon_bin_path, "{os:?}");
            assert_ne!(paths.bin(), system.cli_bin, "{os:?}");
            assert_eq!(
                paths.prev_bin().parent(),
                Some(system.bin_dir.as_path()),
                "{os:?}"
            );
        }
    }

    /// The gate a staged binary runs is structural only, and the updater asks
    /// for it by name.
    #[test]
    fn check_config_for_update_is_structural_only() {
        // An unstated dind grant: fine structurally, and exactly what the
        // Linux dind wall's policy rejects (ADR-0025).
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
        assert_eq!(config_errors(&cfg, true), cfg.validate_structural());
        assert!(
            config_errors(&cfg, true).is_empty(),
            "{:?}",
            config_errors(&cfg, true)
        );
        assert_eq!(config_errors(&cfg, false), cfg.validate());
        #[cfg(target_os = "linux")]
        assert!(
            !config_errors(&cfg, false).is_empty(),
            "the check a human runs still holds the Linux dind wall"
        );

        // A structural fault fails both.
        let broken = FileConfig {
            topic: "  ".to_string(),
            ..cfg.clone()
        };
        assert!(!config_errors(&broken, true).is_empty());
        assert!(!config_errors(&broken, false).is_empty());

        let args = check_config_args(Path::new("/srv/faber/config.toml"));
        assert_eq!(
            args,
            [
                "check-config",
                "--for-update",
                "--config",
                "/srv/faber/config.toml"
            ]
            .map(std::ffi::OsString::from)
            .to_vec()
        );
    }

    /// Where standby (ADR-0025) meets this module's rollback. Probation counts
    /// every boot that ends without the clean-exit mark, and restores the
    /// previous release at the third. A boot whose standby gives up on
    /// probation ends that way (`main` exits without `note_clean_exit`), so a
    /// release that broke its GitHub client is rolled back, not left waiting.
    #[tokio::test]
    async fn a_standby_that_gives_up_on_probation_is_rolled_back_at_the_third_boot() {
        use crate::standby::{self, Blocker, Evidence, StandbyOptions};

        let root = TempRoot::new("standby-probation");
        let paths = root.paths();
        let live_config = paths.root.join("config.toml");
        write_file(&paths.bin(), "new");
        write_file(&paths.prev_bin(), "old");
        write_file(&live_config, "new config");
        write_file(&paths.prev_config(), "old config");
        // What `do_commit` leaves behind it: a fresh marker.
        let marker = Marker {
            from: "0.1.0".into(),
            to: "0.2.0".into(),
            attempts: 0,
            written_ms: 1000,
        };
        write_marker(&paths, &marker).unwrap();

        for boot in 1..=2 {
            let report = rollback_if_needed(&paths, "0.2.0", &live_config).expect("a marker");
            assert!(!report.restored, "boot {boot}");
            assert_eq!(report.note, None, "boot {boot} is on probation");
            // This boot's standby, failing the way a broken client does: no
            // answer, while the probe (injected: no network in tests) reaches
            // GitHub without faber's HTTP stack.
            let opts = StandbyOptions {
                door: crate::control::FrontDoor::new(
                    0,
                    None,
                    0o600,
                    None,
                    standby::euid(),
                    std::sync::Arc::new(crate::control::Sessions::system()),
                ),
                retry: Duration::from_millis(1),
                topic: "faber".into(),
                audit_path: String::new(),
                notices: Vec::new(),
                probation: true,
                reachable: || true,
            };
            let gave_up = standby::until_ready(opts, || async {
                Err::<(), _>(Blocker {
                    what: "the latest actions/runner version",
                    reason: "tls handshake failed".into(),
                    next: "-".into(),
                    evidence: Evidence::Unanswered,
                })
            })
            .await
            .expect_err("on probation, standby gives up");
            assert_eq!(gave_up.attempts, standby::PROBATION_ATTEMPTS);
            // `main` exits here, and nothing on this path writes the mark.
            assert!(!paths.clean_exit().exists(), "boot {boot}");
        }

        let report = rollback_if_needed(&paths, "0.2.0", &live_config).expect("a marker");
        assert!(
            report.restored,
            "the third boot restores the previous release"
        );
        assert_eq!(std::fs::read_to_string(paths.bin()).unwrap(), "old");
        assert_eq!(std::fs::read_to_string(&live_config).unwrap(), "old config");
        assert!(read_quarantine(&paths.quarantine()).contains(&"0.2.0".to_string()));
    }

    // -- the VM launcher asset (ADR-0026) ---------------------------------

    const LAUNCHER_OK: &str = "#!/bin/sh\necho 'faber-vm 1.2.3+g1a2b3c4'\n";

    #[test]
    fn launcher_paths_sit_beside_the_daemons() {
        let paths = UpdatePaths::with_root(PathBuf::from("/r"));
        assert_eq!(paths.launcher(), PathBuf::from("/r/bin/faber-vm"));
        assert_eq!(paths.prev_launcher(), PathBuf::from("/r/bin/faber-vm.prev"));
        assert_eq!(
            paths.staged_launcher(),
            PathBuf::from("/r/bin/faber-vm.staged")
        );
    }

    /// Under SYSTEM the launcher's directory is the layout's, outside `D`, and
    /// the staged copy, the `.prev` and the live launcher share it, so every
    /// rename of a swap or a rollback stays in one directory.
    #[test]
    fn system_launcher_paths_share_the_layouts_launcher_dir() {
        use crate::platform::layout::{Os, SystemLayout};
        for os in [Os::MacOs, Os::Linux] {
            let layout = SystemLayout::for_os(os);
            let mut paths = UpdatePaths::with_root(layout.state_dir.clone());
            paths.launcher_dir = UpdatePaths::launcher_dir_of(&layout);
            assert_eq!(paths.launcher(), layout.launcher_bin, "{os:?}");
            let dir = layout.launcher_bin.parent().unwrap();
            for p in [
                paths.launcher(),
                paths.prev_launcher(),
                paths.staged_launcher(),
            ] {
                assert_eq!(p.parent(), Some(dir), "{os:?}: {}", p.display());
                assert!(!p.starts_with(&layout.state_dir), "{os:?}: {}", p.display());
            }
            // The daemon's own swap stays in `D/bin`.
            assert_eq!(paths.bin(), layout.daemon_bin, "{os:?}");
        }
    }

    const AGENT_OK: &str = "#!/bin/sh\necho 'faber-agent 1.2.3+g1a2b3c4 protocol=2'\n";

    #[test]
    fn agent_paths_sit_beside_the_launcher_and_carry_the_name_the_tools_disk_uses() {
        let paths = UpdatePaths::with_root(PathBuf::from("/r"));
        assert_eq!(paths.agent(), PathBuf::from("/r/bin/faber-agent"));
        assert_eq!(paths.prev_agent(), PathBuf::from("/r/bin/faber-agent.prev"));
        assert_eq!(
            paths.staged_agent(),
            PathBuf::from("/r/update/staged/faber-agent")
        );
        // The installed file's name is the one the tools disk puts the bytes under.
        assert_eq!(
            paths.agent().file_name().unwrap(),
            crate::vm::tools_disk::AGENT_FILE
        );
    }

    /// The asset name the updater asks for is the one release.yml stages, and
    /// SHA256SUMS (`faber-*`) therefore covers it.
    #[test]
    fn the_agent_asset_name_is_the_one_release_yml_ships() {
        if let Some(name) = AGENT_ASSET_NAME {
            assert_eq!(name, format!("faber-agent-{}", env!("FABER_TARGET")));
            assert!(name.starts_with("faber-"), "SHA256SUMS globs faber-*");
        }
        let wf = include_str!("../.github/workflows/release.yml");
        assert!(
            wf.contains(r#"dist/faber-agent-${{ matrix.target }}"#),
            "release.yml no longer stages faber-agent-<target>"
        );
        // Only the macOS leg has a guest agent, as the updater expects.
        assert!(cfg!(target_os = "macos") == AGENT_ASSET_NAME.is_some());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn the_agent_version_is_bound_and_cannot_pass_for_another_program() {
        let tmp = TempRoot::new("agent-version");
        let ok = fake_binary(&tmp, AGENT_OK);
        assert_eq!(verify_agent_version(&ok, "v1.2.3", COMMIT).await, Ok(()));
        assert!(verify_agent_version(&ok, "v1.2.4", COMMIT).await.is_err());
        assert!(verify_agent_version(&ok, "v1.2.3", "ffffffffffffffffffff")
            .await
            .is_err());
        // Neither the launcher nor the daemon passes as the agent, nor the reverse.
        assert!(verify_launcher_version(&ok, "v1.2.3", COMMIT)
            .await
            .is_err());
        assert!(verify_binary_version(&ok, "v1.2.3", COMMIT).await.is_err());
        let tmp = TempRoot::new("agent-version-vm");
        let vm = fake_binary(&tmp, LAUNCHER_OK);
        assert!(verify_agent_version(&vm, "v1.2.3", COMMIT).await.is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_staged_agent_replaced_after_staging_is_refused_at_commit() {
        let tmp = TempRoot::new("agent-gap");
        let bin = fake_binary(&tmp, AGENT_OK);
        let staged = Staged {
            agent_sha256: Some(sha256_hex(AGENT_OK.as_bytes())),
            tag: "v1.2.3".into(),
            commit: COMMIT.into(),
            ..Default::default()
        };
        assert_eq!(recheck_staged_agent(&bin, &staged).await, Ok(()));
        write_file(
            &bin,
            "#!/bin/sh\necho 'faber-agent 1.0.0+g1a2b3c4 protocol=2'\n",
        );
        assert!(recheck_staged_agent(&bin, &staged)
            .await
            .unwrap_err()
            .contains("sha256"));
        let none = Staged {
            agent_sha256: None,
            ..staged
        };
        assert!(recheck_staged_agent(&bin, &none).await.is_err());
    }

    #[tokio::test]
    async fn the_agent_is_snapshotted_and_restored_and_never_deleted_without_a_snapshot() {
        let tmp = TempRoot::new("agent-snapshot");
        let paths = tmp.paths();
        std::fs::create_dir_all(paths.agent().parent().unwrap()).unwrap();
        // First release that ships one: nothing to restore, nothing deleted.
        std::fs::write(paths.agent(), b"new").unwrap();
        assert!(!restore_agent(&paths).unwrap());
        assert_eq!(std::fs::read(paths.agent()).unwrap(), b"new");
        // A snapshot comes back over a replaced agent.
        std::fs::write(paths.agent(), b"old").unwrap();
        snapshot_agent(&paths).await.unwrap();
        std::fs::write(paths.agent(), b"new").unwrap();
        assert!(restore_agent(&paths).unwrap());
        assert_eq!(std::fs::read(paths.agent()).unwrap(), b"old");
        // No running agent: a stale snapshot is dropped, not kept for later.
        std::fs::remove_file(paths.agent()).unwrap();
        snapshot_agent(&paths).await.unwrap();
        assert!(!paths.prev_agent().exists());
    }

    #[test]
    fn a_release_without_the_launcher_asset_is_unavailable_not_refused() {
        let mut release = rel("v1.2.3");
        let err = required_asset(&release, "faber-launcher-x").unwrap_err();
        assert_eq!(err.class, FailClass::Unavailable);
        assert!(err.reason.contains("faber-launcher-x"), "{}", err.reason);
        release.assets.push(ReleaseAsset {
            id: 7,
            name: "faber-launcher-x".into(),
            size: 0,
        });
        assert_eq!(required_asset(&release, "faber-launcher-x").unwrap().id, 7);
    }

    #[test]
    fn the_launcher_digest_is_checked_against_the_attested_sums() {
        let name = "faber-launcher-x";
        let sums = parse_sha256sums(&format!("{}  {name}\n", sha256_hex(b"good")));
        assert_eq!(
            verify_launcher_digest(&sums, name, b"good").unwrap(),
            sha256_hex(b"good")
        );
        let err = verify_launcher_digest(&sums, name, b"evil").unwrap_err();
        assert_eq!(err.class, FailClass::Refused);
        // Not listed at all is a refusal too.
        let err = verify_launcher_digest(&parse_sha256sums(""), name, b"good").unwrap_err();
        assert_eq!(err.class, FailClass::Refused);
    }

    #[tokio::test]
    async fn staging_writes_the_launcher_and_a_failed_stage_removes_it() {
        let tmp = TempRoot::new("stage-launcher");
        let paths = tmp.paths();
        write_staged(
            &paths,
            b"daemon",
            Some(b"app"),
            Some(b"launcher"),
            Some(b"agent"),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read(paths.staged_bin()).unwrap(), b"daemon");
        assert_eq!(std::fs::read(paths.staged_app()).unwrap(), b"app");
        assert_eq!(std::fs::read(paths.staged_launcher()).unwrap(), b"launcher");
        assert_eq!(std::fs::read(paths.staged_agent()).unwrap(), b"agent");
        remove_staged(&paths).await;
        assert!(!paths.staged_agent().exists());
        assert!(!paths.staged_bin().exists());
        assert!(!paths.staged_app().exists());
        assert!(!paths.staged_launcher().exists());
    }

    #[cfg(unix)]
    #[tokio::test(start_paused = true)]
    async fn a_spawn_that_hits_etxtbsy_is_retried_and_other_errors_are_not() {
        use std::io::{Error, ErrorKind};
        let busy = || Error::from(ErrorKind::ExecutableFileBusy);

        let mut calls = 0;
        let got = retry_text_file_busy(|| {
            calls += 1;
            let n = calls;
            async move {
                if n <= 2 {
                    Err(busy())
                } else {
                    Ok(n)
                }
            }
        })
        .await
        .unwrap();
        assert_eq!((got, calls), (3, 3));

        let mut calls = 0;
        let err = retry_text_file_busy(|| {
            calls += 1;
            async { Err::<(), _>(Error::from(ErrorKind::NotFound)) }
        })
        .await
        .unwrap_err();
        assert_eq!((err.kind(), calls), (ErrorKind::NotFound, 1));

        let mut calls = 0;
        let err = retry_text_file_busy(|| {
            calls += 1;
            async { Err::<(), _>(busy()) }
        })
        .await
        .unwrap_err();
        assert!(is_text_file_busy(&err));
        assert_eq!(calls, 5);

        let mut calls = 0;
        let got = retry_text_file_busy_blocking(|| {
            calls += 1;
            if calls < 2 {
                Err(busy())
            } else {
                Ok(calls)
            }
        })
        .unwrap();
        assert_eq!(got, 2);
    }
    #[tokio::test]
    async fn the_launcher_version_is_bound_and_the_program_names_cannot_be_confused() {
        let tmp = TempRoot::new("launcher-version");
        let ok = fake_binary(&tmp, LAUNCHER_OK);
        assert_eq!(verify_launcher_version(&ok, "v1.2.3", COMMIT).await, Ok(()));
        // Wrong version and wrong commit.
        assert!(verify_launcher_version(&ok, "v1.2.4", COMMIT)
            .await
            .unwrap_err()
            .contains("1.2.3"));
        assert!(
            verify_launcher_version(&ok, "v1.2.3", "ffffffffffffffffffff")
                .await
                .is_err()
        );
        // The launcher's output is not a daemon's, and the reverse.
        assert!(verify_binary_version(&ok, "v1.2.3", COMMIT).await.is_err());
        let daemon = fake_binary(&tmp, "#!/bin/sh\necho 'faber 1.2.3+g1a2b3c4'\n");
        assert!(verify_launcher_version(&daemon, "v1.2.3", COMMIT)
            .await
            .is_err());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_staged_launcher_replaced_after_staging_is_refused_at_commit() {
        let tmp = TempRoot::new("launcher-gap");
        let bin = fake_binary(&tmp, LAUNCHER_OK);
        let staged = Staged {
            launcher_sha256: Some(sha256_hex(LAUNCHER_OK.as_bytes())),
            tag: "v1.2.3".into(),
            commit: COMMIT.into(),
            ..Default::default()
        };
        assert_eq!(recheck_staged_launcher(&bin, &staged).await, Ok(()));
        write_file(&bin, "#!/bin/sh\necho 'faber-vm 1.0.0+g1a2b3c4'\n");
        let err = recheck_staged_launcher(&bin, &staged).await.unwrap_err();
        assert!(err.contains("sha256"), "{err}");
        // A staged update that recorded no launcher digest cannot install one.
        let none = Staged {
            launcher_sha256: None,
            ..staged
        };
        assert!(recheck_staged_launcher(&bin, &none).await.is_err());
    }

    /// The commit order: snapshot, launcher, then daemon; each temp name is its
    /// own, so neither install can trip over the other.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_swap_installs_both_and_keeps_both_snapshots() {
        let tmp = TempRoot::new("swap-both");
        let paths = tmp.paths();
        write_file(&paths.bin(), "old daemon");
        write_file(&paths.launcher(), "old launcher");
        write_file(&paths.staged_bin(), "new daemon");
        write_file(&paths.staged_launcher(), "new launcher");
        atomic_install(&paths.bin(), &paths.prev_bin())
            .await
            .unwrap();
        snapshot_launcher(&paths).await.unwrap();
        atomic_install(&paths.staged_launcher(), &paths.launcher())
            .await
            .unwrap();
        atomic_install(&paths.staged_bin(), &paths.bin())
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(paths.bin()).unwrap(), "new daemon");
        assert_eq!(
            std::fs::read_to_string(paths.launcher()).unwrap(),
            "new launcher"
        );
        assert_eq!(
            std::fs::read_to_string(paths.prev_launcher()).unwrap(),
            "old launcher"
        );
        let mode = std::fs::metadata(paths.launcher())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o755);
        // No temp file left behind.
        let leftovers: Vec<_> = std::fs::read_dir(paths.root.join("bin"))
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains(".tmp."))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[tokio::test]
    async fn the_first_launcher_has_no_snapshot_and_a_stale_one_is_dropped() {
        let tmp = TempRoot::new("first-launcher");
        let paths = tmp.paths();
        snapshot_launcher(&paths).await.unwrap();
        assert!(!paths.prev_launcher().exists());
        // A snapshot left by an older release must not outlive a launcher-less one.
        write_file(&paths.prev_launcher(), "stale");
        snapshot_launcher(&paths).await.unwrap();
        assert!(!paths.prev_launcher().exists());
    }

    #[test]
    fn a_crash_loop_restores_the_launcher_with_the_daemon_and_never_deletes_one() {
        let root = TempRoot::new("rollback-launcher");
        let paths = root.paths();
        let live_config = paths.root.join("config.toml");
        write_file(&paths.bin(), "new");
        write_file(&paths.prev_bin(), "old");
        write_file(&paths.launcher(), "new launcher");
        write_file(&paths.prev_launcher(), "old launcher");
        let arm = |paths: &UpdatePaths| {
            let marker = Marker {
                from: "0.1.0".into(),
                to: "0.2.0".into(),
                attempts: 2,
                written_ms: 1000,
            };
            write_marker(paths, &marker).unwrap();
        };
        arm(&paths);
        let report = rollback_if_needed(&paths, "0.2.0", &live_config).expect("a marker");
        assert!(report.restored);
        assert_eq!(std::fs::read_to_string(paths.bin()).unwrap(), "old");
        assert_eq!(
            std::fs::read_to_string(paths.launcher()).unwrap(),
            "old launcher"
        );

        // No snapshot (the first release that shipped a launcher): left alone.
        std::fs::remove_file(paths.prev_launcher()).unwrap();
        write_file(&paths.launcher(), "new launcher");
        arm(&paths);
        let report = rollback_if_needed(&paths, "0.2.0", &live_config).expect("a marker");
        assert!(report.restored);
        assert_eq!(
            std::fs::read_to_string(paths.launcher()).unwrap(),
            "new launcher"
        );
    }

    // -- launcher probation ------------------------------------------------

    const DAEMON_VERSION: &str = "1.2.3+g1a2b3c4";

    /// A fake launcher: reports `version_line` and answers `validate` with
    /// `validate_body`. It also insists on the fixture the real one demands.
    #[cfg(unix)]
    fn fake_launcher(paths: &UpdatePaths, version_line: &str, validate_body: &str) -> PathBuf {
        let script = format!(
            "#!/bin/sh\n\
             case \"$1\" in\n\
             --version) echo '{version_line}' ;;\n\
             validate)\n\
               case \"$(ls -ld \"$2\")\" in drwx------*) ;; *) echo bad bundle mode >&2; exit 9 ;; esac\n\
               [ -f \"$2/disk.raw\" ] || {{ echo no disk >&2; exit 9; }}\n\
               [ \"$3 $4 $5 $6 $7\" = '--slot 0 --network shared-nat --wall-only' ] || exit 8\n\
               {validate_body} ;;\n\
             esac\n"
        );
        let path = paths.launcher();
        write_file(&path, &script);
        chmod_executable(&path).unwrap();
        path
    }

    #[cfg(unix)]
    async fn probe(paths: &UpdatePaths) -> LauncherProbation {
        launcher_probation(
            &paths.launcher(),
            DAEMON_VERSION,
            &paths.root.join("update"),
        )
        .await
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_launcher_of_the_daemons_build_that_holds_the_wall_passes() {
        let tmp = TempRoot::new("probation-ok");
        let paths = tmp.paths();
        fake_launcher(
            &paths,
            "faber-vm 1.2.3+g1a2b3c4",
            "echo wall=ok mac=02:fa:be:00:00:00",
        );
        assert_eq!(probe(&paths).await, LauncherProbation::Passed);
        // The throwaway fixture is gone.
        let left: Vec<_> = std::fs::read_dir(paths.root.join("update"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert!(left.is_empty(), "{left:?}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_launcher_that_is_not_the_daemons_build_fails_probation() {
        let tmp = TempRoot::new("probation-version");
        let paths = tmp.paths();
        for line in [
            "faber-vm 1.2.3+gdeadbee",  // another commit
            "faber-vm 1.2.2+g1a2b3c4",  // another version
            "faber-vm 1.2.3",           // no stamp
            "faber 1.2.3+g1a2b3c4",     // the daemon's own program name
            "faber-vm 1.2.3+g1a2b3c4x", // a longer stamp is not the same stamp
        ] {
            fake_launcher(&paths, line, "echo wall=ok mac=02:fa:be:00:00:00");
            assert!(
                matches!(probe(&paths).await, LauncherProbation::Failed(_)),
                "{line}"
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_launcher_whose_validate_fails_or_proves_nothing_fails_probation() {
        let tmp = TempRoot::new("probation-validate");
        let paths = tmp.paths();
        fake_launcher(
            &paths,
            "faber-vm 1.2.3+g1a2b3c4",
            "echo refused >&2; exit 1",
        );
        match probe(&paths).await {
            LauncherProbation::Failed(why) => assert!(why.contains("refused"), "{why}"),
            other => panic!("{other:?}"),
        }
        // Exit 0 with no `wall=ok` line is no proof.
        fake_launcher(&paths, "faber-vm 1.2.3+g1a2b3c4", "echo wall=okay mac=x");
        assert!(matches!(probe(&paths).await, LauncherProbation::Failed(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_missing_launcher_fails_probation() {
        let tmp = TempRoot::new("probation-missing");
        let paths = tmp.paths();
        assert!(matches!(probe(&paths).await, LauncherProbation::Failed(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_hung_launcher_is_bounded() {
        let tmp = TempRoot::new("probation-hang");
        let paths = tmp.paths();
        write_file(&paths.launcher(), "#!/bin/sh\nexec sleep 30\n");
        chmod_executable(&paths.launcher()).unwrap();
        let started = std::time::Instant::now();
        let got = launcher_probation_with(
            &paths.launcher(),
            DAEMON_VERSION,
            &paths.root.join("update"),
            Duration::from_millis(200),
            Duration::from_millis(200),
        )
        .await;
        assert!(matches!(got, LauncherProbation::Failed(_)), "{got:?}");
        assert!(started.elapsed() < Duration::from_secs(10));
        // A launcher that answers `--version` but hangs in validate is bounded too.
        write_file(
            &paths.launcher(),
            "#!/bin/sh\n[ \"$1\" = --version ] && { echo 'faber-vm 1.2.3+g1a2b3c4'; exit 0; }\nexec sleep 30\n",
        );
        let got = launcher_probation_with(
            &paths.launcher(),
            DAEMON_VERSION,
            &paths.root.join("update"),
            Duration::from_secs(5),
            Duration::from_millis(200),
        )
        .await;
        match got {
            LauncherProbation::Failed(why) => assert!(why.contains("did not finish"), "{why}"),
            other => panic!("{other:?}"),
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn probation_failure_arms_the_rollback_only_while_a_marker_names_this_version() {
        let tmp = TempRoot::new("probation-arm");
        let paths = tmp.paths();
        fake_launcher(
            &paths,
            "faber-vm 9.9.9+g1a2b3c4",
            "echo wall=ok mac=02:fa:be:00:00:00",
        );
        // No marker: not on probation, nothing is checked or armed.
        use LauncherProof::*;
        assert_eq!(prove_launcher_with(&paths, "1.2.3", true).await, NotChecked);
        let marker = Marker {
            from: "1.2.2".into(),
            to: "1.2.3".into(),
            attempts: 0,
            written_ms: 1,
        };
        write_marker(&paths, &marker).unwrap();
        // A platform without a launcher never checks one.
        assert_eq!(
            prove_launcher_with(&paths, "1.2.3", false).await,
            NotChecked
        );
        // The failing launcher arms the same rollback a refused self-check does.
        assert_eq!(
            prove_launcher_with(&paths, "1.2.3+g1a2b3c4", true).await,
            Armed
        );
        let armed: Marker =
            serde_json::from_str(&std::fs::read_to_string(paths.marker()).unwrap()).unwrap();
        assert_eq!(armed.attempts, 2);
        // A good launcher arms nothing.
        fake_launcher(
            &paths,
            "faber-vm 1.2.3+g1a2b3c4",
            "echo wall=ok mac=02:fa:be:00:00:00",
        );
        write_marker(&paths, &marker).unwrap();
        assert_eq!(
            prove_launcher_with(&paths, DAEMON_VERSION, true).await,
            Passed
        );
        let kept: Marker =
            serde_json::from_str(&std::fs::read_to_string(paths.marker()).unwrap()).unwrap();
        assert_eq!(kept.attempts, 0);
    }

    #[tokio::test]
    async fn the_restart_hook_fires_only_for_a_passed_launcher() {
        let tmp = TempRoot::new("restart-gate");
        let h = test_handle(tmp.paths(), "1.2.3");
        let fake = FakeRestart::new(Ok(RestartOutcome::TimedOut));
        for proof in [LauncherProof::Armed, LauncherProof::NotChecked] {
            restart_after_proof_with(proof, request_supervisor_restart(&fake, "1.2.3", &h)).await;
            assert_eq!(fake.calls(), 0, "{proof:?} must not restart the supervisor");
        }
        restart_after_proof_with(
            LauncherProof::Passed,
            request_supervisor_restart(&fake, "1.2.3", &h),
        )
        .await;
        assert_eq!(fake.calls(), 1);
    }

    use crate::vm::proto::SupervisorVersion;
    use crate::vm::restart::{RestartError, RestartOutcome, SupervisorRestart};

    /// A [`SupervisorRestart`] that answers a canned result and counts calls.
    struct FakeRestart {
        result: Result<RestartOutcome, RestartError>,
        calls: AtomicU64,
    }

    impl FakeRestart {
        fn new(result: Result<RestartOutcome, RestartError>) -> Self {
            Self {
                result,
                calls: AtomicU64::new(0),
            }
        }
        fn calls(&self) -> u64 {
            self.calls.load(Ordering::SeqCst)
        }
    }

    impl SupervisorRestart for FakeRestart {
        async fn restart_supervisor(
            &self,
            _expected_version: &str,
        ) -> Result<RestartOutcome, RestartError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.result.clone()
        }
    }

    struct Session(Result<(), String>);

    impl crate::vm::session::Handshake for Session {
        async fn reach(&self) -> Result<(), String> {
            self.0.clone()
        }
    }

    /// ADR-0051: the session supervisor is asked to exit onto the new launcher
    /// only while its session answers; a session that is down starts on it at
    /// the next login, and is no failure. Its success never hides one of
    /// `_faber_vm`'s.
    #[tokio::test]
    async fn the_session_supervisor_restarts_only_while_its_session_is_up() {
        let tmp = TempRoot::new("restart-session");
        let h = test_handle(tmp.paths(), "1.2.3");
        let ok = SupervisorVersion::try_from("1.2.3").unwrap();
        let fake = FakeRestart::new(Ok(RestartOutcome::Restarted { version: ok }));
        let down = Session(Err("no socket".into()));
        request_session_supervisor_restart(&down, &fake, "1.2.3", &h).await;
        assert_eq!(fake.calls(), 0);
        assert!(h.view().supervisor_restart.is_none());

        let system = FakeRestart::new(Ok(RestartOutcome::TimedOut));
        request_supervisor_restart(&system, "1.2.3", &h).await;
        request_session_supervisor_restart(&Session(Ok(())), &fake, "1.2.3", &h).await;
        assert_eq!(fake.calls(), 1);
        assert_eq!(
            h.view().supervisor_restart.unwrap().status,
            "timed_out",
            "_faber_vm's failure stays shown"
        );

        let failing = FakeRestart::new(Ok(RestartOutcome::TimedOut));
        let h = test_handle(tmp.paths(), "1.2.3");
        request_session_supervisor_restart(&Session(Ok(())), &failing, "1.2.3", &h).await;
        assert_eq!(h.view().supervisor_restart.unwrap().status, "timed_out");
    }

    #[tokio::test]
    async fn restart_outcomes_are_surfaced_and_never_roll_back() {
        let tmp = TempRoot::new("restart-outcomes");
        let paths = tmp.paths();
        let h = test_handle(paths.clone(), "1.2.3");
        let seen = SupervisorVersion::try_from("1.0.0").unwrap();
        let ok = SupervisorVersion::try_from("1.2.3").unwrap();

        let fake = FakeRestart::new(Ok(RestartOutcome::TimedOut));
        request_supervisor_restart(&fake, "1.2.3", &h).await;
        assert_eq!(h.view().supervisor_restart.unwrap().status, "timed_out");

        let fake = FakeRestart::new(Ok(RestartOutcome::WrongVersion { seen }));
        request_supervisor_restart(&fake, "1.2.3", &h).await;
        let v = h.view().supervisor_restart.unwrap();
        assert_eq!(v.status, "wrong_version");
        assert!(v.detail.contains("1.0.0"), "{}", v.detail);

        let fake = FakeRestart::new(Err(RestartError::TooSoon {
            ago: Duration::from_secs(1),
            wait: Duration::from_secs(599),
        }));
        request_supervisor_restart(&fake, "1.2.3", &h).await;
        assert_eq!(h.view().supervisor_restart.unwrap().status, "too_soon");

        let fake = FakeRestart::new(Err(RestartError::NotAsked(
            crate::vm::client::ClientError::NoReply,
        )));
        request_supervisor_restart(&fake, "1.2.3", &h).await;
        assert_eq!(h.view().supervisor_restart.unwrap().status, "not_asked");

        // Restarted clears an earlier failure.
        let fake = FakeRestart::new(Ok(RestartOutcome::Restarted { version: ok }));
        request_supervisor_restart(&fake, "1.2.3", &h).await;
        assert!(h.view().supervisor_restart.is_none());
        // Nothing here touched the boot marker: no rollback is armed.
        assert!(!paths.marker().exists());
    }
}
