//! ADR-0049 §7: only a verified golden image runs jobs.
//!
//! `faber golden verify <name>` (and the daemon, on its own) boots one clone of
//! the image exactly as a job does: through the session supervisor, behind the
//! wall, with a fresh machine identifier, holding one of the two macOS places
//! ([`super::job::run_verify`]). The guest agent runs the image's checks against
//! expectations faber sends from the image's manifest ([`expect_from_manifest`])
//! and the launcher passes up only fixed codes ([`Check`]): `_faber` never reads a
//! guest's words. The wall is proven from the anchor's counters, as for a job.
//!
//! **The record.** A result is stored in `_faber`'s state directory
//! ([`RECORD_FILE`]), keyed by the image's file hashes and the host's macOS
//! build ([`ImageKey`]). Either changing makes the image unverified again
//! ([`judge`]), and the daemon re-runs the verification itself: once when the key
//! is new, then with a back-off after each failure ([`due`]), never in a tight
//! loop.
//!
//! **The gate.** Admission sends no macOS job to an image whose [`Standing`] is
//! not [`Standing::Verified`] ([`super::admit::VmRefusal::ImageUnverified`]).
//!
//! Hashing a 40 GB disk is not free, so each file's SHA-256 is kept beside the
//! file's identity (device, inode, size, mtime, ctime) and reused while that is
//! unchanged ([`HashCache`]). The image store is root's, every file `0444`, so only
//! root can change a file, and any change moves its ctime.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use super::proto::{
    BuildToken, Check, ClaimId, Expect, RuntimeExpect, RuntimePlatform, RuntimeVersion,
    XcodeExpect, XcodeVersion, MAX_RUNTIMES,
};
use crate::audit::now_ms;

/// The record's file name in the state directory.
pub const RECORD_FILE: &str = "golden-verify.json";

/// The record's schema. A record of another schema is ignored, as if absent:
/// the images are verified again.
///
/// Raised whenever a check gets stricter, so a pass under the weaker check is
/// not trusted: 2 runs the `xcode` check as `runner` and links a C and a Swift
/// program, after an image that passed as root served jobs that could not link.
pub const SCHEMA: u32 = 2;

/// The schemas whose file hashes still hold: only their results are dropped,
/// so a stricter check does not cost hashing every image again.
const HASHES_STILL_HOLD: [u32; 1] = [1];

/// How often the daemon re-judges every image (a changed file, a host update).
pub const WATCH_INTERVAL: Duration = Duration::from_secs(600);

/// The back-off after the first failed automatic verification, doubled after
/// each further one up to [`MAX_BACKOFF`].
pub const FIRST_BACKOFF: Duration = Duration::from_secs(3600);
pub const MAX_BACKOFF: Duration = Duration::from_secs(24 * 3600);

/// The back-off after a run that could not finish (faber's own path: no VM
/// session, adoption pending, no macOS place) or was cancelled, doubled up to
/// [`MAX_RUN_BACKOFF`]: such a failure says nothing about the image, so it is
/// retried soon, unlike a failed check.
pub const FIRST_RUN_BACKOFF: Duration = Duration::from_secs(300);
pub const MAX_RUN_BACKOFF: Duration = Duration::from_secs(3600);

/// How long a verification waits for one of the two macOS places before it
/// gives up for this pass, so a busy host never holds the loop up.
pub const PLACE_WAIT: Duration = Duration::from_secs(600);

/// How long the guest's checks may take, from `verify` to `verified`. The
/// launcher's own deadline is shorter (`launcher/Sources/Agent.swift`).
pub const CHECK_LIMIT: Duration = Duration::from_secs(420);

/// The largest manifest read (the import's own bound on an optional member).
const MAX_MANIFEST: u64 = 1 << 20;

/// The largest record read: a bound on a file `_faber` wrote itself.
const MAX_RECORD: u64 = 1 << 20;

// ---------------------------------------------------------------------------
// What a verification can find
// ---------------------------------------------------------------------------

/// One reason an image failed: a guest check, the wall, or faber's own run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Failure {
    /// A check the guest agent ran, by its fixed code.
    Guest(Check),
    /// The wall did not stop the guest's probe.
    Wall,
    /// The verification could not finish: the VM never reached its checks, or
    /// the supervisor broke off. Nothing is known about the image.
    Run,
}

impl Failure {
    /// The code `/api/state` shows after `failed:`.
    pub fn code(self) -> &'static str {
        match self {
            Failure::Guest(c) => c.as_str(),
            Failure::Wall => "wall",
            Failure::Run => "run",
        }
    }

    fn from_code(s: &str) -> Option<Self> {
        match s {
            "wall" => Some(Failure::Wall),
            "run" => Some(Failure::Run),
            _ => Check::ALL
                .into_iter()
                .find(|c| c.as_str() == s)
                .map(Failure::Guest),
        }
    }

    /// What is wrong, and what to fix, in a sentence for the operator.
    pub fn sentence(self) -> &'static str {
        match self {
            Failure::Guest(Check::ConsoleUser) => {
                "the console user is not `runner`: the image must log `runner` in automatically \
                 at boot (auto-login through /etc/kcpassword)"
            }
            Failure::Guest(Check::RunnerNotAdmin) => {
                "`runner` is an admin or has sudo rights, so job code could become root: remove it \
                 from the admin group and from every sudoers grant"
            }
            Failure::Guest(Check::AdminsLocked) => {
                "an admin account can log in: every admin needs the shell /usr/bin/false (not \
                 /sbin/nologin, which a GUI login passes) and root no authentication authority or a \
                 disabled one (;DisabledUser;); the build locks the Setup Assistant admin and root \
                 that way"
            }
            Failure::Guest(Check::RemoteLogin) => "Remote Login (SSH) is on in the image: turn it off",
            Failure::Guest(Check::Filevault) => {
                "FileVault is on in the image: leave it off in Setup Assistant"
            }
            Failure::Guest(Check::AppleId) => {
                "an Apple ID is signed in, and every clone would carry it: sign it out, or leave \
                 it out in Setup Assistant"
            }
            Failure::Guest(Check::SoftwareUpdate) => {
                "an automatic Software Update setting is on: the build turns every one off in \
                 /Library/Preferences/com.apple.SoftwareUpdate"
            }
            Failure::Guest(Check::Xcode) => {
                "as `runner`, `xcodebuild -version` does not match the image's manifest, or \
                 `xcode-select -p` does not resolve, or `cc` or `swiftc` cannot link a program: \
                 jobs would find no developer tools; rebuild the image"
            }
            Failure::Guest(Check::Runtimes) => {
                "a simulator runtime the manifest names is missing or not ready"
            }
            Failure::Guest(Check::BrewOwner) => "`runner` does not own the Homebrew prefix",
            Failure::Wall => {
                "the wall did not stop the guest's probe: the pf anchor is not holding; run \
                 `sudo faber wall check` and `sudo faber setup`"
            }
            Failure::Run => "the verification could not finish",
        }
    }
}

/// Why an image is not (or no longer) verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Why {
    /// No verification has run for this image on this host.
    NeverVerified,
    /// A file in the image changed since it was verified.
    ImageChanged,
    /// The host's macOS was updated since: ADR-0026's fresh-identifier trick must
    /// be tested again.
    HostChanged { was: String, now: String },
    /// The image, its manifest or the host's build could not be read.
    Unreadable(String),
}

/// How one golden image stands (ADR-0049 §7). Only [`Standing::Verified`] admits.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Standing {
    /// Not judged yet: the daemon has just started.
    #[default]
    Unknown,
    Unverified(Why),
    /// A verification is running, or waiting for a macOS place.
    Verifying,
    Verified,
    /// The last verification of this exact image on this host failed. `failed`
    /// is never empty; `why` is faber's own words for a [`Failure::Run`].
    Failed {
        failed: Vec<Failure>,
        why: Option<String>,
    },
}

impl Standing {
    pub fn admits(&self) -> bool {
        matches!(self, Standing::Verified)
    }

    /// The short form `/api/state` shows: `verified`, `unverified`, `verifying`,
    /// `unknown` or `failed:<check>`.
    pub fn summary(&self) -> Summary {
        match self {
            Standing::Unknown => Summary::Unknown,
            Standing::Unverified(_) => Summary::Unverified,
            Standing::Verifying => Summary::Verifying,
            Standing::Verified => Summary::Verified,
            Standing::Failed { failed, .. } => {
                Summary::Failed(failed.first().copied().unwrap_or(Failure::Run))
            }
        }
    }

    /// What is wrong with `image` and what to do, `None` when it is verified.
    pub fn detail(&self, image: &str) -> Option<String> {
        let rerun = format!("`faber golden verify {image}`");
        Some(match self {
            Standing::Verified => return None,
            Standing::Unknown => format!("the golden image {image} has not been judged yet"),
            Standing::Verifying => format!(
                "the golden image {image} is being verified: macOS jobs wait for its result"
            ),
            Standing::Unverified(Why::NeverVerified) => format!(
                "the golden image {image} has not been verified on this host; faber verifies it \
                 before it runs a job ({rerun})"
            ),
            Standing::Unverified(Why::ImageChanged) => format!(
                "the golden image {image} changed since it was verified; faber verifies it again \
                 ({rerun})"
            ),
            Standing::Unverified(Why::HostChanged { was, now }) => format!(
                "macOS on this host was updated ({was} to {now}) since the golden image {image} \
                 was verified; faber verifies it again ({rerun})"
            ),
            Standing::Unverified(Why::Unreadable(why)) => {
                format!("the golden image {image} cannot be judged: {why}")
            }
            Standing::Failed { failed, why } => {
                let first = failed.first().copied().unwrap_or(Failure::Run);
                let mut s = format!(
                    "the golden image {image} failed verification ({}): {}",
                    first.code(),
                    first.sentence()
                );
                if let (Failure::Run, Some(why)) = (first, why) {
                    s.push_str(&format!(" ({why})"));
                }
                let more: Vec<&str> = failed.iter().skip(1).map(|f| f.code()).collect();
                if !more.is_empty() {
                    s.push_str(&format!("; also failed: {}", more.join(", ")));
                }
                if failed.iter().any(|f| matches!(f, Failure::Guest(_))) {
                    s.push_str(&format!(
                        ". What each check found (which account, key or value): `{}`",
                        details_command(image)
                    ));
                }
                if first == Failure::Run || first == Failure::Wall {
                    s.push_str(&format!(". Then run {rerun}"));
                } else {
                    s.push_str(&format!(
                        ". Rebuild the image (`faber golden prepare --from {image}`, then `sudo \
                         faber golden build`), then run {rerun}"
                    ));
                }
                s
            }
        })
    }
}

/// Where the macOS VM supervisor keeps the guest console of `image`'s latest
/// verification: the agent's `FABER-GOLDEN-VERIFY` line per check, naming the
/// account, key or value that decided it (docs/vm-agent-protocol.md, *verify*).
/// It is in the session account's state directory (ADR-0051), `_faber_mac`'s
/// alone (0700, the file 0600), so only root reads it besides that account;
/// `_faber` never does, and faber never parses it. A host without the session
/// account keeps it in `_faber_vm`'s state directory instead. Pure.
pub fn verify_log_path(image: &str) -> PathBuf {
    Path::new(crate::platform::layout::FABER_MAC_HOME)
        .join("state")
        .join(format!("verify-{image}-latest.log"))
}

/// The operator's command to read [`verify_log_path`]: `cat -v`, so no byte of
/// the guest's reaches the terminal as a control sequence. Display only. Pure.
pub fn details_command(image: &str) -> String {
    format!("sudo cat -v {}", verify_log_path(image).display())
}

/// [`Standing::summary`]: what `vm.images.macos` and each golden row show.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Summary {
    #[default]
    Unknown,
    Verified,
    Unverified,
    Verifying,
    Failed(Failure),
}

impl Summary {
    pub fn text(self) -> String {
        match self {
            Summary::Unknown => "unknown".into(),
            Summary::Verified => "verified".into(),
            Summary::Unverified => "unverified".into(),
            Summary::Verifying => "verifying".into(),
            Summary::Failed(f) => format!("failed:{}", f.code()),
        }
    }

    fn rank(self) -> u8 {
        match self {
            Summary::Verified => 0,
            Summary::Unknown => 1,
            Summary::Verifying => 2,
            Summary::Unverified => 3,
            Summary::Failed(_) => 4,
        }
    }

    /// The images' standing as one: the worst of them, `unknown` for none.
    pub fn worst(all: impl IntoIterator<Item = Summary>) -> Summary {
        all.into_iter()
            .max_by_key(|s| s.rank())
            .unwrap_or(Summary::Unknown)
    }
}

impl Serialize for Summary {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.text())
    }
}

// ---------------------------------------------------------------------------
// The record
// ---------------------------------------------------------------------------

/// What a result is keyed by: every file of the image and its SHA-256, and the
/// host's macOS build.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImageKey {
    pub files: BTreeMap<String, String>,
    pub macos_build: String,
}

/// One image's last result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub key: ImageKey,
    /// The failed codes ([`Failure::code`]); empty is a pass.
    pub failed: Vec<String>,
    /// faber's own words for a run that could not finish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    pub at_ms: u64,
    /// Failed runs in a row for this key: the back-off's exponent.
    #[serde(default)]
    pub failures: u32,
}

impl Entry {
    pub fn passed(&self) -> bool {
        self.failed.is_empty()
    }
}

/// One file's identity, and the SHA-256 it had then.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fingerprint {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
    pub sha256: String,
}

/// `image -> file -> fingerprint`.
pub type HashCache = BTreeMap<String, BTreeMap<String, Fingerprint>>;

/// `golden-verify.json`.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub schema: u32,
    #[serde(default)]
    pub images: BTreeMap<String, Entry>,
    #[serde(default)]
    pub hashes: HashCache,
}

impl Record {
    /// Read `path`, or an empty record when it is missing, unreadable, of another
    /// schema or not this shape: every image is then verified again, which fails
    /// safe. An older schema's hashes are kept ([`HASHES_STILL_HOLD`]), never its
    /// results.
    pub fn load(path: &Path) -> Record {
        let empty = Record {
            schema: SCHEMA,
            ..Default::default()
        };
        let Ok(meta) = std::fs::metadata(path) else {
            return empty;
        };
        if meta.len() > MAX_RECORD {
            return empty;
        }
        let Ok(bytes) = std::fs::read(path) else {
            return empty;
        };
        match serde_json::from_slice::<Record>(&bytes) {
            Ok(r) if r.schema == SCHEMA => r,
            Ok(r) if HASHES_STILL_HOLD.contains(&r.schema) => Record {
                hashes: r.hashes,
                ..empty
            },
            _ => empty,
        }
    }

    /// Write `path` atomically: a temporary file beside it, then a rename.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let bytes = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, bytes)?;
        std::fs::rename(&tmp, path)
    }

    /// Record a result for `image` under `key`. A failure under the same key as
    /// the last failure counts one more in a row; anything else starts over.
    pub fn put(&mut self, image: &str, key: ImageKey, outcome: &Outcome, at_ms: u64) {
        let (failed, why) = match outcome {
            Outcome::Passed => (Vec::new(), None),
            Outcome::Failed { failed, why } => (
                failed.iter().map(|f| f.code().to_string()).collect(),
                why.clone(),
            ),
        };
        let failures = match (self.images.get(image), failed.is_empty()) {
            (_, true) => 0,
            // In a row: the same key, and the same kind (a run that could not finish,
            // or a failed check), since each kind backs off on its own scale.
            (Some(e), false)
                if e.key == key
                    && !e.passed()
                    && e.failed.iter().all(|f| f == Failure::Run.code())
                        == failed.iter().all(|f| f == Failure::Run.code()) =>
            {
                e.failures.saturating_add(1)
            }
            (_, false) => 1,
        };
        self.images.insert(
            image.to_string(),
            Entry {
                key,
                failed,
                why,
                at_ms,
                failures,
            },
        );
    }
}

/// A finished verification's result, as recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Passed,
    Failed {
        failed: Vec<Failure>,
        why: Option<String>,
    },
}

impl Outcome {
    /// From the checks the guest failed: none is a pass.
    pub fn of_checks(failed: &[Check]) -> Self {
        if failed.is_empty() {
            Outcome::Passed
        } else {
            Outcome::Failed {
                failed: failed.iter().copied().map(Failure::Guest).collect(),
                why: None,
            }
        }
    }
}

/// How `image` stands under `key`, by the record. Pure.
pub fn judge(record: &Record, image: &str, key: &ImageKey) -> Standing {
    let Some(entry) = record.images.get(image) else {
        return Standing::Unverified(Why::NeverVerified);
    };
    if entry.key.files != key.files {
        return Standing::Unverified(Why::ImageChanged);
    }
    if entry.key.macos_build != key.macos_build {
        return Standing::Unverified(Why::HostChanged {
            was: entry.key.macos_build.clone(),
            now: key.macos_build.clone(),
        });
    }
    if entry.passed() {
        return Standing::Verified;
    }
    let mut failed: Vec<Failure> = entry
        .failed
        .iter()
        .map(|c| Failure::from_code(c).unwrap_or(Failure::Run))
        .collect();
    failed.sort();
    failed.dedup();
    Standing::Failed {
        failed,
        why: entry.why.clone(),
    }
}

/// The back-off after `failures` failed runs in a row.
pub fn backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    FIRST_BACKOFF
        .saturating_mul(1u32 << doublings)
        .min(MAX_BACKOFF)
}

/// The back-off after `failures` runs in a row that could not finish.
pub fn run_backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(16);
    FIRST_RUN_BACKOFF
        .saturating_mul(1u32 << doublings)
        .min(MAX_RUN_BACKOFF)
}

impl Entry {
    /// When this result's key is due again: a pass never; a failed check after
    /// [`backoff`]; a run that could not finish after [`run_backoff`].
    fn retry_after(&self) -> Option<Duration> {
        if self.passed() {
            None
        } else if self.failed.iter().all(|f| f == Failure::Run.code()) {
            Some(run_backoff(self.failures))
        } else {
            Some(backoff(self.failures))
        }
    }
}

/// Whether the daemon should verify `image` on its own at `now_ms`. A key no
/// result is recorded for is due at once, once; a failure under the current key
/// is due again only after [`backoff`]; a pass is never due. Pure.
pub fn due(record: &Record, image: &str, key: &ImageKey, now_ms: u64) -> bool {
    match record.images.get(image) {
        Some(e) if e.key == *key => e
            .retry_after()
            .is_some_and(|wait| now_ms >= e.at_ms.saturating_add(wait.as_millis() as u64)),
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// The image, its manifest, the host
// ---------------------------------------------------------------------------

/// A file's identity, without its hash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileId {
    pub dev: u64,
    pub ino: u64,
    pub size: u64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
}

impl FileId {
    fn of(meta: &std::fs::Metadata) -> Self {
        use std::os::unix::fs::MetadataExt;
        let ns = |s: i64, n: i64| i128::from(s) * 1_000_000_000 + i128::from(n);
        FileId {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime_ns: ns(meta.mtime(), meta.mtime_nsec()),
            ctime_ns: ns(meta.ctime(), meta.ctime_nsec()),
        }
    }

    fn matches(&self, f: &Fingerprint) -> bool {
        self.dev == f.dev
            && self.ino == f.ino
            && self.size == f.size
            && self.mtime_ns == f.mtime_ns
            && self.ctime_ns == f.ctime_ns
    }
}

/// What admission checks against a verified key, cheaply, every time (no hash):
/// each file's identity and the host's macOS build. A file that changes gets a
/// new ctime at least, so an image rebuilt or touched since its verification is
/// unverified at once, not at the next pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub files: BTreeMap<String, FileId>,
    pub macos_build: String,
}

impl Identity {
    /// The identity a pass hashed under: the cache's fingerprints for `files`.
    pub fn of_cache(
        cache: &BTreeMap<String, Fingerprint>,
        files: &BTreeMap<String, String>,
        macos_build: &str,
    ) -> Option<Identity> {
        let ids = files
            .keys()
            .map(|f| {
                let p = cache.get(f)?;
                Some((
                    f.clone(),
                    FileId {
                        dev: p.dev,
                        ino: p.ino,
                        size: p.size,
                        mtime_ns: p.mtime_ns,
                        ctime_ns: p.ctime_ns,
                    },
                ))
            })
            .collect::<Option<BTreeMap<_, _>>>()?;
        Some(Identity {
            files: ids,
            macos_build: macos_build.to_string(),
        })
    }

    /// The image at `dir` and the host, now: a stat of each file and one small read.
    pub fn now(dir: &Path) -> Result<Identity, String> {
        let mut files = BTreeMap::new();
        for name in bundle_files(dir)? {
            let meta =
                std::fs::symlink_metadata(dir.join(&name)).map_err(|e| format!("{name}: {e}"))?;
            files.insert(name, FileId::of(&meta));
        }
        Ok(Identity {
            files,
            macos_build: host_build_now()?,
        })
    }

    /// Why `now` is not the identity verified as `self`, if it is not.
    pub fn drift(&self, now: &Identity) -> Option<Why> {
        if self.macos_build != now.macos_build {
            Some(Why::HostChanged {
                was: self.macos_build.clone(),
                now: now.macos_build.clone(),
            })
        } else if self.files != now.files {
            Some(Why::ImageChanged)
        } else {
            None
        }
    }
}

/// The files of a golden bundle a key covers: the required four and the
/// optional manifest, as the import hashes them.
fn bundle_files(dir: &Path) -> Result<Vec<String>, String> {
    use crate::setup::vm_images::{GOLDEN_FILES, GOLDEN_OPTIONAL};
    let mut out: Vec<String> = GOLDEN_FILES.iter().map(|s| s.to_string()).collect();
    for f in GOLDEN_OPTIONAL {
        if std::fs::symlink_metadata(dir.join(f)).is_ok() {
            out.push(f.to_string());
        }
    }
    for f in GOLDEN_FILES {
        if std::fs::symlink_metadata(dir.join(f)).is_err() {
            return Err(format!("{} has no {f}", dir.display()));
        }
    }
    Ok(out)
}

/// Every file of the image at `dir` and its SHA-256, reusing `cache` for each file
/// whose identity is unchanged and updating it for the rest. Blocking: it may
/// read 40 GB.
pub fn hash_image(
    dir: &Path,
    cache: &mut BTreeMap<String, Fingerprint>,
) -> Result<BTreeMap<String, String>, String> {
    let mut files = BTreeMap::new();
    let names = bundle_files(dir)?;
    cache.retain(|k, _| names.contains(k));
    for name in names {
        let path = dir.join(&name);
        let meta = std::fs::symlink_metadata(&path).map_err(|e| format!("{name}: {e}"))?;
        if !meta.file_type().is_file() {
            return Err(format!("{name} is not a regular file"));
        }
        let id = FileId::of(&meta);
        let sha = match cache.get(&name) {
            Some(f) if id.matches(f) => f.sha256.clone(),
            _ => {
                let mut file = std::fs::File::open(&path).map_err(|e| format!("{name}: {e}"))?;
                let sha = crate::setup::fsx::sha256_of(&mut file)
                    .map_err(|e| format!("reading {name}: {e}"))?;
                // Hashed from a descriptor opened after the stat: a file replaced
                // in between gets a new identity on the next pass and is hashed
                // again.
                cache.insert(
                    name.clone(),
                    Fingerprint {
                        dev: id.dev,
                        ino: id.ino,
                        size: id.size,
                        mtime_ns: id.mtime_ns,
                        ctime_ns: id.ctime_ns,
                        sha256: sha.clone(),
                    },
                );
                sha
            }
        };
        files.insert(name, sha);
    }
    Ok(files)
}

/// `installed` as the manifest records it (`setup::golden_build::manifest_json`).
#[derive(Deserialize)]
struct ManifestInstalled {
    xcode_version: String,
    xcode_build: String,
    #[serde(default)]
    runtimes: Vec<ManifestRuntime>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestRuntime {
    platform: String,
    version: String,
    build: String,
}

#[derive(Deserialize)]
struct Manifest {
    schema: u32,
    installed: ManifestInstalled,
}

/// What a verification of the image with `manifest` holds the guest to, from
/// the manifest's installed half. `None` is a bundle without a manifest: no
/// particular Xcode and no runtimes. Every value is held to the wire's grammar
/// here, so a manifest that does not fit is refused, not sent.
pub fn expect_from_manifest(manifest: Option<&[u8]>) -> Result<Expect, String> {
    let Some(bytes) = manifest else {
        return Ok(Expect {
            xcode: None,
            runtimes: Vec::new(),
        });
    };
    let m: Manifest =
        serde_json::from_slice(bytes).map_err(|_| "its manifest.json is not one faber reads")?;
    if m.schema != 1 {
        return Err(format!("its manifest.json is schema {}, not 1", m.schema));
    }
    let bad = |what: &str| format!("its manifest.json records an {what} faber cannot check");
    let build = |s: &str| {
        BuildToken::try_from(s)
            .ok()
            .filter(|b| b.as_str().len() >= 3)
    };
    let xcode = XcodeExpect {
        version: XcodeVersion::try_from(m.installed.xcode_version.as_str())
            .ok()
            .filter(|v| !v.as_str().contains("  ") && !v.as_str().ends_with(' '))
            .ok_or_else(|| bad("Xcode version"))?,
        build: build(&m.installed.xcode_build).ok_or_else(|| bad("Xcode build"))?,
    };
    if m.installed.runtimes.len() > MAX_RUNTIMES {
        return Err(format!(
            "its manifest.json names more than {MAX_RUNTIMES} runtimes"
        ));
    }
    let runtimes = m
        .installed
        .runtimes
        .iter()
        .map(|r| {
            Some(RuntimeExpect {
                platform: RuntimePlatform::try_from(r.platform.as_str()).ok()?,
                version: RuntimeVersion::try_from(r.version.as_str()).ok()?,
                build: build(&r.build)?,
            })
        })
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| bad("runtime"))?;
    Ok(Expect {
        xcode: Some(xcode),
        runtimes,
    })
}

/// Read `dir/manifest.json`, bounded, or `None` when the bundle has none.
pub fn read_manifest(dir: &Path) -> Result<Option<Vec<u8>>, String> {
    use std::io::Read;
    let path = dir.join("manifest.json");
    let file = match std::fs::File::open(&path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("manifest.json: {e}")),
    };
    let mut bytes = Vec::new();
    file.take(MAX_MANIFEST + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("manifest.json: {e}"))?;
    if bytes.len() as u64 > MAX_MANIFEST {
        return Err("manifest.json is larger than the import takes".into());
    }
    Ok(Some(bytes))
}

/// Whether `s` reads as a macOS build (`26A434`): 3 to 12 ASCII letters and digits.
pub fn is_macos_build(s: &str) -> bool {
    (3..=12).contains(&s.len()) && s.bytes().all(|c| c.is_ascii_alphanumeric())
}

/// Where macOS records its own build: readable by every account, and read on
/// every macOS admission, so it is a file and not a command.
pub const SYSTEM_VERSION_PLIST: &str = "/System/Library/CoreServices/SystemVersion.plist";

/// The copy a Rapid Security Response updates: the sealed system volume's copy
/// above keeps the base build, so it alone would miss an RSR and never ask for
/// a re-verify. Read first; absent on hosts without the Cryptex.
pub const CRYPTEX_SYSTEM_VERSION_PLIST: &str =
    "/System/Volumes/Preboot/Cryptexes/OS/System/Library/CoreServices/SystemVersion.plist";

/// `ProductBuildVersion` from `SystemVersion.plist`'s XML. Pure.
pub fn build_from_plist(text: &str) -> Option<String> {
    let after = text.split("<key>ProductBuildVersion</key>").nth(1)?;
    let value = after.trim_start().strip_prefix("<string>")?;
    let build = value.split("</string>").next()?.trim();
    is_macos_build(build).then(|| build.to_string())
}

/// The host's macOS build, now: the Cryptex copy when there is one, so a Rapid
/// Security Response counts as a new build.
pub fn host_build_now() -> Result<String, String> {
    let path = pick_version_plist(std::path::Path::new(CRYPTEX_SYSTEM_VERSION_PLIST).exists());
    let text = std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"))?;
    build_from_plist(&text).ok_or_else(|| format!("{path} names no macOS build"))
}

/// Which `SystemVersion.plist` names the running build. Pure.
fn pick_version_plist(cryptex_exists: bool) -> &'static str {
    if cryptex_exists {
        CRYPTEX_SYSTEM_VERSION_PLIST
    } else {
        SYSTEM_VERSION_PLIST
    }
}

/// [`host_build_now`], for the loop.
pub async fn host_macos_build() -> Result<String, String> {
    host_build_now()
}

/// The claim a verification runs under: job id 0, which no GitHub job has, so
/// the claim is this instance's ([`super::adopt::ours`]) and never a job's.
pub fn claim_for(instance: &str, now_ms: u64) -> Option<ClaimId> {
    ClaimId::try_from(format!("0-{now_ms}-{instance}")).ok()
}

// ---------------------------------------------------------------------------
// Asking the daemon
// ---------------------------------------------------------------------------

/// Why a request to verify now was not taken.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RequestError {
    #[error("this daemon verifies no golden image: no `macos-vm` profile is configured")]
    NotConfigured,
    #[error("no `macos-vm` profile names the image {0}")]
    UnknownImage(String),
}

/// The daemon's verifier, as the control API reaches it.
pub struct Handle {
    images: Vec<String>,
    tx: mpsc::UnboundedSender<String>,
    /// Images a verification is running for, or queued.
    pending: Mutex<std::collections::BTreeSet<String>>,
}

impl Handle {
    pub fn new(images: Vec<String>) -> (Arc<Self>, mpsc::UnboundedReceiver<String>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Arc::new(Handle {
                images,
                tx,
                pending: Mutex::default(),
            }),
            rx,
        )
    }

    /// Queue a verification of `image` now. `Ok(false)` when one is already
    /// queued or running: the caller waits for that one.
    pub fn request(&self, image: &str) -> Result<bool, RequestError> {
        if !self.images.iter().any(|i| i == image) {
            return Err(RequestError::UnknownImage(image.to_string()));
        }
        if !self.pending.lock().unwrap().insert(image.to_string()) {
            return Ok(false);
        }
        let _ = self.tx.send(image.to_string());
        Ok(true)
    }

    /// Mark `image` as being verified (by the daemon's own schedule too), so a
    /// request meanwhile waits for it. `false` when it already was.
    pub fn take(&self, image: &str) -> bool {
        self.pending.lock().unwrap().insert(image.to_string())
    }

    /// `image`'s verification is over.
    pub fn done(&self, image: &str) {
        self.pending.lock().unwrap().remove(image);
    }
}

static HANDLE: OnceLock<Arc<Handle>> = OnceLock::new();

/// Install the daemon's verifier (once, at boot).
pub fn install(handle: Arc<Handle>) {
    let _ = HANDLE.set(handle);
}

/// Ask the daemon's verifier to verify `image` now.
pub fn request(image: &str) -> Result<bool, RequestError> {
    HANDLE
        .get()
        .ok_or(RequestError::NotConfigured)?
        .request(image)
}

/// Where the record lives: `_faber`'s state directory.
pub fn record_path() -> Option<PathBuf> {
    crate::platform::state_dir().map(|d| d.join(RECORD_FILE))
}

// ---------------------------------------------------------------------------
// `faber golden verify <name>`
// ---------------------------------------------------------------------------

/// How long the CLI waits for a verification it asked for: one waits for a
/// macOS place, then boots a VM to its desktop and runs the checks.
pub const CLI_WAIT: Duration = Duration::from_secs(3600);

/// How often the CLI re-reads `/api/state`.
const CLI_POLL: Duration = Duration::from_secs(5);

/// One golden row of `/api/state` (`budget.vm.golden`), as the CLI reads it:
/// `(standing, detail)`. Pure.
pub fn golden_row(state: &serde_json::Value, image: &str) -> Option<(String, Option<String>)> {
    let rows = state.get("budget")?.get("vm")?.get("golden")?.as_array()?;
    let row = rows
        .iter()
        .find(|r| r.get("image").and_then(|i| i.as_str()) == Some(image))?;
    let standing = row.get("standing")?.as_str()?.to_string();
    let detail = row
        .get("detail")
        .and_then(|d| d.as_str())
        .map(str::to_string);
    Some((standing, detail))
}

/// Where the CLI's wait is, from the rows it has seen so far.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliWait {
    /// The daemon has not started this verification yet.
    Queued,
    /// It is running.
    Running,
    /// It ended, in this standing.
    Ended(String, Option<String>),
}

/// The next [`CliWait`] given the last one and a fresh row. Only a standing
/// seen after `verifying` is the result, so an earlier verdict is never taken
/// for this one. Pure.
pub fn cli_step(last: &CliWait, row: Option<(String, Option<String>)>) -> CliWait {
    match (last, row) {
        (_, Some((s, _))) if s == "verifying" => CliWait::Running,
        (CliWait::Running, Some((s, d))) => CliWait::Ended(s, d),
        (CliWait::Running, None) => CliWait::Ended("unknown".into(), None),
        (other, _) => other.clone(),
    }
}

/// `faber golden verify <image>`: ask the running daemon to verify `image` now,
/// over its control socket, and wait for the result. No `sudo`: verifying is
/// a routine request (ADR-0028), and it only ever holds admission back.
pub async fn cli(image: &str) -> anyhow::Result<()> {
    use crate::control::{self, Fallback};
    let socket = crate::platform::control_socket()
        .ok_or_else(|| anyhow::anyhow!("no HOME to find the daemon's control socket under"))?;
    let ask = |method: reqwest::Method, path: &'static str, body: Option<serde_json::Value>| {
        let socket = socket.clone();
        async move {
            control::request(&socket, Fallback::Never, method, path, body)
                .await
                .map_err(|e| anyhow::anyhow!("{}", crate::drain::client_error_message(&e)))
        }
    };
    let (status, said) = ask(
        reqwest::Method::POST,
        "/api/golden/verify",
        Some(serde_json::json!({ "image": image })),
    )
    .await?;
    let said = crate::term::sanitize_bounded(said.trim(), 512).into_owned();
    if !status.is_success() {
        anyhow::bail!("the daemon refused: {said} ({status})");
    }
    println!("{said}; waiting for the result (a VM boots, which takes a few minutes)…");
    let deadline = std::time::Instant::now() + CLI_WAIT;
    let mut wait = CliWait::Queued;
    loop {
        let (status, text) = ask(reqwest::Method::GET, "/api/state", None).await?;
        let row = status
            .is_success()
            .then(|| serde_json::from_str::<serde_json::Value>(&text).ok())
            .flatten()
            .and_then(|v| golden_row(&v, image));
        let next = cli_step(&wait, row);
        if next == CliWait::Running && wait == CliWait::Queued {
            println!("verifying {image}…");
        }
        wait = next;
        if let CliWait::Ended(standing, detail) = &wait {
            let detail = detail
                .as_deref()
                .map(|d| crate::term::sanitize_bounded(d, 2048).into_owned());
            if standing == "verified" {
                println!("{image} passed: macOS jobs may run on it");
                return Ok(());
            }
            anyhow::bail!("{image}: {}", detail.unwrap_or_else(|| standing.clone()));
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "no result after {:?}; the daemon goes on verifying, and `faber status` shows it",
                CLI_WAIT
            );
        }
        tokio::time::sleep(CLI_POLL).await;
    }
}

// ---------------------------------------------------------------------------
// The daemon's loop
// ---------------------------------------------------------------------------

/// What the loop needs from the host, so a test drives it with fakes: the key
/// and expectations of an image, and one verification run.
pub trait Host: Send + Sync {
    /// The host's macOS build.
    fn macos_build(&self) -> impl std::future::Future<Output = Result<String, String>> + Send;
    /// `image`'s files and their SHA-256, reusing and updating `cache`.
    fn hash(
        &self,
        image: &str,
        cache: BTreeMap<String, Fingerprint>,
    ) -> impl std::future::Future<
        Output = (
            Result<BTreeMap<String, String>, String>,
            BTreeMap<String, Fingerprint>,
        ),
    > + Send;
    /// What a verification of `image` expects, from its manifest.
    fn expect(&self, image: &str) -> Result<Expect, String>;
    /// Where admission re-checks `image`'s identity ([`Identity::now`]); `None`
    /// pins nothing (a fake host, whose image is not on disk).
    fn dir(&self, image: &str) -> Option<PathBuf> {
        let _ = image;
        None
    }
    /// Run one verification of `image` (admission, tools disk, VM), to its end.
    fn run(
        &self,
        image: &str,
        expect: Expect,
    ) -> impl std::future::Future<Output = super::job::VerifyEnd> + Send;
}

/// The outcome a run's end is recorded as; `None` for a run that was cancelled,
/// which teaches nothing.
pub fn outcome_of(end: super::job::VerifyEnd) -> Option<Outcome> {
    use super::job::VerifyEnd;
    match end {
        VerifyEnd::Checked(failed) => Some(Outcome::of_checks(&failed)),
        VerifyEnd::WallBreached => Some(Outcome::Failed {
            failed: vec![Failure::Wall],
            why: None,
        }),
        VerifyEnd::Failed(e) => Some(Outcome::Failed {
            failed: vec![Failure::Run],
            why: Some(format!("{e:#}")),
        }),
        VerifyEnd::Cancelled => None,
    }
}

/// The verifier's state between passes.
pub struct Verifier<H> {
    pub host: H,
    /// Runs that recorded nothing (cancelled, or no macOS place in time), by
    /// image: the key, when, and how many in a row, so they back off too.
    pub untold: BTreeMap<String, (ImageKey, u64, u32)>,
    pub handle: Arc<Handle>,
    pub limiter: Arc<crate::limiter::Limiter>,
    pub images: Vec<String>,
    pub record: Record,
    /// Where the record is saved; `None` keeps it in memory (tests, or a
    /// process without a state directory).
    pub path: Option<PathBuf>,
}

impl<H: Host> Verifier<H> {
    fn save(&self) {
        if let Some(path) = &self.path {
            if let Err(e) = self.record.save(path) {
                tracing::warn!(
                    "golden verify: cannot save {}: {e} — the result holds until the daemon \
                     restarts",
                    path.display()
                );
            }
        }
    }

    /// `image`'s key now, or why it cannot be had.
    async fn key(
        &mut self,
        image: &str,
        build: &Result<String, String>,
    ) -> Result<ImageKey, String> {
        let cache = self.record.hashes.remove(image).unwrap_or_default();
        let (files, cache) = self.host.hash(image, cache).await;
        self.record.hashes.insert(image.to_string(), cache);
        let files = files?;
        let macos_build = build.clone()?;
        Ok(ImageKey { files, macos_build })
    }

    /// One pass: judge every image, publish it, and verify each that is due or
    /// `asked` for. Returns the images verified.
    pub async fn pass(&mut self, asked: Option<&str>) -> Vec<String> {
        let build = self.host.macos_build().await;
        let mut ran = Vec::new();
        for image in self.images.clone() {
            let is_asked = asked == Some(image.as_str());
            let key = match self.key(&image, &build).await {
                Ok(k) => k,
                Err(why) => {
                    if is_asked {
                        self.handle.done(&image);
                    }
                    self.limiter
                        .observe_golden(&image, Standing::Unverified(Why::Unreadable(why)));
                    continue;
                }
            };
            let standing = judge(&self.record, &image, &key);
            let now = now_ms();
            let held_off = self.untold.get(&image).is_some_and(|(k, at, n)| {
                *k == key && now < at.saturating_add(run_backoff(*n).as_millis() as u64)
            });
            let wanted = is_asked
                || (!held_off && due(&self.record, &image, &key, now) && self.handle.take(&image));
            if !wanted {
                let pin = if standing.admits() {
                    self.host.dir(&image).and_then(|dir| {
                        let cache = self.record.hashes.get(&image)?;
                        Some((
                            dir,
                            Identity::of_cache(cache, &key.files, &key.macos_build)?,
                        ))
                    })
                } else {
                    None
                };
                self.limiter.observe_golden(&image, standing);
                if let Some((dir, identity)) = pin {
                    self.limiter.pin_golden(&image, dir, identity);
                }
                continue;
            }
            let expect = match self.host.expect(&image) {
                Ok(e) => e,
                Err(why) => {
                    self.handle.done(&image);
                    self.limiter
                        .observe_golden(&image, Standing::Unverified(Why::Unreadable(why)));
                    continue;
                }
            };
            tracing::info!("golden verify: verifying {image}");
            self.limiter.observe_golden(&image, Standing::Verifying);
            let end = self.host.run(&image, expect).await;
            match outcome_of(end) {
                Some(outcome) => {
                    self.untold.remove(&image);
                    self.record.put(&image, key.clone(), &outcome, now_ms());
                    self.save();
                }
                None => {
                    let n = match self.untold.get(&image) {
                        Some((k, _, n)) if *k == key => n.saturating_add(1),
                        _ => 1,
                    };
                    self.untold
                        .insert(image.clone(), (key.clone(), now_ms(), n));
                }
            }
            self.handle.done(&image);
            let after = judge(&self.record, &image, &key);
            if after.admits() {
                tracing::info!("golden verify: {image} passed");
            }
            let pin = if after.admits() {
                self.host.dir(&image).and_then(|dir| {
                    let cache = self.record.hashes.get(&image)?;
                    Some((
                        dir,
                        Identity::of_cache(cache, &key.files, &key.macos_build)?,
                    ))
                })
            } else {
                None
            };
            self.limiter.observe_golden(&image, after);
            if let Some((dir, identity)) = pin {
                self.limiter.pin_golden(&image, dir, identity);
            }
            ran.push(image);
        }
        self.save();
        ran
    }

    /// Pass now, then whenever asked and every [`WATCH_INTERVAL`], forever.
    pub async fn watch(mut self, mut asks: mpsc::UnboundedReceiver<String>) {
        let mut asked: Option<String> = None;
        loop {
            self.pass(asked.as_deref()).await;
            asked = tokio::select! {
                a = asks.recv() => a,
                _ = tokio::time::sleep(WATCH_INTERVAL) => None,
            };
        }
    }
}

/// The real host: the image store, `sw_vers`, the session supervisor, the wall's
/// counters and the tools disk.
pub struct ThisHost {
    pub limiter: Arc<crate::limiter::Limiter>,
    pub gh: Arc<crate::github::Github>,
    pub instance: String,
    /// The first `macos-vm` profile's VM settings for each image.
    pub configs: BTreeMap<String, crate::backend::MacosVmConfig>,
}

impl Host for ThisHost {
    async fn macos_build(&self) -> Result<String, String> {
        host_macos_build().await
    }

    async fn hash(
        &self,
        image: &str,
        cache: BTreeMap<String, Fingerprint>,
    ) -> (
        Result<BTreeMap<String, String>, String>,
        BTreeMap<String, Fingerprint>,
    ) {
        let dir = crate::setup::vm_images::golden_dir().join(image);
        let fallback = cache.clone();
        tokio::task::spawn_blocking(move || {
            let mut cache = cache;
            let files = hash_image(&dir, &mut cache);
            (files, cache)
        })
        .await
        .unwrap_or_else(|_| (Err("hashing the image failed".into()), fallback))
    }

    fn expect(&self, image: &str) -> Result<Expect, String> {
        let dir = crate::setup::vm_images::golden_dir().join(image);
        expect_from_manifest(read_manifest(&dir)?.as_deref())
    }

    fn dir(&self, image: &str) -> Option<PathBuf> {
        Some(crate::setup::vm_images::golden_dir().join(image))
    }

    async fn run(&self, image: &str, expect: Expect) -> super::job::VerifyEnd {
        use super::job::{run_verify, VerifyEnd};
        let Some(cfg) = self.configs.get(image) else {
            return VerifyEnd::Failed(anyhow::anyhow!("no profile names {image}"));
        };
        // A place of the two, like a job, but not gated on the image itself.
        // Bounded, so a host whose places stay taken never holds the loop up: the
        // run records nothing and is tried again after a short back-off.
        let Ok(admission) =
            tokio::time::timeout(PLACE_WAIT, self.limiter.vm_admit(None, |_| {})).await
        else {
            tracing::info!("golden verify: no macOS place for {image} within {PLACE_WAIT:?}");
            return VerifyEnd::Cancelled;
        };
        let tools = match super::tools_disk::for_this_host(&self.gh, &cfg.runner_version).await {
            Ok(t) => t,
            Err(e) => return VerifyEnd::Failed(anyhow::anyhow!(e).context("the tools disk")),
        };
        let Some(claim) = claim_for(&self.instance, now_ms()) else {
            return VerifyEnd::Failed(anyhow::anyhow!("no claim id for this instance"));
        };
        let env = super::job::VmEnv {
            supervisor: super::job::ProductionSupervisor::for_macos_jobs(),
            wall: super::wall_proof::CounterWall::for_this_host(),
            mint: super::job::NoMint,
            jobs: super::job::global(),
        };
        let (_keep, cancel) = tokio::sync::watch::channel(false);
        run_verify(
            &env,
            admission,
            claim,
            cfg,
            &tools,
            expect,
            CHECK_LIMIT,
            cancel,
        )
        .await
    }
}

/// Start verifying this host's golden images and gate macOS jobs on them: one
/// loop for every image a `macos-vm` profile names. Called once at boot.
pub fn spawn_for_this_host(
    limiter: Arc<crate::limiter::Limiter>,
    gh: Arc<crate::github::Github>,
    instance: String,
    profiles: &[crate::backend::Profile],
) {
    let mut configs = BTreeMap::new();
    for p in profiles {
        if let crate::backend::Backend::MacosVm(cfg) = &p.backend {
            configs
                .entry(cfg.image.as_str().to_string())
                .or_insert_with(|| cfg.clone());
        }
    }
    if configs.is_empty() {
        return;
    }
    let images: Vec<String> = configs.keys().cloned().collect();
    let (handle, asks) = Handle::new(images.clone());
    install(handle.clone());
    limiter.require_verified_images();
    let path = record_path();
    let record = path.as_deref().map(Record::load).unwrap_or(Record {
        schema: SCHEMA,
        ..Default::default()
    });
    let verifier = Verifier {
        host: ThisHost {
            limiter: limiter.clone(),
            gh,
            instance,
            configs,
        },
        untold: BTreeMap::new(),
        handle,
        limiter,
        images,
        record,
        path,
    };
    tokio::spawn(verifier.watch(asks));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm::job::VerifyEnd;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn key(disk: &str, build: &str) -> ImageKey {
        ImageKey {
            files: [("disk".to_string(), disk.to_string())]
                .into_iter()
                .collect(),
            macos_build: build.into(),
        }
    }

    fn empty() -> Record {
        Record {
            schema: SCHEMA,
            ..Default::default()
        }
    }

    #[test]
    fn a_pass_is_keyed_by_the_files_and_the_hosts_build() {
        let mut r = empty();
        let k = key("aa", "26A434");
        assert_eq!(
            judge(&r, "img", &k),
            Standing::Unverified(Why::NeverVerified)
        );
        r.put("img", k.clone(), &Outcome::Passed, 1);
        assert_eq!(judge(&r, "img", &k), Standing::Verified);
        // A file changes: unverified, whatever the build.
        assert_eq!(
            judge(&r, "img", &key("bb", "26A434")),
            Standing::Unverified(Why::ImageChanged)
        );
        // The host updates: unverified, and it says from what to what.
        assert_eq!(
            judge(&r, "img", &key("aa", "26B12")),
            Standing::Unverified(Why::HostChanged {
                was: "26A434".into(),
                now: "26B12".into()
            })
        );
        // Another image's pass is not this one's.
        assert_eq!(
            judge(&r, "other", &k),
            Standing::Unverified(Why::NeverVerified)
        );
    }

    #[test]
    fn a_failure_is_recorded_with_its_checks_and_counted_in_a_row() {
        let mut r = empty();
        let k = key("aa", "26A434");
        let failed = Outcome::of_checks(&[Check::Filevault, Check::AppleId]);
        r.put("img", k.clone(), &failed, 1);
        assert_eq!(r.images["img"].failures, 1);
        r.put("img", k.clone(), &failed, 2);
        assert_eq!(r.images["img"].failures, 2);
        assert_eq!(
            judge(&r, "img", &k),
            Standing::Failed {
                failed: vec![
                    Failure::Guest(Check::Filevault),
                    Failure::Guest(Check::AppleId)
                ],
                why: None
            }
        );
        // A new key starts the count over; a pass clears it.
        r.put("img", key("bb", "26A434"), &failed, 3);
        assert_eq!(r.images["img"].failures, 1);
        r.put("img", k.clone(), &Outcome::Passed, 4);
        assert_eq!(r.images["img"].failures, 0);
        assert!(r.images["img"].passed());
    }

    #[test]
    fn re_verification_is_due_once_per_key_and_backs_off_after_failures() {
        let mut r = empty();
        let k = key("aa", "26A434");
        assert!(due(&r, "img", &k, 0), "a key with no result is due at once");
        r.put("img", k.clone(), &Outcome::Passed, 1_000);
        assert!(!due(&r, "img", &k, u64::MAX), "a pass is never due again");
        assert!(
            due(&r, "img", &key("aa", "26B12"), 1_000),
            "a host update is due"
        );
        let fail = Outcome::of_checks(&[Check::Filevault]);
        r.put("img", k.clone(), &fail, 1_000);
        let hour = 3_600_000;
        assert!(
            !due(&r, "img", &k, 1_000),
            "never straight away: no tight loop"
        );
        assert!(!due(&r, "img", &k, 1_000 + hour - 1));
        assert!(due(&r, "img", &k, 1_000 + hour));
        r.put("img", k.clone(), &fail, 1_000);
        assert!(
            !due(&r, "img", &k, 1_000 + hour),
            "the second waits two hours"
        );
        assert!(due(&r, "img", &k, 1_000 + 2 * hour));
        assert_eq!(backoff(1), FIRST_BACKOFF);
        assert_eq!(backoff(3), FIRST_BACKOFF * 4);
        assert_eq!(backoff(40), MAX_BACKOFF);
    }

    static SEQ: AtomicU64 = AtomicU64::new(0);
    struct Tmp(PathBuf);
    impl Tmp {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "faber-verify-test-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn a_run_that_could_not_finish_backs_off_briefly_and_a_check_for_an_hour() {
        let k = key("aa", "26A434");
        let mut r = empty();
        let run = Outcome::Failed {
            failed: vec![Failure::Run],
            why: Some("adoption pending".into()),
        };
        r.put("img", k.clone(), &run, 0);
        let min = 60_000;
        assert!(!due(&r, "img", &k, 5 * min - 1));
        assert!(
            due(&r, "img", &k, 5 * min),
            "a run failure retries after five minutes"
        );
        r.put("img", k.clone(), &run, 0);
        assert!(!due(&r, "img", &k, 10 * min - 1) && due(&r, "img", &k, 10 * min));
        assert_eq!(run_backoff(40), MAX_RUN_BACKOFF);
        r.put("img", k.clone(), &Outcome::of_checks(&[Check::Xcode]), 0);
        assert!(
            !due(&r, "img", &k, 59 * min),
            "a failed check waits an hour"
        );
        assert!(due(&r, "img", &k, 60 * min));
    }

    #[test]
    fn a_rapid_security_response_is_read_from_the_cryptex_copy() {
        assert_eq!(pick_version_plist(true), CRYPTEX_SYSTEM_VERSION_PLIST);
        assert_eq!(pick_version_plist(false), SYSTEM_VERSION_PLIST);
    }

    #[test]
    fn the_hosts_build_is_read_from_system_version() {
        let plist = "<?xml version=\"1.0\"?>\n<plist version=\"1.0\">\n<dict>\n\t<key>ProductBuildVersion</key>\n\t<string>26A434</string>\n\t<key>ProductName</key>\n\t<string>macOS</string>\n</dict>\n</plist>\n";
        assert_eq!(build_from_plist(plist).as_deref(), Some("26A434"));
        assert_eq!(
            build_from_plist("<key>ProductBuildVersion</key><string>26A 434</string>"),
            None
        );
        assert_eq!(
            build_from_plist("<key>ProductName</key><string>macOS</string>"),
            None
        );
    }

    #[test]
    fn an_identity_drifts_with_any_file_or_the_host() {
        let t = Tmp::new();
        bundle(&t.0);
        let mut cache = BTreeMap::new();
        let files = hash_image(&t.0, &mut cache).unwrap();
        let pinned = Identity::of_cache(&cache, &files, "26A434").unwrap();
        let mut now = pinned.clone();
        assert_eq!(pinned.drift(&now), None);
        now.macos_build = "26B12".into();
        assert!(matches!(pinned.drift(&now), Some(Why::HostChanged { .. })));
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(t.0.join("vz-aux"), b"rebuilt").unwrap();
        let mut cache2 = BTreeMap::new();
        let files2 = hash_image(&t.0, &mut cache2).unwrap();
        let changed = Identity::of_cache(&cache2, &files2, "26A434").unwrap();
        assert_eq!(pinned.drift(&changed), Some(Why::ImageChanged));
    }

    #[test]
    fn the_record_survives_a_restart_and_a_foreign_one_is_ignored() {
        let t = Tmp::new();
        let path = t.0.join(RECORD_FILE);
        assert_eq!(Record::load(&path), empty(), "absent is empty");
        let mut r = empty();
        r.put("img", key("aa", "26A434"), &Outcome::Passed, 5);
        r.save(&path).unwrap();
        assert_eq!(Record::load(&path), r);
        std::fs::write(&path, br#"{"schema":99,"images":{}}"#).unwrap();
        assert_eq!(
            Record::load(&path),
            empty(),
            "another schema verifies again"
        );
        std::fs::write(&path, b"not json").unwrap();
        assert_eq!(Record::load(&path), empty());
    }

    /// An image that passed a schema-1 `xcode` check, run as root, served jobs that
    /// found no developer tools. Its pass must not admit after the check got
    /// stricter; its hashes may stay.
    #[test]
    fn a_pass_under_a_weaker_check_verifies_again_and_keeps_its_hashes() {
        let t = Tmp::new();
        let path = t.0.join(RECORD_FILE);
        let mut old = empty();
        old.schema = 1;
        old.put("img", key("aa", "26A434"), &Outcome::Passed, 5);
        let print = Fingerprint {
            dev: 1,
            ino: 2,
            size: 3,
            mtime_ns: 4,
            ctime_ns: 5,
            sha256: "aa".into(),
        };
        old.hashes.insert(
            "img".into(),
            [("disk".to_string(), print)].into_iter().collect(),
        );
        old.save(&path).unwrap();

        let loaded = Record::load(&path);
        assert_eq!(loaded.schema, SCHEMA);
        assert_eq!(
            judge(&loaded, "img", &key("aa", "26A434")),
            Standing::Unverified(Why::NeverVerified),
            "the old pass no longer admits"
        );
        assert_eq!(loaded.hashes, old.hashes, "the hashes still hold");
    }

    fn bundle(dir: &Path) {
        for f in crate::setup::vm_images::GOLDEN_FILES {
            std::fs::write(dir.join(f), f.as_bytes()).unwrap();
        }
    }

    #[test]
    fn a_files_hash_is_reused_while_its_identity_holds() {
        let t = Tmp::new();
        bundle(&t.0);
        let mut cache = BTreeMap::new();
        let first = hash_image(&t.0, &mut cache).unwrap();
        assert_eq!(first.len(), 4, "the four files, no manifest");
        assert_eq!(first["disk"].len(), 64);
        // A cached hash for an unchanged file is used as is: no re-read.
        cache.get_mut("disk").unwrap().sha256 = "cached".into();
        assert_eq!(hash_image(&t.0, &mut cache).unwrap()["disk"], "cached");
        // A change to the file is a new identity, so it is hashed again.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(t.0.join("disk"), b"another disk").unwrap();
        let after = hash_image(&t.0, &mut cache).unwrap();
        assert_ne!(after["disk"], "cached");
        assert_ne!(after["disk"], first["disk"]);
        // The manifest joins the key when present.
        std::fs::write(t.0.join("manifest.json"), b"{}").unwrap();
        assert!(hash_image(&t.0, &mut cache)
            .unwrap()
            .contains_key("manifest.json"));
        // A missing required file is no key at all.
        std::fs::remove_file(t.0.join("vz-aux")).unwrap();
        assert!(hash_image(&t.0, &mut cache).is_err());
    }

    #[test]
    fn expectations_come_from_the_manifests_installed_half_held_to_the_wire() {
        let m = br#"{"schema":1,"recipe":{"runtimes":["iOS"]},"built_at":1,
            "installed":{"reported_by":"guest","macos_build":"26A434","xcode_version":"27.0 Beta 2",
            "xcode_build":"18A5123b","runtimes":[{"platform":"iOS","version":"27.0","build":"24A335"}],
            "brew":[]}}"#;
        let e = expect_from_manifest(Some(m)).unwrap();
        let x = e.xcode.unwrap();
        assert_eq!(x.version.as_str(), "27.0 Beta 2");
        assert_eq!(x.build.as_str(), "18A5123b");
        assert_eq!(e.runtimes.len(), 1);
        assert_eq!(e.runtimes[0].platform.as_str(), "iOS");
        // No manifest: no particular Xcode, no runtimes.
        let none = expect_from_manifest(None).unwrap();
        assert!(none.xcode.is_none() && none.runtimes.is_empty());
        // Anything that would not fit the wire is refused here, never sent.
        for bad in [
            &br#"{"schema":2,"installed":{"xcode_version":"27.0","xcode_build":"18A5123b"}}"#[..],
            br#"{"schema":1,"installed":{"xcode_version":"27.0;rm","xcode_build":"18A5123b"}}"#,
            br#"{"schema":1,"installed":{"xcode_version":"27.0","xcode_build":"18"}}"#,
            br#"{"schema":1,"installed":{"xcode_version":"27.0","xcode_build":"18A5123b",
                "runtimes":[{"platform":"iOS 1","version":"27.0","build":"24A335"}]}}"#,
            br#"{"schema":1,"installed":{"xcode_version":"27.0","xcode_build":"18A5123b",
                "runtimes":[{"platform":"iOS","version":"27.0","build":"24A335","x":1}]}}"#,
            b"[]",
        ] {
            assert!(
                expect_from_manifest(Some(bad)).is_err(),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn each_standing_says_what_is_wrong_and_what_to_do() {
        assert_eq!(Standing::Verified.detail("img"), None);
        assert!(Standing::Verified.admits());
        for s in [
            Standing::Unknown,
            Standing::Verifying,
            Standing::Unverified(Why::NeverVerified),
            Standing::Unverified(Why::ImageChanged),
            Standing::Unverified(Why::Unreadable("x".into())),
        ] {
            assert!(!s.admits(), "{s:?}");
            assert!(s.detail("img").unwrap().contains("img"));
        }
        for check in Check::ALL {
            let s = Standing::Failed {
                failed: vec![Failure::Guest(check)],
                why: None,
            };
            let text = s.detail("img").unwrap();
            assert!(text.contains(&format!("({})", check.as_str())), "{text}");
            assert!(text.contains("faber golden prepare --from img"), "{text}");
            assert!(
                text.contains("`sudo cat -v /var/db/faber-mac/state/verify-img-latest.log`"),
                "a failed check names where its details are: {text}"
            );
            assert_eq!(s.summary().text(), format!("failed:{}", check.as_str()));
        }
        // A guest check alongside the wall still points at the details.
        let both = Standing::Failed {
            failed: vec![
                Failure::Guest(Check::AdminsLocked),
                Failure::Guest(Check::SoftwareUpdate),
                Failure::Wall,
            ],
            why: None,
        };
        let text = both.detail("xcode-27.1").unwrap();
        assert!(
            text.contains("also failed: software_update, wall")
                && text
                    .contains("sudo cat -v /var/db/faber-mac/state/verify-xcode-27.1-latest.log"),
            "{text}"
        );
        let wall = Standing::Failed {
            failed: vec![Failure::Wall],
            why: None,
        };
        assert!(wall
            .detail("img")
            .unwrap()
            .contains("sudo faber wall check"));
        let run = Standing::Failed {
            failed: vec![Failure::Run],
            why: Some("the VM did not boot".into()),
        };
        let text = run.detail("img").unwrap();
        assert!(text.contains("the VM did not boot") && text.contains("faber golden verify img"));
        assert!(
            !text.contains("prepare"),
            "a run failure is no reason to rebuild"
        );
        assert!(
            !text.contains("sudo cat") && !wall.detail("img").unwrap().contains("sudo cat"),
            "no guest check ran to explain"
        );
        assert_eq!(
            details_command("xcode-27.1"),
            "sudo cat -v /var/db/faber-mac/state/verify-xcode-27.1-latest.log"
        );
        assert_eq!(
            Summary::worst([Summary::Verified, Summary::Verifying]),
            Summary::Verifying
        );
        assert_eq!(
            Summary::worst([Summary::Unverified, Summary::Failed(Failure::Wall)]).text(),
            "failed:wall"
        );
        assert_eq!(Summary::worst([]), Summary::Unknown);
    }

    #[test]
    fn a_verification_claim_is_this_instances_and_no_jobs() {
        let c = claim_for("a1b2c3d4", 1_760_700_000_000).unwrap();
        let rec = crate::vm::proto::VmRecord {
            slot: 0.try_into().unwrap(),
            kind: crate::vm::proto::VmKind::Macos,
            claim: c,
            state: crate::vm::proto::VmState::Verified,
            stop_reason: None,
            failed: Some(vec![]),
            started_unix: 1.try_into().unwrap(),
            disk_bytes: None,
            disk_allocated_bytes: None,
        };
        assert_eq!(
            crate::vm::adopt::ours(&rec, "a1b2c3d4"),
            crate::vm::adopt::Ownership::Ours
        );
        // A verification found at boot is stopped: nobody waits for its result.
        assert!(matches!(
            crate::vm::adopt::adoption_action(&rec, "a1b2c3d4"),
            crate::vm::adopt::AdoptAction::Stop(_)
        ));
    }

    #[test]
    fn a_request_is_taken_once_until_it_is_done() {
        let (h, mut rx) = Handle::new(vec!["img".into()]);
        assert_eq!(
            h.request("nope"),
            Err(RequestError::UnknownImage("nope".into()))
        );
        assert_eq!(h.request("img"), Ok(true));
        assert_eq!(h.request("img"), Ok(false), "already queued: wait for it");
        assert_eq!(rx.try_recv().unwrap(), "img");
        assert!(rx.try_recv().is_err(), "queued once");
        assert!(!h.take("img"), "the schedule does not run it twice");
        h.done("img");
        assert!(h.take("img"));
    }

    #[test]
    fn the_cli_takes_only_a_result_seen_after_verifying() {
        let state = |s: &str| {
            serde_json::json!({"budget": {"vm": {"golden": [
                {"image": "other", "standing": "failed:xcode", "detail": "x"},
                {"image": "img", "standing": s, "detail": null}
            ]}}})
        };
        let row = |s: &str| golden_row(&state(s), "img");
        assert_eq!(row("verified"), Some(("verified".into(), None)));
        assert_eq!(golden_row(&state("verified"), "missing"), None);
        // An old verdict before this run started is not this run's result.
        let w = cli_step(&CliWait::Queued, row("verified"));
        assert_eq!(w, CliWait::Queued);
        let w = cli_step(&w, row("verifying"));
        assert_eq!(w, CliWait::Running);
        let w = cli_step(&w, row("verifying"));
        assert_eq!(w, CliWait::Running);
        assert_eq!(
            cli_step(&w, row("failed:apple_id")),
            CliWait::Ended("failed:apple_id".into(), None)
        );
    }

    // --- the daemon's loop, on a fake host --------------------------------

    struct FakeHost {
        build: Mutex<String>,
        disk: Mutex<String>,
        ends: Mutex<Vec<VerifyEnd>>,
        runs: Mutex<u32>,
        manifest_ok: bool,
    }

    impl FakeHost {
        fn new(ends: Vec<VerifyEnd>) -> Self {
            FakeHost {
                build: Mutex::new("26A434".into()),
                disk: Mutex::new("aa".into()),
                ends: Mutex::new(ends),
                runs: Mutex::new(0),
                manifest_ok: true,
            }
        }
    }

    impl Host for &FakeHost {
        async fn macos_build(&self) -> Result<String, String> {
            Ok(self.build.lock().unwrap().clone())
        }
        async fn hash(
            &self,
            _: &str,
            cache: BTreeMap<String, Fingerprint>,
        ) -> (
            Result<BTreeMap<String, String>, String>,
            BTreeMap<String, Fingerprint>,
        ) {
            let disk = self.disk.lock().unwrap().clone();
            (
                Ok([("disk".to_string(), disk)].into_iter().collect()),
                cache,
            )
        }
        fn expect(&self, _: &str) -> Result<Expect, String> {
            if self.manifest_ok {
                expect_from_manifest(None)
            } else {
                Err("its manifest.json is not one faber reads".into())
            }
        }
        async fn run(&self, _: &str, _: Expect) -> VerifyEnd {
            *self.runs.lock().unwrap() += 1;
            let mut ends = self.ends.lock().unwrap();
            if ends.is_empty() {
                VerifyEnd::Checked(vec![])
            } else {
                ends.remove(0)
            }
        }
    }

    fn limiter() -> Arc<crate::limiter::Limiter> {
        let tuning =
            crate::config::AutoLimitFile::default().resolve(&crate::config::Overrides::default());
        crate::limiter::Limiter::new(tuning, &std::collections::HashMap::new())
    }

    fn verifier(host: &FakeHost) -> Verifier<&FakeHost> {
        let (handle, _rx) = Handle::new(vec!["img".into()]);
        Verifier {
            host,
            untold: BTreeMap::new(),
            handle,
            limiter: limiter(),
            images: vec!["img".into()],
            record: empty(),
            path: None,
        }
    }

    #[tokio::test]
    async fn the_daemon_verifies_a_new_image_once_and_publishes_it() {
        let host = FakeHost::new(vec![]);
        let mut v = verifier(&host);
        v.limiter.require_verified_images();
        assert_eq!(v.pass(None).await, vec!["img".to_string()]);
        assert_eq!(v.limiter.golden("img"), Standing::Verified);
        assert_eq!(v.limiter.snapshot().vm.images.macos.text(), "verified");
        // Passed under this key: the next passes run nothing.
        assert!(v.pass(None).await.is_empty());
        assert!(v.pass(None).await.is_empty());
        assert_eq!(*host.runs.lock().unwrap(), 1);
        // The host's macOS updates: unverified, and verified again by itself.
        *host.build.lock().unwrap() = "26B12".into();
        assert_eq!(v.pass(None).await.len(), 1);
        // A file of the image changes: the same.
        *host.disk.lock().unwrap() = "bb".into();
        assert_eq!(v.pass(None).await.len(), 1);
        assert_eq!(*host.runs.lock().unwrap(), 3);
    }

    #[tokio::test]
    async fn a_failed_verification_gates_and_waits_out_its_back_off() {
        let host = FakeHost::new(vec![
            VerifyEnd::Checked(vec![Check::RemoteLogin]),
            VerifyEnd::Checked(vec![]),
        ]);
        let mut v = verifier(&host);
        v.pass(None).await;
        assert_eq!(
            v.limiter.golden("img").summary().text(),
            "failed:remote_login"
        );
        // Not again on its own, however often the loop passes.
        for _ in 0..5 {
            assert!(v.pass(None).await.is_empty());
        }
        assert_eq!(*host.runs.lock().unwrap(), 1);
        assert_eq!(
            v.limiter.golden("img").summary().text(),
            "failed:remote_login"
        );
        // The operator asks: it runs now, back-off or not.
        assert!(v.handle.request("img").unwrap());
        assert_eq!(v.pass(Some("img")).await.len(), 1);
        assert_eq!(v.limiter.golden("img"), Standing::Verified);
        assert!(v.handle.take("img"), "the request is done");
    }

    #[tokio::test]
    async fn a_wall_breach_or_a_broken_run_is_recorded_and_a_cancel_is_not() {
        let host = FakeHost::new(vec![VerifyEnd::Cancelled, VerifyEnd::WallBreached]);
        let mut v = verifier(&host);
        v.pass(None).await;
        assert!(
            v.record.images.is_empty(),
            "a cancelled run teaches nothing"
        );
        v.untold.clear();
        assert_eq!(
            v.limiter.golden("img"),
            Standing::Unverified(Why::NeverVerified)
        );
        v.pass(None).await;
        assert_eq!(v.limiter.golden("img").summary().text(), "failed:wall");
        let host = FakeHost::new(vec![VerifyEnd::Failed(anyhow::anyhow!("no VM session"))]);
        let mut v = verifier(&host);
        v.pass(None).await;
        let s = v.limiter.golden("img");
        assert_eq!(s.summary().text(), "failed:run");
        assert!(s.detail("img").unwrap().contains("no VM session"));
    }

    #[tokio::test]
    async fn a_run_that_teaches_nothing_backs_off_instead_of_retrying_every_pass() {
        let host = FakeHost::new(vec![VerifyEnd::Cancelled, VerifyEnd::Cancelled]);
        let mut v = verifier(&host);
        assert_eq!(v.pass(None).await.len(), 1);
        for _ in 0..5 {
            assert!(
                v.pass(None).await.is_empty(),
                "held off, not retried every pass"
            );
        }
        assert_eq!(*host.runs.lock().unwrap(), 1);
        // Once the back-off is over, it runs again, and holds off longer.
        v.untold.get_mut("img").unwrap().1 = 0;
        assert_eq!(v.pass(None).await.len(), 1);
        assert_eq!(v.untold["img"].2, 2);
        // A changed image is a new key: it is due at once.
        *host.disk.lock().unwrap() = "bb".into();
        assert_eq!(v.pass(None).await.len(), 1);
    }

    #[tokio::test]
    async fn an_unreadable_manifest_is_unverified_and_runs_nothing() {
        let mut host = FakeHost::new(vec![]);
        host.manifest_ok = false;
        let mut v = verifier(&host);
        assert!(v.pass(None).await.is_empty());
        assert_eq!(*host.runs.lock().unwrap(), 0);
        assert!(matches!(
            v.limiter.golden("img"),
            Standing::Unverified(Why::Unreadable(_))
        ));
        assert!(v.handle.take("img"), "nothing is left pending");
    }
}
