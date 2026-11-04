//! The menu-bar app updates itself, as the operator (ADR-0034, #80).
//!
//! The daemon runs as `_faber` and must not write the operator's app (ADR-0025),
//! so the app follows the daemon instead. The daemon's `/api/state` is a *hint*:
//! once it reports an installed, probation-passed release, the app fetches that
//! release's app asset itself and runs ADR-0031's whole check through
//! `faber-release`, the crate the daemon verifies with. **This file writes no
//! cryptographic or policy check of its own**: it decides *when* and *whether to
//! try*, then swaps the bundle and rolls it back if the new one does not live.
//!
//! The file is laid out as the decisions first (pure, unit-tested), the
//! filesystem swap behind [`Layout`] (tested against a temp dir), then the
//! network and the loop that glue them to the app.
//!
//! The watchdog is this same binary re-run with `--update-watchdog` from the
//! *old* bundle, so the code that decides a rollback is the code that was already
//! known to work.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use faber_release::{
    active_policy, check_bundle_version, describe_active_policy, is_newer, parse_sha256sums,
    parse_version, verify_digest, Release, Verdict,
};

/// The shortest wait the app will accept for itself. Mirrors the daemon's floor:
/// the wait is how a release its owner did not make gets caught.
pub const MIN_MATURITY_SECS: u64 = 3600;
/// The app's own maturity floor when the operator has not chosen one.
pub const DEFAULT_MATURITY_SECS: u64 = 86_400;
/// How long the popover must have been closed before the swap runs.
pub const IDLE_BEFORE_SWAP: Duration = Duration::from_secs(30);
/// How long a refused or unreachable release is left alone: the daemon's default
/// poll interval (`update.poll_interval_secs`), which is the rate it holds its own.
pub const HOLD_SECS: u64 = 1800;
/// How long the watchdog gives the new app to prove it is alive.
pub const WATCHDOG_WAIT_SECS: u64 = 30;
/// How long the new app must run before it deletes `faber.app.previous`.
pub const HEALTHY_SECS: u64 = 60;
/// How long the popover keeps saying "Updated to X".
const UPDATED_NOTICE_MS: u64 = 24 * 3600 * 1000;
/// Cadence of the local decision loop. Cheap: it reads memory unless it acts.
const TICK: Duration = Duration::from_secs(10);
/// How often the daemon's own `maturity_secs` is re-read for the mismatch warning.
const DAEMON_MATURITY_REFRESH_MS: u64 = 10 * 60 * 1000;
/// An app tarball is ~10 MB; anything near this is not one.
const MAX_ASSET_BYTES: usize = 200 * 1024 * 1024;

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const TARGET: &str = "aarch64-apple-darwin";
#[cfg(all(target_os = "macos", target_arch = "x86_64"))]
const TARGET: &str = "x86_64-apple-darwin";
#[cfg(not(all(
    target_os = "macos",
    any(target_arch = "aarch64", target_arch = "x86_64")
)))]
const TARGET: &str = "unsupported";

// ---------------------------------------------------------------------------
// Preferences: the operator's, never `config.toml`
// ---------------------------------------------------------------------------

/// The app's own preferences, in the operator's app-config directory, which
/// `_faber` cannot write. `config.toml` is `_faber`'s, so the floor must not come
/// from there (ADR-0034).
#[derive(Debug, Default, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Prefs {
    #[serde(default)]
    pub maturity_floor_secs: Option<u64>,
}

impl Prefs {
    /// The floor in force. A stored value under the minimum (a hand edit) is
    /// refused the same way a command would refuse it, and the default applies.
    pub fn floor_secs(&self) -> u64 {
        match self.maturity_floor_secs {
            Some(s) if s >= MIN_MATURITY_SECS => s,
            _ => DEFAULT_MATURITY_SECS,
        }
    }
}

/// Validate a floor the operator asked for.
pub fn validate_floor(secs: u64) -> Result<u64, String> {
    if secs < MIN_MATURITY_SECS {
        return Err(format!(
            "the app's update wait must be at least {MIN_MATURITY_SECS} seconds (one hour): the wait is how a bad release is caught before it installs"
        ));
    }
    Ok(secs)
}

fn prefs_path(dir: &Path) -> PathBuf {
    dir.join("prefs.json")
}

pub fn load_prefs(dir: &Path) -> Prefs {
    std::fs::read_to_string(prefs_path(dir))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

pub fn save_floor(dir: &Path, secs: u64) -> Result<u64, String> {
    let secs = validate_floor(secs)?;
    let prefs = Prefs {
        maturity_floor_secs: Some(secs),
    };
    write_json_atomic(&prefs_path(dir), &prefs).map_err(|e| format!("saving preferences: {e}"))?;
    Ok(secs)
}

/// The popover's warning when the app's floor and the daemon's wait disagree.
/// A daemon value lowered behind the operator's back shows up here (ADR-0034).
pub fn floor_mismatch(app_floor: u64, daemon_maturity: Option<u64>) -> Option<String> {
    let daemon = daemon_maturity?;
    (daemon != app_floor).then(|| {
        format!(
            "App update wait is {} but the daemon's is {}",
            human_secs(app_floor),
            human_secs(daemon)
        )
    })
}

pub fn human_secs(s: u64) -> String {
    if s >= 3600 && s.is_multiple_of(3600) {
        format!("{} h", s / 3600)
    } else if s >= 3600 {
        format!("{:.1} h", s as f64 / 3600.0)
    } else if s >= 60 {
        format!("{} min", s.div_ceil(60))
    } else {
        format!("{s} s")
    }
}

// ---------------------------------------------------------------------------
// The decisions
// ---------------------------------------------------------------------------

/// What the app reads from the daemon's `/api/state` `update` block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DaemonFacts {
    /// The daemon's running version, e.g. `0.4.1+gabc1234`.
    pub current: String,
    /// Set for one boot after the daemon rolled an update back.
    pub rolled_back_from: Option<String>,
    /// The boot self-check's version and status (`verified`, `pending`, ...).
    pub self_check: Option<(String, String)>,
}

/// The version the app should move to, if the daemon says one is installed.
///
/// "Installed and probation-passed" is read from the daemon's boot self-check
/// reaching `verified` **for the version it is running**: the daemon clears its
/// boot marker only when that settles (`self_check::prove_updater`), and
/// `/api/state` exposes no other probation signal. `None`, `pending`, `unproven`
/// and `refused` all wait. A hand-installed daemon has no self-check, so the app
/// does not follow it: hand installs are done by hand (ADR-0034 § The daemon
/// stops touching the app).
///
/// This is only the *trigger*; it never decides that an install is allowed. The
/// strictly-newer guard is [`plan`]'s, and the verification is the crate's.
pub fn target_version(d: &DaemonFacts) -> Option<String> {
    if d.rolled_back_from.is_some() {
        return None;
    }
    let (sc_version, sc_status) = d.self_check.as_ref()?;
    if sc_status != "verified" {
        return None;
    }
    let cur = parse_version(&d.current)?;
    if parse_version(sc_version)? != cur {
        return None;
    }
    Some(format!("{}.{}.{}", cur.0, cur.1, cur.2))
}

/// A release held back until `until_ms`, with the reason the popover shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hold {
    pub version: String,
    pub until_ms: u64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Nothing to do: no target, or the target is not above this app.
    Idle,
    /// The target was rolled back here; not retried until a newer release.
    Rejected(String),
    /// Held after a refusal or an outage; says why and until when.
    Held { reason: String, until_ms: u64 },
    /// Stage and verify this version.
    Attempt(String),
}

/// The whole *whether to try* decision. Forward-only is here: a target at or
/// below the running app, or one that does not parse, is `Idle`.
pub fn plan(
    target: Option<&str>,
    app_version: &str,
    now_ms: u64,
    hold: Option<&Hold>,
    rolled_back: &BTreeSet<String>,
) -> Plan {
    let Some(target) = target else {
        return Plan::Idle;
    };
    if !is_newer(target, app_version) {
        return Plan::Idle;
    }
    if rolled_back.contains(target) {
        return Plan::Rejected(target.to_string());
    }
    if let Some(h) = hold {
        if h.version == target && now_ms < h.until_ms {
            return Plan::Held {
                reason: h.reason.clone(),
                until_ms: h.until_ms,
            };
        }
    }
    Plan::Attempt(target.to_string())
}

/// `Ok` once a release is at least `floor_secs` old, else the seconds left. The
/// app keeps no clock of its own (ADR-0034 rejects a second one): GitHub's
/// `published_at` is the start, so a daemon reporting a release it never
/// installed cannot pull the app onto one that has not matured.
pub fn maturity(published_ms: u64, floor_secs: u64, now_ms: u64) -> Result<(), u64> {
    let ready = published_ms.saturating_add(floor_secs.saturating_mul(1000));
    if now_ms >= ready {
        Ok(())
    } else {
        Err((ready - now_ms).div_ceil(1000))
    }
}

/// Whether the swap may run now: the popover is closed and has been for the idle
/// window. `popover_closed_for` is `None` while the popover is open.
pub fn swap_due(popover_closed_for: Option<Duration>) -> bool {
    popover_closed_for.is_some_and(|d| d >= IDLE_BEFORE_SWAP)
}

/// What the new app writes at startup, so the watchdog can tell alive from not.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Heartbeat {
    pub pid: u32,
    pub version: String,
    pub at_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchVerdict {
    Healthy,
    Rollback(String),
}

/// The watchdog's decision: the new app is healthy only if it wrote a heartbeat
/// for the expected version after the swap began, and that process still runs.
pub fn watchdog_decision(
    hb: Option<&Heartbeat>,
    expected_version: &str,
    swap_started_ms: u64,
    pid_alive: bool,
) -> WatchVerdict {
    let Some(hb) = hb else {
        return WatchVerdict::Rollback("the new app never reported that it started".into());
    };
    if hb.version != expected_version {
        return WatchVerdict::Rollback(format!(
            "the app that started reports version {}, not {expected_version}",
            hb.version
        ));
    }
    if hb.at_ms < swap_started_ms {
        return WatchVerdict::Rollback("the new app never reported that it started".into());
    }
    if !pid_alive {
        return WatchVerdict::Rollback("the new app exited shortly after launching".into());
    }
    WatchVerdict::Healthy
}

// ---------------------------------------------------------------------------
// The swap, behind a seam
// ---------------------------------------------------------------------------

/// The three paths of a swap. All beside each other, so every rename is atomic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub installed: PathBuf,
    pub staged: PathBuf,
    pub previous: PathBuf,
}

impl Layout {
    /// `faber.app` → `.faber.app.new` and `faber.app.previous`, beside it.
    pub fn beside(installed: &Path) -> Option<Layout> {
        let name = installed.file_name()?.to_str()?;
        let dir = installed.parent()?;
        Some(Layout {
            installed: installed.to_path_buf(),
            staged: dir.join(format!(".{name}.new")),
            previous: dir.join(format!("{name}.previous")),
        })
    }

    /// Scratch directory the tarball is unpacked into before the bundle is
    /// moved to `staged`.
    pub fn extract_dir(&self) -> PathBuf {
        let name = self
            .installed
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("faber.app");
        self.installed.with_file_name(format!(".{name}.extract"))
    }

    /// `old → previous`, `staged → installed`. If the second rename fails the
    /// first is undone, so the installed bundle is never left missing.
    pub fn swap_in(&self) -> std::io::Result<()> {
        if !self.staged.exists() {
            return Err(std::io::Error::other("no staged bundle to install"));
        }
        if self.previous.exists() {
            std::fs::remove_dir_all(&self.previous)?;
        }
        std::fs::rename(&self.installed, &self.previous)?;
        if let Err(e) = std::fs::rename(&self.staged, &self.installed) {
            let _ = std::fs::rename(&self.previous, &self.installed);
            return Err(e);
        }
        Ok(())
    }

    /// Put `previous` back as `installed`, discarding the failed bundle.
    pub fn swap_back(&self) -> std::io::Result<()> {
        if !self.previous.exists() {
            return Err(std::io::Error::other("no previous bundle to restore"));
        }
        let failed = self.installed.with_extension("app.failed");
        if failed.exists() {
            std::fs::remove_dir_all(&failed)?;
        }
        if self.installed.exists() {
            std::fs::rename(&self.installed, &failed)?;
        }
        if let Err(e) = std::fs::rename(&self.previous, &self.installed) {
            // Put the failed one back rather than leave nothing at all.
            let _ = std::fs::rename(&failed, &self.installed);
            return Err(e);
        }
        let _ = std::fs::remove_dir_all(&failed);
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Files in the state directory
// ---------------------------------------------------------------------------

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(value).map_err(std::io::Error::other)?,
    )?;
    std::fs::rename(&tmp, path)
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Option<T> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct RefusedFile {
    #[serde(default)]
    versions: BTreeSet<String>,
    #[serde(default)]
    last_reason: Option<String>,
}

/// Versions a watchdog rolled back, so they are not retried until the next
/// release (ADR-0034 step 4).
pub fn load_rolled_back(state_dir: &Path) -> BTreeSet<String> {
    read_json::<RefusedFile>(&state_dir.join("rolled_back.json"))
        .map(|f| f.versions)
        .unwrap_or_default()
}

pub fn record_rollback(state_dir: &Path, version: &str, reason: &str) -> std::io::Result<()> {
    let path = state_dir.join("rolled_back.json");
    let mut f: RefusedFile = read_json(&path).unwrap_or_default();
    f.versions.insert(version.to_string());
    f.last_reason = Some(format!("{version}: {reason}"));
    write_json_atomic(&path, &f)
}

#[derive(Debug, Serialize, Deserialize)]
struct SwappedFile {
    from: String,
    to: String,
    at_ms: u64,
}

fn heartbeat_path(state_dir: &Path) -> PathBuf {
    state_dir.join("heartbeat.json")
}

/// Called by every app at startup: the watchdog's evidence of life.
pub fn write_heartbeat(state_dir: &Path, version: &str) {
    let hb = Heartbeat {
        pid: std::process::id(),
        version: version.to_string(),
        at_ms: now_ms(),
    };
    let _ = write_json_atomic(&heartbeat_path(state_dir), &hb);
}

/// "Updated to X" for the popover, while the marker is fresh and says this very
/// version is the one running.
pub fn updated_notice(state_dir: &Path, running: &str, now_ms: u64) -> Option<String> {
    let s: SwappedFile = read_json(&state_dir.join("swapped.json"))?;
    (s.to == running && now_ms.saturating_sub(s.at_ms) < UPDATED_NOTICE_MS).then_some(s.to)
}

/// The rolled-back note for the popover: the last refusal the watchdog recorded.
/// Shown only while it concerns a version above the running app, so a later
/// successful update clears it.
pub fn last_rollback_reason(state_dir: &Path, running: &str) -> Option<String> {
    let f: RefusedFile = read_json(&state_dir.join("rolled_back.json"))?;
    f.versions
        .iter()
        .any(|v| is_newer(v, running))
        .then_some(f.last_reason)?
}

// ---------------------------------------------------------------------------
// What the popover shows
// ---------------------------------------------------------------------------

/// The app-update lines of a snapshot. Plain strings, so the webview does no
/// logic of its own.
#[derive(Debug, Clone, Default, Serialize)]
pub struct AppUpdateView {
    /// `idle`, `checking`, `waiting` (maturing), `staged` (ready, waiting for
    /// idle), `held`, `installing` or `disabled` (not running from a bundle).
    pub status: String,
    pub version: Option<String>,
    pub detail: Option<String>,
    pub floor_secs: u64,
    pub daemon_maturity_secs: Option<u64>,
    /// Set when the app's floor and the daemon's wait differ.
    pub floor_warning: Option<String>,
    /// "Updated to X", for a day after a swap.
    pub updated_to: Option<String>,
    /// A rollback the watchdog recorded, for the popover to say why.
    pub rolled_back: Option<String>,
}

// ---------------------------------------------------------------------------
// The release fetch, verify and stage
// ---------------------------------------------------------------------------

/// Why an attempt did not stage a bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The release or bundle failed a check: held one poll interval.
    Refused(String),
    /// A check could not run (offline, GitHub, TUF): held one poll interval.
    Unavailable(String),
    /// Not old enough yet: the seconds until it is.
    Immature(u64),
}

/// `owner/repo` from the compiled-in policy's signing workflow URL, so the app
/// can never fetch releases from a repository its policy would not trust.
pub fn policy_repo(signer_workflow: &str) -> Option<String> {
    let rest = signer_workflow.strip_prefix("https://github.com/")?;
    let mut parts = rest.splitn(3, '/');
    let owner = parts.next().filter(|s| !s.is_empty())?;
    let repo = parts.next().filter(|s| !s.is_empty())?;
    parts.next()?;
    Some(format!("{owner}/{repo}"))
}

/// `YYYY-MM-DDTHH:MM:SSZ` (GitHub's `published_at`) → ms since the epoch.
pub fn parse_rfc3339_ms(s: &str) -> Option<u64> {
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-');
    let year: i64 = d.next()?.parse().ok()?;
    let month: i64 = d.next()?.parse().ok()?;
    let day: i64 = d.next()?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let hms_end = time
        .find(|c: char| !(c.is_ascii_digit() || c == ':'))
        .unwrap_or(time.len());
    let mut t = time[..hms_end].split(':');
    let hh: u64 = t.next()?.parse().ok()?;
    let mm: u64 = t.next()?.parse().ok()?;
    let ss: u64 = t.next().unwrap_or("0").parse().ok()?;
    if hh > 23 || mm > 59 || ss > 60 {
        return None;
    }
    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    if days < 0 {
        return None;
    }
    Some((days as u64 * 86400 + hh * 3600 + mm * 60 + ss) * 1000)
}

fn unavailable(what: &str, e: impl std::fmt::Display) -> Failure {
    Failure::Unavailable(format!("{what}: {e}"))
}

struct Net {
    client: reqwest::Client,
    repo: String,
}

#[derive(Deserialize)]
struct GitObject {
    sha: String,
    #[serde(rename = "type")]
    kind: String,
}
#[derive(Deserialize)]
struct GitRef {
    object: GitObject,
}
#[derive(Deserialize)]
struct GitTag {
    object: GitObject,
}

fn lower_sha(s: &str) -> bool {
    s.len() == 40
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Net {
    async fn get_json<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
    ) -> Result<Option<T>, Failure> {
        let url = format!("https://api.github.com/{path}");
        let resp = self
            .client
            .get(&url)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| unavailable("GitHub", e))?;
        if resp.status().as_u16() == 404 {
            return Ok(None);
        }
        if !resp.status().is_success() {
            return Err(Failure::Unavailable(format!(
                "GitHub answered HTTP {} for {path}",
                resp.status()
            )));
        }
        resp.json()
            .await
            .map(Some)
            .map_err(|e| unavailable("GitHub", e))
    }

    /// The commit a tag points to, annotated tags peeled. `tag` is built from a
    /// parsed version, so it is digits and dots.
    async fn tag_commit(&self, tag: &str) -> Result<String, Failure> {
        let repo = &self.repo;
        let Some(r) = self
            .get_json::<GitRef>(&format!("repos/{repo}/git/ref/tags/{tag}"))
            .await?
        else {
            return Err(Failure::Refused(format!(
                "tag {tag} does not exist in {repo}"
            )));
        };
        let mut object = r.object;
        for _ in 0..5 {
            if !lower_sha(&object.sha) {
                return Err(Failure::Refused(format!(
                    "tag {tag}: object sha is not 40 lowercase hex"
                )));
            }
            match object.kind.as_str() {
                "commit" => return Ok(object.sha),
                "tag" => {
                    let sha = object.sha.clone();
                    object = self
                        .get_json::<GitTag>(&format!("repos/{repo}/git/tags/{sha}"))
                        .await?
                        .ok_or_else(|| Failure::Unavailable(format!("tag object {sha} not found")))?
                        .object;
                }
                other => {
                    return Err(Failure::Refused(format!(
                        "tag {tag} points at a {other}, not a commit"
                    )))
                }
            }
        }
        Err(Failure::Refused(format!("tag {tag} is nested too deep")))
    }

    /// An asset's bytes. Anonymous: the repository is public after the cut.
    async fn asset(&self, id: u64, name: &str) -> Result<Vec<u8>, Failure> {
        let url = format!(
            "https://api.github.com/repos/{}/releases/assets/{id}",
            self.repo
        );
        let resp = self
            .client
            .get(&url)
            .header("Accept", "application/octet-stream")
            .timeout(Duration::from_secs(600))
            .send()
            .await
            .map_err(|e| unavailable(&format!("downloading {name}"), e))?;
        if !resp.status().is_success() {
            return Err(Failure::Unavailable(format!(
                "downloading {name}: HTTP {}",
                resp.status()
            )));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| unavailable(&format!("downloading {name}"), e))?;
        if bytes.len() > MAX_ASSET_BYTES {
            return Err(Failure::Refused(format!("{name} is implausibly large")));
        }
        Ok(bytes.to_vec())
    }
}

fn find<'a>(r: &'a Release, name: &str) -> Result<&'a faber_release::ReleaseAsset, Failure> {
    r.assets
        .iter()
        .find(|a| a.name == name)
        .ok_or_else(|| Failure::Refused(format!("release {} has no {name} asset", r.tag_name)))
}

fn run_tool(cmd: &mut std::process::Command, what: &str) -> Result<String, Failure> {
    let out = cmd
        .output()
        .map_err(|e| Failure::Refused(format!("{what}: {e}")))?;
    if !out.status.success() {
        return Err(Failure::Refused(format!(
            "{what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A bundle staged and verified, ready to swap in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Staged {
    pub version: String,
}

/// Fetch `version`'s release, hold it to ADR-0031's whole check, and stage the
/// bundle as `layout.staged`. Every check is `faber-release`'s; the steps here
/// are the order they run in and the plumbing around them.
async fn stage(
    client: &reqwest::Client,
    version: &str,
    floor_secs: u64,
    layout: &Layout,
    state_dir: &Path,
) -> Result<Staged, Failure> {
    let tag = format!("v{version}");
    let policy = active_policy();
    let repo = policy_repo(policy.signer_workflow)
        .ok_or_else(|| Failure::Refused("the compiled-in policy names no repository".into()))?;
    let net = Net {
        client: client.clone(),
        repo: repo.clone(),
    };

    let release: Release = net
        .get_json(&format!("repos/{repo}/releases/tags/{tag}"))
        .await?
        .ok_or_else(|| Failure::Refused(format!("release {tag} does not exist")))?;
    if release.tag_name != tag || release.draft || release.prerelease {
        return Err(Failure::Refused(format!(
            "{tag} is not a published release"
        )));
    }
    let published = release
        .published_at
        .as_deref()
        .and_then(parse_rfc3339_ms)
        .ok_or_else(|| Failure::Refused(format!("release {tag} has no usable published_at")))?;
    maturity(published, floor_secs, now_ms()).map_err(Failure::Immature)?;

    let app_name = format!("faber-app-{TARGET}.tar.gz");
    let sums_asset = find(&release, "SHA256SUMS")?;
    let bundle_asset = find(&release, "SHA256SUMS.sigstore.json")?;
    let app_asset = find(&release, &app_name)?;

    let sums_bytes = net.asset(sums_asset.id, "SHA256SUMS").await?;
    let bundle_bytes = net
        .asset(bundle_asset.id, "SHA256SUMS.sigstore.json")
        .await?;
    let commit = net.tag_commit(&tag).await?;
    let tuf_cache = state_dir.join("tuf");
    let ua = format!("faber-menubar/{}", env!("CARGO_PKG_VERSION"));
    let root = faber_release::fetch_trusted_root(&tuf_cache, &ua)
        .await
        .map_err(|e| Failure::Unavailable(format!("{e} -- no cached root is used")))?;
    match faber_release::check(policy, &root, &sums_bytes, &bundle_bytes, &commit) {
        Verdict::Verified => {}
        Verdict::Refused(r) => return Err(Failure::Refused(r)),
        Verdict::Unavailable(r) => return Err(Failure::Unavailable(r)),
    }

    let sums = parse_sha256sums(&String::from_utf8_lossy(&sums_bytes));
    let tarball = net.asset(app_asset.id, &app_name).await?;
    verify_digest(&sums, &app_name, &tarball).map_err(Failure::Refused)?;

    // Unpack beside the installed bundle, so the final rename is atomic.
    let extract = layout.extract_dir();
    let _ = std::fs::remove_dir_all(&extract);
    std::fs::create_dir_all(&extract)
        .map_err(|e| Failure::Unavailable(format!("{}: {e}", extract.display())))?;
    let tar_file = extract.join("app.tar.gz");
    std::fs::write(&tar_file, &tarball)
        .map_err(|e| Failure::Unavailable(format!("writing the tarball: {e}")))?;
    let result = unpack_and_bind(&tar_file, &extract, &tag, layout);
    let _ = std::fs::remove_dir_all(&extract);
    result?;
    Ok(Staged {
        version: version.to_string(),
    })
}

fn unpack_and_bind(
    tar_file: &Path,
    extract: &Path,
    tag: &str,
    layout: &Layout,
) -> Result<(), Failure> {
    run_tool(
        std::process::Command::new("tar")
            .arg("xzf")
            .arg(tar_file)
            .arg("-C")
            .arg(extract),
        "unpacking the app tarball",
    )?;
    let bundle = extract.join("faber.app");
    if !bundle.is_dir() {
        return Err(Failure::Refused(
            "the app tarball did not contain faber.app".into(),
        ));
    }
    let short = run_tool(
        std::process::Command::new("plutil")
            .args(["-extract", "CFBundleShortVersionString", "raw", "-o", "-"])
            .arg(bundle.join("Contents/Info.plist")),
        "reading the bundle's version",
    )?;
    check_bundle_version(&short, tag).map_err(Failure::Refused)?;
    let _ = std::fs::remove_dir_all(&layout.staged);
    std::fs::rename(&bundle, &layout.staged)
        .map_err(|e| Failure::Unavailable(format!("staging the bundle: {e}")))
}

// ---------------------------------------------------------------------------
// The loop
// ---------------------------------------------------------------------------

/// What the loop needs from the app, so it can be driven without Tauri.
pub trait Host: Send + Sync + 'static {
    fn daemon_facts(&self) -> Option<DaemonFacts>;
    /// How long the popover has been closed; `None` while it is open.
    fn popover_closed_for(&self) -> Option<Duration>;
    fn publish(&self, view: AppUpdateView);
    /// Quit this app, after the new one is launched.
    fn quit(&self);
}

/// The installed bundle this process is running from, if it is one. A dev run
/// (`tauri dev`) is not, so it never updates: the app never installs itself fresh.
pub fn running_bundle() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .map(Path::to_path_buf)
}

/// Startup housekeeping for every app: say that it is alive, drop a stale staged
/// bundle, and delete `.previous` once this app has run healthy for a minute.
pub fn on_startup(state_dir: PathBuf, version: &'static str) {
    write_heartbeat(&state_dir, version);
    let Some(layout) = running_bundle().and_then(|b| Layout::beside(&b)) else {
        return;
    };
    let _ = std::fs::remove_dir_all(&layout.staged);
    let _ = std::fs::remove_dir_all(layout.extract_dir());
    if layout.previous.exists() {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(HEALTHY_SECS));
            let _ = std::fs::remove_dir_all(&layout.previous);
        });
    }
}

/// `daemon` is bound to the daemon's control socket (ADR-0028) and reads its
/// settings; `net` reaches GitHub for the release itself.
pub async fn run(
    host: std::sync::Arc<dyn Host>,
    daemon: reqwest::Client,
    net: reqwest::Client,
    base_url: String,
    state_dir: PathBuf,
    app_version: &'static str,
) {
    let layout = running_bundle().and_then(|b| Layout::beside(&b));
    let mut hold: Option<Hold> = None;
    let mut staged: Option<Staged> = None;
    let mut daemon_maturity: Option<u64> = None;
    let mut maturity_read_ms = 0u64;

    loop {
        tokio::time::sleep(TICK).await;
        let now = now_ms();
        let floor = load_prefs(&state_dir).floor_secs();
        if now.saturating_sub(maturity_read_ms) >= DAEMON_MATURITY_REFRESH_MS {
            maturity_read_ms = now;
            if let Some(v) = fetch_daemon_maturity(&daemon, &base_url).await {
                daemon_maturity = Some(v);
            }
        }
        let mut view = AppUpdateView {
            status: "idle".into(),
            floor_secs: floor,
            daemon_maturity_secs: daemon_maturity,
            floor_warning: floor_mismatch(floor, daemon_maturity),
            updated_to: updated_notice(&state_dir, app_version, now),
            rolled_back: last_rollback_reason(&state_dir, app_version),
            ..AppUpdateView::default()
        };
        let Some(layout) = &layout else {
            view.status = "disabled".into();
            host.publish(view);
            continue;
        };

        let target = host.daemon_facts().and_then(|d| target_version(&d));
        let rolled_back = load_rolled_back(&state_dir);
        match plan(
            target.as_deref(),
            app_version,
            now,
            hold.as_ref(),
            &rolled_back,
        ) {
            Plan::Idle => {}
            Plan::Rejected(v) => {
                view.version = Some(v);
                view.status = "held".into();
                view.detail =
                    Some("rolled back on this Mac; not retried until the next release".into());
            }
            Plan::Held { reason, .. } => {
                view.version = target.clone();
                view.status = "held".into();
                view.detail = Some(reason);
            }
            Plan::Attempt(v) => {
                if staged.as_ref().map(|s| s.version.as_str()) != Some(v.as_str()) {
                    staged = None;
                    view.version = Some(v.clone());
                    view.status = "checking".into();
                    host.publish(view.clone());
                    match stage(&net, &v, floor, layout, &state_dir).await {
                        Ok(s) => staged = Some(s),
                        Err(Failure::Immature(secs)) => {
                            hold = Some(Hold {
                                version: v.clone(),
                                until_ms: now + secs * 1000,
                                reason: format!("matures in {}", human_secs(secs)),
                            });
                            view.status = "waiting".into();
                            view.detail = Some(format!("matures in {}", human_secs(secs)));
                        }
                        Err(Failure::Refused(r) | Failure::Unavailable(r)) => {
                            hold = Some(Hold {
                                version: v.clone(),
                                until_ms: now + HOLD_SECS * 1000,
                                reason: r.clone(),
                            });
                            view.status = "held".into();
                            view.detail = Some(r);
                        }
                    }
                }
                if let Some(s) = &staged {
                    view.version = Some(s.version.clone());
                    if swap_due(host.popover_closed_for()) {
                        view.status = "installing".into();
                        host.publish(view.clone());
                        match install(layout, &state_dir, app_version, &s.version) {
                            Ok(()) => {
                                host.quit();
                                return;
                            }
                            Err(e) => {
                                hold = Some(Hold {
                                    version: s.version.clone(),
                                    until_ms: now_ms() + HOLD_SECS * 1000,
                                    reason: e.clone(),
                                });
                                staged = None;
                                let _ = std::fs::remove_dir_all(&layout.staged);
                                view.status = "held".into();
                                view.detail = Some(e);
                            }
                        }
                    } else {
                        view.status = "staged".into();
                        view.detail =
                            Some("installs when the popover has been closed for a moment".into());
                    }
                }
            }
        }
        host.publish(view);
    }
}

async fn fetch_daemon_maturity(client: &reqwest::Client, base: &str) -> Option<u64> {
    let v: serde_json::Value = client
        .get(format!("{base}/api/settings"))
        .timeout(Duration::from_secs(4))
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let at = |k: &str| v.get(k)?.get("update")?.get("maturity_secs")?.as_u64();
    at("config").or_else(|| at("defaults"))
}

// ---------------------------------------------------------------------------
// Install: watchdog first, then the swap, then the new app
// ---------------------------------------------------------------------------

fn install(layout: &Layout, state_dir: &Path, from: &str, to: &str) -> Result<(), String> {
    use std::os::unix::process::CommandExt;

    let started = now_ms();
    write_json_atomic(
        &state_dir.join("swapped.json"),
        &SwappedFile {
            from: from.to_string(),
            to: to.to_string(),
            at_ms: started,
        },
    )
    .map_err(|e| format!("recording the swap: {e}"))?;

    // The watchdog runs from the OLD bundle's code, before it is renamed away: a
    // running image survives its directory being renamed.
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    std::process::Command::new(exe)
        .arg("--update-watchdog")
        .arg(&layout.installed)
        .arg(state_dir)
        .arg(to)
        .arg(started.to_string())
        .arg(WATCHDOG_WAIT_SECS.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("starting the watchdog: {e}"))?;

    layout
        .swap_in()
        .map_err(|e| format!("swapping the bundle: {e}"))?;
    open_new(&layout.installed).map_err(|e| format!("launching the new app: {e}"))
}

fn open_new(bundle: &Path) -> std::io::Result<()> {
    let st = std::process::Command::new("open")
        .arg("-n")
        .arg(bundle)
        .status()?;
    if st.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!("open exited with {st}")))
    }
}

fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid.to_string())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// `faber-menubar --update-watchdog <installed> <state_dir> <version> <started_ms> <wait_secs>`.
/// Returns true when this process was the watchdog (so `main` must not start the
/// app). Exits after one decision.
pub fn maybe_run_watchdog() -> bool {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) != Some("--update-watchdog") || args.len() != 7 {
        return false;
    }
    let installed = PathBuf::from(&args[2]);
    let state_dir = PathBuf::from(&args[3]);
    let version = args[4].clone();
    let started: u64 = args[5].parse().unwrap_or(0);
    let wait: u64 = args[6].parse().unwrap_or(WATCHDOG_WAIT_SECS);
    std::thread::sleep(Duration::from_secs(wait));

    // No `.previous` means the swap never happened (it failed and was undone, so
    // the old app is still the one running): nothing to judge, nothing to restore.
    let Some(layout) = Layout::beside(&installed).filter(|l| l.previous.exists()) else {
        return true;
    };
    let hb: Option<Heartbeat> = read_json(&heartbeat_path(&state_dir));
    let alive = hb.as_ref().is_some_and(|h| pid_alive(h.pid));
    if let WatchVerdict::Rollback(reason) = watchdog_decision(hb.as_ref(), &version, started, alive)
    {
        if let Some(h) = hb
            .as_ref()
            .filter(|h| alive && h.version == version && h.at_ms >= started)
        {
            // Only the new app, identified by its fresh heartbeat, is ever killed.
            let _ = std::process::Command::new("kill")
                .arg(h.pid.to_string())
                .status();
        }
        let _ = record_rollback(&state_dir, &version, &reason);
        if layout.swap_back().is_ok() {
            let _ = open_new(&layout.installed);
        }
    }
    true
}

/// `faber-menubar --provenance-policy`: print the verification policy compiled
/// into this binary and exit, in the format of the daemon's `faber
/// provenance-policy` (both are `faber_release::describe_active_policy`).
/// `release.yml` runs it on the built bundle and refuses a build whose policy is
/// the sandbox one or names another repository, which every update would
/// otherwise refuse after the release is out. Returns true when this process was
/// that one-shot, so `main` must not start the app.
pub fn maybe_print_policy() -> bool {
    let args: Vec<String> = std::env::args().collect();
    if !asks_for_policy(&args) {
        return false;
    }
    print!("{}", describe_active_policy());
    true
}

fn asks_for_policy(args: &[String]) -> bool {
    args.len() == 2 && args[1] == "--provenance-policy"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(current: &str, sc: Option<(&str, &str)>) -> DaemonFacts {
        DaemonFacts {
            current: current.into(),
            rolled_back_from: None,
            self_check: sc.map(|(v, s)| (v.into(), s.into())),
        }
    }

    // --- trigger ---

    #[test]
    fn trigger_needs_the_daemons_probation_to_have_passed_for_its_own_version() {
        let ok = facts("0.4.1+gabc1234", Some(("0.4.1+gabc1234", "verified")));
        assert_eq!(target_version(&ok).as_deref(), Some("0.4.1"));
        for status in ["pending", "unproven", "refused"] {
            let d = facts("0.4.1+gabc1234", Some(("0.4.1+gabc1234", status)));
            assert_eq!(target_version(&d), None, "{status}");
        }
        // On probation, or hand-installed: no self-check yet.
        assert_eq!(target_version(&facts("0.4.1", None)), None);
        // A record about another version proves nothing about this one.
        let stale = facts("0.4.1", Some(("0.4.0", "verified")));
        assert_eq!(target_version(&stale), None);
    }

    #[test]
    fn trigger_is_off_after_a_daemon_rollback() {
        let mut d = facts("0.4.0", Some(("0.4.0", "verified")));
        d.rolled_back_from = Some("0.4.1".into());
        assert_eq!(target_version(&d), None);
    }

    // --- forward-only, hold, rejected ---

    #[test]
    fn forward_only() {
        let none = BTreeSet::new();
        assert_eq!(
            plan(Some("0.4.1"), "0.4.0", 0, None, &none),
            Plan::Attempt("0.4.1".into())
        );
        assert_eq!(plan(Some("0.4.0"), "0.4.0", 0, None, &none), Plan::Idle);
        assert_eq!(plan(Some("0.3.9"), "0.4.0", 0, None, &none), Plan::Idle);
        assert_eq!(
            plan(Some("0.10.0"), "0.9.0", 0, None, &none),
            Plan::Attempt("0.10.0".into())
        );
        assert_eq!(plan(Some("junk"), "0.4.0", 0, None, &none), Plan::Idle);
        assert_eq!(plan(None, "0.4.0", 0, None, &none), Plan::Idle);
    }

    #[test]
    fn a_refused_release_is_held_one_interval_and_then_retried() {
        let none = BTreeSet::new();
        let hold = Hold {
            version: "0.4.1".into(),
            until_ms: 5_000,
            reason: "digest mismatch".into(),
        };
        assert_eq!(
            plan(Some("0.4.1"), "0.4.0", 4_999, Some(&hold), &none),
            Plan::Held {
                reason: "digest mismatch".into(),
                until_ms: 5_000
            }
        );
        assert_eq!(
            plan(Some("0.4.1"), "0.4.0", 5_000, Some(&hold), &none),
            Plan::Attempt("0.4.1".into())
        );
        // A newer release ends the hold at once.
        assert_eq!(
            plan(Some("0.4.2"), "0.4.0", 1, Some(&hold), &none),
            Plan::Attempt("0.4.2".into())
        );
    }

    #[test]
    fn a_rolled_back_version_is_not_retried_but_the_next_one_is() {
        let rb: BTreeSet<String> = ["0.4.1".to_string()].into();
        assert_eq!(
            plan(Some("0.4.1"), "0.4.0", 0, None, &rb),
            Plan::Rejected("0.4.1".into())
        );
        assert_eq!(
            plan(Some("0.4.2"), "0.4.0", 0, None, &rb),
            Plan::Attempt("0.4.2".into())
        );
    }

    // --- floor ---

    #[test]
    fn a_release_younger_than_the_floor_does_not_move_the_app() {
        let day = DEFAULT_MATURITY_SECS * 1000;
        assert_eq!(
            maturity(1_000, DEFAULT_MATURITY_SECS, 1_000 + day - 1),
            Err(1)
        );
        assert_eq!(maturity(1_000, DEFAULT_MATURITY_SECS, 1_000 + day), Ok(()));
        assert_eq!(maturity(0, 3600, 1_800_000), Err(1800));
    }

    #[test]
    fn the_floor_defaults_to_a_day_and_refuses_under_an_hour() {
        assert_eq!(Prefs::default().floor_secs(), DEFAULT_MATURITY_SECS);
        assert_eq!(
            Prefs {
                maturity_floor_secs: Some(3600)
            }
            .floor_secs(),
            3600
        );
        // A hand-edited value under the minimum is refused, not honoured.
        assert_eq!(
            Prefs {
                maturity_floor_secs: Some(60)
            }
            .floor_secs(),
            DEFAULT_MATURITY_SECS
        );
        assert!(validate_floor(3599).is_err());
        assert_eq!(validate_floor(3600), Ok(3600));
    }

    #[test]
    fn the_floor_is_stored_in_the_operators_directory_and_refused_under_an_hour() {
        let dir = tempdir("prefs");
        assert!(save_floor(&dir, 120).is_err());
        assert_eq!(load_prefs(&dir).floor_secs(), DEFAULT_MATURITY_SECS);
        assert_eq!(save_floor(&dir, 7200), Ok(7200));
        assert_eq!(load_prefs(&dir).floor_secs(), 7200);
    }

    #[test]
    fn the_popover_warns_only_when_the_two_waits_differ() {
        assert_eq!(floor_mismatch(86_400, Some(86_400)), None);
        assert_eq!(floor_mismatch(86_400, None), None);
        let w = floor_mismatch(86_400, Some(3600)).expect("differs");
        assert!(w.contains("24 h") && w.contains("1 h"), "{w}");
    }

    // --- idle ---

    #[test]
    fn the_swap_waits_for_a_closed_popover_and_an_idle_moment() {
        assert!(!swap_due(None));
        assert!(!swap_due(Some(Duration::from_secs(29))));
        assert!(swap_due(Some(Duration::from_secs(30))));
    }

    // --- watchdog ---

    #[test]
    fn the_watchdog_rolls_back_unless_the_new_app_is_alive() {
        let hb = |version: &str, at_ms: u64| Heartbeat {
            pid: 1,
            version: version.into(),
            at_ms,
        };
        assert_eq!(
            watchdog_decision(Some(&hb("0.4.1", 200)), "0.4.1", 100, true),
            WatchVerdict::Healthy
        );
        for (h, alive) in [
            (None, false),
            (Some(hb("0.4.1", 200)), false), // started, then died
            (Some(hb("0.4.1", 50)), true),   // the old app's heartbeat
            (Some(hb("0.4.0", 200)), true),  // the wrong app came up
        ] {
            assert!(
                matches!(
                    watchdog_decision(h.as_ref(), "0.4.1", 100, alive),
                    WatchVerdict::Rollback(_)
                ),
                "{h:?} alive={alive}"
            );
        }
    }

    // --- the swap, against a temp dir ---

    fn tempdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "faber-updater-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn bundle(path: &Path, marker: &str) {
        std::fs::create_dir_all(path.join("Contents")).unwrap();
        std::fs::write(path.join("Contents/marker"), marker).unwrap();
    }

    fn marker(path: &Path) -> String {
        std::fs::read_to_string(path.join("Contents/marker")).unwrap()
    }

    #[test]
    fn the_layout_sits_beside_the_installed_bundle() {
        let l = Layout::beside(Path::new("/Applications/faber.app")).unwrap();
        assert_eq!(l.staged, Path::new("/Applications/.faber.app.new"));
        assert_eq!(l.previous, Path::new("/Applications/faber.app.previous"));
    }

    #[test]
    fn swap_in_keeps_the_old_bundle_as_previous() {
        let dir = tempdir("swap");
        let l = Layout::beside(&dir.join("faber.app")).unwrap();
        bundle(&l.installed, "old");
        bundle(&l.staged, "new");
        l.swap_in().unwrap();
        assert_eq!(marker(&l.installed), "new");
        assert_eq!(marker(&l.previous), "old");
        assert!(!l.staged.exists());
    }

    #[test]
    fn swap_in_replaces_a_stale_previous_and_needs_a_staged_bundle() {
        let dir = tempdir("stale");
        let l = Layout::beside(&dir.join("faber.app")).unwrap();
        bundle(&l.installed, "old");
        bundle(&l.previous, "ancient");
        assert!(l.swap_in().is_err(), "no staged bundle");
        assert_eq!(
            marker(&l.installed),
            "old",
            "the installed bundle is untouched"
        );
        bundle(&l.staged, "new");
        l.swap_in().unwrap();
        assert_eq!(marker(&l.previous), "old");
    }

    #[test]
    fn swap_back_restores_the_previous_bundle() {
        let dir = tempdir("back");
        let l = Layout::beside(&dir.join("faber.app")).unwrap();
        bundle(&l.installed, "old");
        bundle(&l.staged, "new");
        l.swap_in().unwrap();
        l.swap_back().unwrap();
        assert_eq!(marker(&l.installed), "old");
        assert!(!l.previous.exists());
        assert!(l.swap_back().is_err(), "nothing left to restore");
        assert_eq!(marker(&l.installed), "old");
    }

    #[test]
    fn a_rollback_is_recorded_so_the_version_is_not_retried() {
        let dir = tempdir("rb");
        assert!(load_rolled_back(&dir).is_empty());
        record_rollback(&dir, "0.4.1", "the new app exited shortly after launching").unwrap();
        assert!(load_rolled_back(&dir).contains("0.4.1"));
        assert!(last_rollback_reason(&dir, "0.4.0")
            .unwrap()
            .contains("0.4.1"));
        assert_eq!(last_rollback_reason(&dir, "0.4.2"), None);
    }

    #[test]
    fn updated_notice_names_only_the_running_version_and_fades() {
        let dir = tempdir("notice");
        write_json_atomic(
            &dir.join("swapped.json"),
            &SwappedFile {
                from: "0.4.0".into(),
                to: "0.4.1".into(),
                at_ms: 1000,
            },
        )
        .unwrap();
        assert_eq!(
            updated_notice(&dir, "0.4.1", 2000).as_deref(),
            Some("0.4.1")
        );
        assert_eq!(updated_notice(&dir, "0.4.0", 2000), None, "rolled back");
        assert_eq!(
            updated_notice(&dir, "0.4.1", 1000 + UPDATED_NOTICE_MS),
            None
        );
    }

    // --- plumbing ---

    #[test]
    fn only_the_bare_flag_asks_for_the_policy() {
        let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(asks_for_policy(&args(&[
            "faber-menubar",
            "--provenance-policy"
        ])));
        assert!(!asks_for_policy(&args(&["faber-menubar"])));
        assert!(!asks_for_policy(&args(&[
            "faber-menubar",
            "--provenance-policy",
            "extra"
        ])));
        assert!(!asks_for_policy(&args(&[
            "faber-menubar",
            "--update-watchdog"
        ])));
    }

    #[test]
    fn the_repository_comes_from_the_compiled_in_policy() {
        assert_eq!(
            policy_repo(faber_release::RELEASE_POLICY.signer_workflow).as_deref(),
            Some("rejifald/faber")
        );
        assert_eq!(policy_repo("https://evil.example/a/b/c"), None);
    }

    #[test]
    fn published_at_parses() {
        assert_eq!(parse_rfc3339_ms("1970-01-02T00:00:00Z"), Some(86_400_000));
        assert_eq!(
            parse_rfc3339_ms("2026-10-17T12:00:00Z"),
            Some(1_792_238_400_000)
        );
        assert_eq!(parse_rfc3339_ms("nope"), None);
    }
}
