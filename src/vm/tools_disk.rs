//! The tools disk (ADR-0026, "The runner and the agent arrive on a tools disk"):
//! the one read-only disk image a macOS clone boots with beside its golden image,
//! holding the `actions/runner` tarball the job pins and faber's guest agent.
//!
//! Neither lives in the golden image, so the image never goes stale against a
//! runner release and never needs rebuilding to ship a new agent. Instead faber
//! builds one disk per (runner version, agent build) pair, caches it under
//! `D/tools-disks`, and the supervisor attaches it by *name* (`Start.tools`, an
//! [`ImageName`]) — never by path (`docs/vm-supervisor-protocol.md`).
//!
//! What goes on the disk is checked before it is written, because every later VM
//! starts from it:
//!
//! - **The runner** is the version the job pins (the choice ADR-0008 makes for
//!   runner images), refused rather than defaulted when the pin is not a
//!   `N.N.N` release, and reported by length, never value (#113). Its tarball is
//!   checked against the SHA-256 GitHub publishes for that release *by exact
//!   tag*: the hash in the release body is required, and the API's asset digest,
//!   when present, must agree with it. The tarball is never extracted on the host;
//!   it goes onto the disk as downloaded and the guest agent unpacks it.
//! - **The agent** comes from the installed, verified faber release (ADR-0031).
//!   It is read once into memory, hashed, and written from those same bytes, so
//!   the file on the disk is the file that was hashed.
//!
//! The cache is written by `_faber` and only read by `_faber_vm` (ADR-0025): the
//! root is 0755, every image and marker 0444, all modes set explicitly rather
//! than left to the umask. A VM escape as `_faber_vm` therefore cannot change
//! what later VMs start from, and [`ToolsDiskCache::ensure`] refuses a cache hit
//! whose root, image or marker has gained a group or other write bit — it never
//! rebuilds over one in place, because something wrote where nothing should.
//!
//! An entry is `<name>.img` plus the marker `<name>.faber-tools.json`, written
//! last: marker present means image complete. Builds stage under `.staging/`
//! in the same root and land by rename. Eviction reads markers, never names: a
//! file without a valid marker naming itself is not faber's to delete (AGENTS.md,
//! "Read labels, never names").
//!
//! A job reaches all of this through [`for_this_host`]: no config names a tools
//! disk, since its name follows from the runner version and the agent's bytes.
//!
//! Errors never carry GitHub's bytes or a pin's value; they name a category, a
//! length, or the already-validated version.

use std::collections::HashSet;
use std::fmt;
use std::future::Future;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

use super::proto::{ImageName, MAX_NAME_LEN};
use crate::github::{Github, GithubError};
use crate::runner::{asset_url, transport_detail, PLATFORM};

/// The longest release body read for the published hash. Runner release notes
/// are a few KiB; anything past this is refused, not scanned.
pub const MAX_BODY: usize = 256 * 1024;
/// The largest runner tarball accepted. The osx-arm64 tarball is under 200 MiB.
pub const MAX_TARBALL: u64 = 512 * 1024 * 1024;
/// The largest agent binary accepted.
pub const MAX_AGENT: u64 = 256 * 1024 * 1024;
/// The largest marker file read back.
const MAX_MARKER: u64 = 4096;
/// The marker's `kind`, the provenance eviction trusts.
pub const MARKER_KIND: &str = "faber-tools-disk";
/// The marker file's suffix after the entry name.
const MARKER_SUFFIX: &str = ".faber-tools.json";
/// The image file's suffix after the entry name.
const IMAGE_SUFFIX: &str = ".img";
/// The agent's file name on the disk.
pub const AGENT_FILE: &str = "faber-agent";
/// The manifest's file name on the disk.
pub const MANIFEST_FILE: &str = "manifest.json";
/// The disk's volume name.
const VOLUME_NAME: &str = "FABERTOOLS";
/// How old a `.staging/` entry must be before it is reaped as a crash orphan.
const STAGING_ORPHAN_AGE: Duration = Duration::from_secs(60 * 60);
/// The whole-download timeout for the runner tarball.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Distinguishes this process's concurrent staging dirs.
static STAGING_SEQ: AtomicU64 = AtomicU64::new(0);

/// Why a tools disk could not be had. No variant carries GitHub's text or a
/// pin's value.
#[derive(Debug, thiserror::Error)]
pub enum ToolsDiskError {
    #[error("the job's runner pin is not a runner release version ({len} bytes) — refusing it rather than running another version")]
    InvalidPin { len: usize },
    #[error("the profile's default runner version is not a runner release version ({len} bytes)")]
    InvalidDefault { len: usize },
    #[error("actions/runner has no release tagged v{0}")]
    ReleaseNotFound(String),
    #[error("GitHub could not answer for actions/runner v{version}: {detail}")]
    Github { version: String, detail: String },
    #[error("the published SHA-256 for actions/runner v{version} is unusable: {why}")]
    PublishedHash { version: String, why: &'static str },
    #[error("downloading actions/runner v{version} failed: {detail}")]
    Download { version: String, detail: String },
    #[error("actions/runner v{version} is larger than {MAX_TARBALL} bytes — refused")]
    TooLarge { version: String },
    #[error(
        "actions/runner v{version} does not match the SHA-256 GitHub published for it — refused"
    )]
    HashMismatch { version: String },
    #[error("the agent binary is unusable: {0}")]
    Agent(&'static str),
    #[error(
        "no guest agent is installed at {0}: `sudo faber setup` installs it from the release \
         beside the launcher, and every update after that replaces it"
    )]
    NoAgent(PathBuf),
    #[error("cannot name the tools disk: {0}")]
    Name(&'static str),
    #[error("the tools-disk cache is unsafe to use: {0}")]
    Unsafe(String),
    #[error("building the tools disk failed: {0}")]
    Build(String),
    #[error("tools-disk cache io error: {0}")]
    Io(#[from] std::io::Error),
}

// ---------------------------------------------------------------------------
// The runner version
// ---------------------------------------------------------------------------

/// An `actions/runner` release version, `N.N.N` (without the `v`), validated by
/// the same grammar the retirement parser reads GitHub's annotation with.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RunnerVersion(String);

impl RunnerVersion {
    /// `s` with at most one leading `v` stripped, or `None` when it is not a
    /// runner release version.
    pub fn parse(s: &str) -> Option<Self> {
        let bare = s.strip_prefix('v').unwrap_or(s);
        crate::runner_retirement::runner_version(bare).then(|| Self(bare.to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for RunnerVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The runner version a job runs: its pin when it has one, else the profile's
/// default. An invalid pin is refused, not defaulted — running a version the job
/// did not ask for would hide the mismatch — and reported by its length, since
/// the value is the repo's choice (#113). The default must be valid too.
pub fn runner_version_for(
    pin: Option<&str>,
    profile_default: &str,
) -> Result<RunnerVersion, ToolsDiskError> {
    match pin {
        Some(p) => RunnerVersion::parse(p).ok_or(ToolsDiskError::InvalidPin { len: p.len() }),
        None => RunnerVersion::parse(profile_default).ok_or(ToolsDiskError::InvalidDefault {
            len: profile_default.len(),
        }),
    }
}

// ---------------------------------------------------------------------------
// The published hash
// ---------------------------------------------------------------------------

/// One asset of a runner release, as the hash check reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublishedAsset {
    pub name: String,
    /// The API's `digest`, `sha256:<hex>`, when GitHub reports one.
    pub digest: Option<String>,
}

/// What [`RunnerSource::release`] returns: the release body and its assets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RunnerRelease {
    pub body: String,
    pub assets: Vec<PublishedAsset>,
}

/// The tarball's file name for `version`, as GitHub names the asset.
pub fn tarball_name(version: &RunnerVersion) -> String {
    format!("actions-runner-{PLATFORM}-{version}.tar.gz")
}

/// Exactly 64 hex digits, either case, as 32 bytes.
fn parse_hex32(s: &str) -> Option<[u8; 32]> {
    let b = s.as_bytes();
    if b.len() != 64 {
        return None;
    }
    let nib = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    let mut out = [0u8; 32];
    for (i, o) in out.iter_mut().enumerate() {
        *o = (nib(b[2 * i])? << 4) | nib(b[2 * i + 1])?;
    }
    Some(out)
}

/// Lowercase hex of `bytes`.
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The SHA-256 GitHub published for `version`'s osx-arm64 tarball.
///
/// The release body's `<!-- BEGIN SHA osx-arm64 -->{hex}<!-- END SHA osx-arm64 -->`
/// is required: exactly one, holding exactly 64 hex digits. The API's asset
/// `digest`, when the asset carries one, must say the same; an absent digest is
/// fine (GitHub only computes them for newer uploads). Anything else is refused.
pub fn published_sha256(
    body: &str,
    assets: &[PublishedAsset],
    version: &RunnerVersion,
) -> Result<[u8; 32], ToolsDiskError> {
    let refuse = |why| ToolsDiskError::PublishedHash {
        version: version.to_string(),
        why,
    };
    if body.len() > MAX_BODY {
        return Err(refuse("the release body is too long"));
    }
    let begin = format!("<!-- BEGIN SHA {PLATFORM} -->");
    let end = format!("<!-- END SHA {PLATFORM} -->");
    if body.matches(&begin).count() != 1 || body.matches(&end).count() != 1 {
        return Err(refuse(
            "the release body does not hold exactly one hash block",
        ));
    }
    let after = &body[body.find(&begin).map(|i| i + begin.len()).unwrap_or(0)..];
    let inner = after
        .find(&end)
        .map(|i| &after[..i])
        .ok_or_else(|| refuse("the hash block is not closed"))?;
    let from_body =
        parse_hex32(inner).ok_or_else(|| refuse("the hash block is not 64 hex digits"))?;

    let name = tarball_name(version);
    let mut matching = assets.iter().filter(|a| a.name == name);
    let asset = matching.next();
    if matching.next().is_some() {
        return Err(refuse("the release lists the tarball twice"));
    }
    if let Some(digest) = asset.and_then(|a| a.digest.as_deref()) {
        let from_api = digest
            .strip_prefix("sha256:")
            .and_then(parse_hex32)
            .ok_or_else(|| refuse("the asset digest is not sha256:<64 hex>"))?;
        if from_api != from_body {
            return Err(refuse("the asset digest disagrees with the release body"));
        }
    }
    Ok(from_body)
}

// ---------------------------------------------------------------------------
// The seams
// ---------------------------------------------------------------------------

/// A runner tarball arriving in chunks.
pub trait Download: Send {
    /// The next chunk, `None` at the end.
    fn chunk(&mut self) -> impl Future<Output = Result<Option<Vec<u8>>, ToolsDiskError>> + Send;
}

/// Where runner releases come from: GitHub in production, a fake in tests.
pub trait RunnerSource: Sync {
    type Download: Download;
    /// The release tagged exactly `v{version}`, `None` when there is none.
    fn release(
        &self,
        version: &RunnerVersion,
    ) -> impl Future<Output = Result<Option<RunnerRelease>, ToolsDiskError>> + Send;
    /// Start downloading `version`'s osx-arm64 tarball.
    fn download(
        &self,
        version: &RunnerVersion,
    ) -> impl Future<Output = Result<Self::Download, ToolsDiskError>> + Send;
}

/// What turns a staged directory into a disk image.
pub trait DiskBuilder: Sync {
    /// Build a read-only image of `src_dir`'s contents at exactly `out`.
    fn build(&self, src_dir: &Path, out: &Path) -> impl Future<Output = anyhow::Result<()>> + Send;
}

/// The production [`RunnerSource`]: release metadata through faber's GitHub
/// client, the tarball from the release-download URL [`crate::runner`] uses.
pub struct GithubRunnerSource<'a> {
    gh: &'a Github,
    http: reqwest::Client,
}

impl<'a> GithubRunnerSource<'a> {
    pub fn new(gh: &'a Github) -> Result<Self, ToolsDiskError> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("faber/", env!("FABER_VERSION")))
            .connect_timeout(Duration::from_secs(10))
            .timeout(DOWNLOAD_TIMEOUT)
            .build()
            .map_err(|e| ToolsDiskError::Build(transport_detail(&e)))?;
        Ok(Self { gh, http })
    }
}

/// A GitHub failure, rendered without anything GitHub sent back.
fn github_detail(e: &GithubError) -> String {
    match e {
        GithubError::Status { status, .. } => format!("status {status}"),
        GithubError::Decode(_) => "the response did not decode".to_string(),
        GithubError::Http(e) => transport_detail(e),
        _ => "request failed".to_string(),
    }
}

impl RunnerSource for GithubRunnerSource<'_> {
    type Download = ReqwestDownload;

    async fn release(
        &self,
        version: &RunnerVersion,
    ) -> Result<Option<RunnerRelease>, ToolsDiskError> {
        let release = self
            .gh
            .runner_release(version.as_str())
            .await
            .map_err(|e| ToolsDiskError::Github {
                version: version.to_string(),
                detail: github_detail(&e),
            })?;
        Ok(release.map(|r| RunnerRelease {
            body: r.body.unwrap_or_default(),
            assets: r
                .assets
                .into_iter()
                .map(|a| PublishedAsset {
                    name: a.name,
                    digest: a.digest,
                })
                .collect(),
        }))
    }

    async fn download(&self, version: &RunnerVersion) -> Result<ReqwestDownload, ToolsDiskError> {
        let fail = |detail| ToolsDiskError::Download {
            version: version.to_string(),
            detail,
        };
        let resp = self
            .http
            .get(asset_url(version.as_str()))
            .send()
            .await
            .map_err(|e| fail(transport_detail(&e)))?;
        if !resp.status().is_success() {
            return Err(fail(format!("status {}", resp.status().as_u16())));
        }
        if resp.content_length().is_some_and(|n| n > MAX_TARBALL) {
            return Err(ToolsDiskError::TooLarge {
                version: version.to_string(),
            });
        }
        Ok(ReqwestDownload {
            resp,
            version: version.to_string(),
        })
    }
}

/// A tarball download over `reqwest`.
pub struct ReqwestDownload {
    resp: reqwest::Response,
    version: String,
}

impl Download for ReqwestDownload {
    async fn chunk(&mut self) -> Result<Option<Vec<u8>>, ToolsDiskError> {
        self.resp
            .chunk()
            .await
            .map(|c| c.map(|b| b.to_vec()))
            .map_err(|e| ToolsDiskError::Download {
                version: self.version.clone(),
                detail: transport_detail(&e),
            })
    }
}

/// The production [`DiskBuilder`]: `hdiutil` on macOS.
///
/// `-format UDTO` writes a raw (DVD/CD-master) image, which Virtualization.framework
/// attaches as a block device directly; the compressed and sparse formats it
/// cannot. `hdiutil` appends `.cdr` to the name it is given, so the output is
/// renamed onto `out` afterwards. On Linux there is no `hdiutil` and the build
/// fails at run time, which is the right answer: there are no macOS VMs there.
pub struct Hdiutil;

/// The `hdiutil` output base and the `.cdr` file it writes for `out`.
///
/// `out` is `<name>.img`; `hdiutil` is given `<name>` and appends `.cdr`. The
/// name contains dots (`runner-2.330.0-agent-0.4.0-…`), so the `.cdr` path is
/// built by appending, never with `with_extension`, which would replace the
/// final dotted segment.
fn hdiutil_paths(out: &Path) -> (PathBuf, PathBuf) {
    let base = out.with_extension("");
    let mut cdr = base.clone().into_os_string();
    cdr.push(".cdr");
    (base, PathBuf::from(cdr))
}

impl DiskBuilder for Hdiutil {
    async fn build(&self, src_dir: &Path, out: &Path) -> anyhow::Result<()> {
        build_volume(src_dir, out, VOLUME_NAME).await
    }
}

/// A raw HFS+ image of `src_dir`'s contents at exactly `out` (`<name>.img`), its
/// volume named `volname`. The runner's tools disk is the one volume faber's
/// guests mount by that name; the golden build's setup disk is another
/// (ADR-0049). The child is killed if this future is dropped.
pub async fn build_volume(src_dir: &Path, out: &Path, volname: &str) -> anyhow::Result<()> {
    let (base, cdr) = hdiutil_paths(out);
    let status = tokio::process::Command::new("/usr/bin/hdiutil")
        .kill_on_drop(true)
        .arg("create")
        .arg("-quiet")
        .arg("-srcfolder")
        .arg(src_dir)
        .args(["-volname", volname, "-fs", "HFS+", "-format", "UDTO", "-ov"])
        .arg(&base)
        .status()
        .await?;
    anyhow::ensure!(status.success(), "hdiutil exited with {status}");
    tokio::fs::rename(&cdr, out).await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Download and verify
// ---------------------------------------------------------------------------

/// Stream `version`'s tarball into `dest` while hashing it, then compare with
/// `expected`. Over [`MAX_TARBALL`] or a mismatch removes `dest` and refuses.
pub async fn fetch_verified<S: RunnerSource>(
    source: &S,
    version: &RunnerVersion,
    expected: &[u8; 32],
    dest: &Path,
) -> Result<(), ToolsDiskError> {
    let result = fetch_into(source, version, expected, dest).await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(dest).await;
    }
    result
}

async fn fetch_into<S: RunnerSource>(
    source: &S,
    version: &RunnerVersion,
    expected: &[u8; 32],
    dest: &Path,
) -> Result<(), ToolsDiskError> {
    let mut download = source.download(version).await?;
    let mut file = tokio::fs::File::create(dest).await?;
    let mut hasher = Sha256::new();
    let mut total: u64 = 0;
    while let Some(chunk) = download.chunk().await? {
        total = total.saturating_add(chunk.len() as u64);
        if total > MAX_TARBALL {
            return Err(ToolsDiskError::TooLarge {
                version: version.to_string(),
            });
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    file.sync_all().await?;
    let got: [u8; 32] = hasher.finalize().into();
    if &got != expected {
        return Err(ToolsDiskError::HashMismatch {
            version: version.to_string(),
        });
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The agent and the name
// ---------------------------------------------------------------------------

/// The guest agent as it goes onto the disk: its version and the exact bytes
/// that were hashed.
#[derive(Clone)]
pub struct AgentInput {
    version: String,
    bytes: Vec<u8>,
    sha256: [u8; 32],
}

impl fmt::Debug for AgentInput {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AgentInput")
            .field("version", &self.version)
            .field("sha256", &hex(&self.sha256))
            .finish()
    }
}

impl AgentInput {
    /// Read the agent at `path` (from the installed, verified release) into
    /// memory and hash it. Refuses anything but a regular file: a symlink, even
    /// to a regular file, is refused, and so is a file swapped between the check
    /// and the open.
    pub fn read(path: &Path, version: &str) -> Result<Self, ToolsDiskError> {
        let before = std::fs::symlink_metadata(path)?;
        if !before.file_type().is_file() {
            return Err(ToolsDiskError::Agent("not a regular file"));
        }
        if before.len() > MAX_AGENT {
            return Err(ToolsDiskError::Agent("larger than the agent cap"));
        }
        let file = std::fs::File::open(path)?;
        let opened = file.metadata()?;
        if !opened.file_type().is_file()
            || opened.dev() != before.dev()
            || opened.ino() != before.ino()
        {
            return Err(ToolsDiskError::Agent("changed while it was opened"));
        }
        let mut bytes = Vec::new();
        file.take(MAX_AGENT + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_AGENT {
            return Err(ToolsDiskError::Agent("larger than the agent cap"));
        }
        let sha256 = Sha256::digest(&bytes).into();
        Ok(Self {
            version: version.to_string(),
            bytes,
            sha256,
        })
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    pub fn sha256(&self) -> &[u8; 32] {
        &self.sha256
    }
}

/// The agent version in the name grammar: `+` (semver build metadata) becomes
/// `.`; any other character outside `[a-z0-9._-]` is refused.
fn agent_version_for_name(v: &str) -> Result<String, ToolsDiskError> {
    if v.is_empty() {
        return Err(ToolsDiskError::Name("the agent version is empty"));
    }
    v.chars()
        .map(|c| match c {
            '+' => Ok('.'),
            'a'..='z' | '0'..='9' | '.' | '_' | '-' => Ok(c),
            _ => Err(ToolsDiskError::Name(
                "the agent version has a character no tools-disk name allows",
            )),
        })
        .collect()
}

/// The cache name for this pair: `runner-{rv}-agent-{av}-{8 hex of the agent's
/// SHA-256}`. The same inputs always give the same name; a rebuilt agent under an
/// unchanged version gives a different one, so a stale disk is never reused.
pub fn disk_name(runner: &RunnerVersion, agent: &AgentInput) -> Result<ImageName, ToolsDiskError> {
    let av = agent_version_for_name(&agent.version)?;
    let name = format!("runner-{runner}-agent-{av}-{}", &hex(&agent.sha256)[..8]);
    if name.len() > MAX_NAME_LEN {
        return Err(ToolsDiskError::Name(
            "the name is longer than the protocol allows",
        ));
    }
    ImageName::try_from(name.as_str())
        .map_err(|_| ToolsDiskError::Name("the name is not a valid tools-disk name"))
}

// ---------------------------------------------------------------------------
// The cache
// ---------------------------------------------------------------------------

/// What the disk's `manifest.json` records.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub runner_version: String,
    pub runner_sha256: String,
    pub agent_version: String,
    pub agent_sha256: String,
}

/// The marker beside each image: its provenance, and its completeness.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Marker {
    kind: String,
    name: String,
    runner_version: String,
    runner_sha256: String,
    agent_version: String,
    agent_sha256: String,
    built_unix: u64,
}

/// Refuse `path` if it is a symlink, is not owned by this process's effective
/// uid (the daemon, `_faber`), or has a group or other write bit: any of those
/// means an account other than the daemon could change what later VMs boot.
pub fn check_not_writable_by_others(path: &Path) -> Result<(), ToolsDiskError> {
    let meta = std::fs::symlink_metadata(path)?;
    if meta.file_type().is_symlink() {
        return Err(ToolsDiskError::Unsafe(format!(
            "{} is a symlink",
            path.display()
        )));
    }
    if meta.uid() != crate::platform::record::current_euid() {
        return Err(ToolsDiskError::Unsafe(format!(
            "{} is not owned by the daemon's account",
            path.display()
        )));
    }
    if meta.permissions().mode() & 0o022 != 0 {
        return Err(ToolsDiskError::Unsafe(format!(
            "{} is writable by group or others",
            path.display()
        )));
    }
    Ok(())
}

/// Make the staged tree what `runner` in the guest can use. `hdiutil create
/// -srcfolder`, run as `_faber`, stamps `_faber`'s uid and gid on every file and
/// gives the volume's root the source folder's own mode, and the guest's
/// bootstrap mounts it with owners honoured. So `runner`, another uid in another
/// directory, gets only the "other" bits: the root must be searchable and every
/// file readable by other, whatever the daemon's umask (027) left. Before this,
/// the root was 0700 and the tarball and manifest 0640, so launchd could not even
/// run `runner-exec` from the disk: the hand-off waits 60 s for it to connect,
/// which matches the `start_failed` about a minute in that the first macOS jobs
/// showed.
/// The staging directory above `src` stays 0700 on the host.
fn guest_readable(src: &Path, tarball: &str) -> std::io::Result<()> {
    set_mode(src, 0o755)?;
    set_mode(&src.join(tarball), 0o444)?;
    set_mode(&src.join(MANIFEST_FILE), 0o444)?;
    set_mode(&src.join(AGENT_FILE), 0o555)
}

/// Create `path` and any missing parents with mode `mode` from the first
/// instant, so a permissive umask never leaves a window in which another
/// account could write the directory. An existing directory is accepted (the
/// caller chmods it), which is for the long-lived cache dirs only.
fn create_dir_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(mode)
        .create(path)
}

/// Create exactly one directory with mode `mode`; it is an error if `path`
/// already exists, so a directory somebody else pre-created is never adopted.
fn create_fresh_dir(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new().mode(mode).create(path)
}

/// Set `path`'s mode to exactly `mode`.
fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Read and parse a marker, bounded; `None` for anything that is not one.
fn read_marker(path: &Path) -> Option<Marker> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.file_type().is_file() || meta.len() > MAX_MARKER {
        return None;
    }
    let mut text = String::new();
    std::fs::File::open(path)
        .ok()?
        .take(MAX_MARKER)
        .read_to_string(&mut text)
        .ok()?;
    let marker: Marker = serde_json::from_str(&text).ok()?;
    (marker.kind == MARKER_KIND).then_some(marker)
}

/// The production cache root: on a SYSTEM macOS host
/// [`crate::platform::layout::TOOLS_DISKS`], which setup creates `_faber`'s
/// own outside the state directory so the supervisor (`_faber_vm`) can read
/// it; otherwise `D/tools-disks`, `None` without a state dir.
pub fn default_root() -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    if crate::platform::layout() == crate::platform::layout::Layout::System {
        return Some(PathBuf::from(crate::platform::layout::TOOLS_DISKS));
    }
    crate::platform::state_dir().map(|d| d.join("tools-disks"))
}

/// The tools-disk cache under one root.
pub struct ToolsDiskCache {
    root: PathBuf,
    /// Serializes builds and eviction, so two jobs never build the same disk
    /// twice and eviction never removes one mid-build.
    build_lock: Mutex<()>,
}

impl ToolsDiskCache {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            build_lock: Mutex::new(()),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn image_path(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}{IMAGE_SUFFIX}"))
    }

    fn marker_path(&self, name: &str) -> PathBuf {
        self.root.join(format!("{name}{MARKER_SUFFIX}"))
    }

    fn staging_root(&self) -> PathBuf {
        self.root.join(".staging")
    }

    /// The disk for `runner` and `agent`, built if this host does not have it,
    /// and its name for the supervisor's `start.tools`.
    pub async fn ensure<S: RunnerSource, B: DiskBuilder>(
        &self,
        source: &S,
        builder: &B,
        runner: &RunnerVersion,
        agent: &AgentInput,
    ) -> Result<ImageName, ToolsDiskError> {
        let name = disk_name(runner, agent)?;
        if let Some(hit) = self.hit(&name, runner, agent)? {
            return Ok(hit);
        }
        let _lock = self.build_lock.lock().await;
        // Re-check under the lock: whoever held it may have built this one.
        if let Some(hit) = self.hit(&name, runner, agent)? {
            return Ok(hit);
        }
        create_dir_mode(&self.root, 0o755)?;
        set_mode(&self.root, 0o755)?;
        let staging_root = self.staging_root();
        create_dir_mode(&staging_root, 0o700)?;
        set_mode(&staging_root, 0o700)?;
        reap_stale_staging(&staging_root, SystemTime::now());

        let staged = staging_root.join(format!(
            "{}-{}",
            std::process::id(),
            STAGING_SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        let result = self
            .build(source, builder, &name, runner, agent, &staged)
            .await;
        let _ = tokio::fs::remove_dir_all(&staged).await;
        result.map(|()| name)
    }

    /// A complete, safe, matching cache entry for `name`, touched as used; `None`
    /// when there is no marker. An entry that exists but is unsafe or does not
    /// match is an error, never a rebuild.
    fn hit(
        &self,
        name: &ImageName,
        runner: &RunnerVersion,
        agent: &AgentInput,
    ) -> Result<Option<ImageName>, ToolsDiskError> {
        let marker_path = self.marker_path(name.as_str());
        if std::fs::symlink_metadata(&marker_path).is_err() {
            return Ok(None);
        }
        let image_path = self.image_path(name.as_str());
        // A marker with no image is what an eviction leaves when it removed
        // the image but not the marker. It must not wedge the name: drop the
        // marker (a marker-less entry is never a hit) and let the caller
        // rebuild. Only a genuinely absent image counts; anything else that
        // fails to stat is an error.
        if let Err(e) = std::fs::symlink_metadata(&image_path) {
            if e.kind() == std::io::ErrorKind::NotFound {
                std::fs::remove_file(&marker_path)?;
                return Ok(None);
            }
        }
        check_not_writable_by_others(&self.root)?;
        check_not_writable_by_others(&marker_path)?;
        check_not_writable_by_others(&image_path)?;
        if !std::fs::symlink_metadata(&image_path)?
            .file_type()
            .is_file()
        {
            return Err(ToolsDiskError::Unsafe(format!(
                "{} is not a regular file",
                image_path.display()
            )));
        }
        let marker = read_marker(&marker_path).ok_or_else(|| {
            ToolsDiskError::Unsafe(format!("{} is not a valid marker", marker_path.display()))
        })?;
        if marker.name != name.as_str()
            || marker.runner_version != runner.as_str()
            || marker.agent_version != agent.version
            || marker.agent_sha256 != hex(&agent.sha256)
        {
            return Err(ToolsDiskError::Unsafe(format!(
                "{} does not describe the disk it sits beside",
                marker_path.display()
            )));
        }
        // Last use is the marker's mtime; eviction reads it.
        std::fs::File::open(&marker_path)?.set_modified(SystemTime::now())?;
        Ok(Some(name.clone()))
    }

    async fn build<S: RunnerSource, B: DiskBuilder>(
        &self,
        source: &S,
        builder: &B,
        name: &ImageName,
        runner: &RunnerVersion,
        agent: &AgentInput,
        staged: &Path,
    ) -> Result<(), ToolsDiskError> {
        // A leftover is removed, then the dir must be created fresh: if the
        // removal failed (another account owns it), creating fails too, and
        // nothing is built into a directory that account can write.
        let _ = tokio::fs::remove_dir_all(staged).await;
        create_fresh_dir(staged, 0o700)?;
        let src = staged.join("src");
        create_fresh_dir(&src, 0o700)?;

        let release = source
            .release(runner)
            .await?
            .ok_or_else(|| ToolsDiskError::ReleaseNotFound(runner.to_string()))?;
        let expected = published_sha256(&release.body, &release.assets, runner)?;
        fetch_verified(source, runner, &expected, &src.join(tarball_name(runner))).await?;

        let agent_path = src.join(AGENT_FILE);
        tokio::fs::write(&agent_path, &agent.bytes).await?;
        set_mode(&agent_path, 0o555)?;

        let manifest = Manifest {
            runner_version: runner.to_string(),
            runner_sha256: hex(&expected),
            agent_version: agent.version.clone(),
            agent_sha256: hex(&agent.sha256),
        };
        let manifest_json = serde_json::to_vec_pretty(&manifest)
            .map_err(|e| ToolsDiskError::Build(format!("manifest: {e}")))?;
        tokio::fs::write(src.join(MANIFEST_FILE), manifest_json).await?;
        guest_readable(&src, &tarball_name(runner))?;

        let staged_image = staged.join(format!("{}{IMAGE_SUFFIX}", name.as_str()));
        builder
            .build(&src, &staged_image)
            .await
            .map_err(|e| ToolsDiskError::Build(format!("{e:#}")))?;
        if !std::fs::symlink_metadata(&staged_image)?
            .file_type()
            .is_file()
        {
            return Err(ToolsDiskError::Build(
                "the builder left no image".to_string(),
            ));
        }
        set_mode(&staged_image, 0o444)?;

        let marker = Marker {
            kind: MARKER_KIND.to_string(),
            name: name.as_str().to_string(),
            runner_version: manifest.runner_version,
            runner_sha256: manifest.runner_sha256,
            agent_version: manifest.agent_version,
            agent_sha256: manifest.agent_sha256,
            built_unix: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
        };
        let staged_marker = staged.join(format!("{}{MARKER_SUFFIX}", name.as_str()));
        let marker_json = serde_json::to_vec(&marker)
            .map_err(|e| ToolsDiskError::Build(format!("marker: {e}")))?;
        tokio::fs::write(&staged_marker, marker_json).await?;
        set_mode(&staged_marker, 0o444)?;

        // Image first, marker last: a marker only ever stands beside a whole image.
        tokio::fs::rename(&staged_image, self.image_path(name.as_str())).await?;
        if let Err(e) = tokio::fs::rename(&staged_marker, self.marker_path(name.as_str())).await {
            // Never leave an image no marker names: eviction could not find it.
            let _ = tokio::fs::remove_file(self.image_path(name.as_str())).await;
            return Err(e.into());
        }
        // The placed entry must already pass the check every later hit applies.
        check_not_writable_by_others(&self.root)?;
        check_not_writable_by_others(&self.image_path(name.as_str()))?;
        check_not_writable_by_others(&self.marker_path(name.as_str()))?;
        Ok(())
    }

    /// Remove every faber-built disk idle for at least `max_idle` at `now` and not
    /// in `in_use`, and reap crash-orphaned staging dirs. Only an entry with a
    /// valid marker naming its own file is a candidate; a file without one is
    /// not faber's to judge, whatever it is called. Returns the evicted names.
    pub async fn evict(
        &self,
        in_use: &HashSet<ImageName>,
        now: SystemTime,
        max_idle: Duration,
    ) -> Vec<ImageName> {
        let _lock = self.build_lock.lock().await;
        reap_stale_staging(&self.staging_root(), now);
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Vec::new();
        };
        let mut evicted = Vec::new();
        for entry in entries.flatten() {
            let file_name = entry.file_name();
            let Some(stem) = file_name
                .to_str()
                .and_then(|f| f.strip_suffix(MARKER_SUFFIX))
            else {
                continue;
            };
            let Ok(name) = ImageName::try_from(stem) else {
                continue;
            };
            let marker_path = entry.path();
            let Some(marker) = read_marker(&marker_path) else {
                continue;
            };
            if marker.name != stem || in_use.contains(&name) {
                continue;
            }
            let idle = std::fs::symlink_metadata(&marker_path)
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| now.duration_since(m).ok());
            if !idle.is_some_and(|i| i >= max_idle) {
                continue;
            }
            let image = self.image_path(stem);
            match std::fs::remove_file(&image) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => continue,
            }
            if std::fs::remove_file(&marker_path).is_ok() {
                evicted.push(name);
            }
        }
        evicted
    }
}

// ---------------------------------------------------------------------------
// One job's disk
// ---------------------------------------------------------------------------

/// The tools disk one macOS VM job boots with, by name: built into `cache` on a
/// miss, reused on a hit, and refused, never rebuilt, when the entry is unsafe
/// ([`ToolsDiskCache::ensure`]'s rules, unchanged).
///
/// There is no `tools` key to get wrong: the name is derived from the runner
/// version and the agent's own bytes ([`disk_name`]). The runner is `pin` when
/// the job has one, else `profile_default`, the host's resolved runner version.
/// The agent is read fresh from `agent_path`, the file the updater writes and
/// setup installs (`UpdatePaths::agent`), so a disk is never built from an
/// agent other than the installed one.
pub async fn tools_for_job<S: RunnerSource, B: DiskBuilder>(
    cache: &ToolsDiskCache,
    source: &S,
    builder: &B,
    agent_path: &Path,
    agent_version: &str,
    pin: Option<&str>,
    profile_default: &str,
) -> Result<ImageName, ToolsDiskError> {
    let runner = runner_version_for(pin, profile_default)?;
    let (path, version) = (agent_path.to_path_buf(), agent_version.to_string());
    // Up to MAX_AGENT bytes read and hashed: off the async workers.
    let agent = tokio::task::spawn_blocking(move || match AgentInput::read(&path, &version) {
        Err(ToolsDiskError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
            Err(ToolsDiskError::NoAgent(path))
        }
        other => other,
    })
    .await
    .map_err(|e| ToolsDiskError::Build(format!("reading the agent: {e}")))??;
    cache.ensure(source, builder, &runner, &agent).await
}

/// The process's one cache, at [`default_root`]: one build lock for every job,
/// so two jobs never build the same disk at once. `None` without a state dir.
pub fn process_cache() -> Option<&'static ToolsDiskCache> {
    static CACHE: std::sync::OnceLock<Option<ToolsDiskCache>> = std::sync::OnceLock::new();
    CACHE
        .get_or_init(|| default_root().map(ToolsDiskCache::new))
        .as_ref()
}

/// [`tools_for_job`] on this host: the process cache, the installed agent at
/// this binary's version (setup and the updater install the two from one
/// release), the runner from GitHub, and `hdiutil`. No job pins an
/// `actions/runner` version today, so the runner is the host's.
pub async fn for_this_host(
    gh: &Github,
    profile_default: &str,
) -> Result<ImageName, ToolsDiskError> {
    let cache = process_cache()
        .ok_or_else(|| ToolsDiskError::Build("this process has no state directory".into()))?;
    let agent = crate::update::UpdatePaths::from_home()
        .ok_or_else(|| ToolsDiskError::Build("this process has no state directory".into()))?
        .agent();
    let source = GithubRunnerSource::new(gh)?;
    tools_for_job(
        cache,
        &source,
        &Hdiutil,
        &agent,
        env!("FABER_VERSION"),
        None,
        profile_default,
    )
    .await
}

/// Best-effort: remove `.staging/` entries older than [`STAGING_ORPHAN_AGE`].
fn reap_stale_staging(staging_root: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(staging_root) else {
        return;
    };
    for entry in entries.flatten() {
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| now.duration_since(m).ok())
            .is_some_and(|age| age >= STAGING_ORPHAN_AGE);
        if old {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    /// A throwaway directory removed (with its contents) on drop — the repo's
    /// hand-rolled test-scratch pattern (mirrors `TempDir` in `runner.rs`).
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new() -> Self {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "faber-tools-disk-test-{}-{}",
                std::process::id(),
                TMP_SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            // Read-only files inside a writable dir are removable; restore the
            // dir modes anyway in case a test narrowed one.
            let _ = set_mode(&self.path, 0o755);
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn v(s: &str) -> RunnerVersion {
        RunnerVersion::parse(s).unwrap()
    }

    const TARBALL: &[u8] = b"pretend this is a runner tarball";

    fn sha(bytes: &[u8]) -> [u8; 32] {
        Sha256::digest(bytes).into()
    }

    fn body_for(hash: &str) -> String {
        format!(
            "## Changes\n<!-- BEGIN SHA win-x64 -->{}<!-- END SHA win-x64 -->\n\
             <!-- BEGIN SHA osx-arm64 -->{hash}<!-- END SHA osx-arm64 -->\n",
            "0".repeat(64)
        )
    }

    struct FakeSource {
        body: String,
        assets: Vec<PublishedAsset>,
        served: Vec<u8>,
        chunk: usize,
        downloads: AtomicUsize,
    }

    impl FakeSource {
        fn honest() -> Self {
            Self {
                body: body_for(&hex(&sha(TARBALL))),
                assets: vec![],
                served: TARBALL.to_vec(),
                chunk: 7,
                downloads: AtomicUsize::new(0),
            }
        }
    }

    struct FakeDownload(Vec<Vec<u8>>);

    impl Download for FakeDownload {
        async fn chunk(&mut self) -> Result<Option<Vec<u8>>, ToolsDiskError> {
            Ok(if self.0.is_empty() {
                None
            } else {
                Some(self.0.remove(0))
            })
        }
    }

    impl RunnerSource for FakeSource {
        type Download = FakeDownload;
        async fn release(
            &self,
            _: &RunnerVersion,
        ) -> Result<Option<RunnerRelease>, ToolsDiskError> {
            Ok(Some(RunnerRelease {
                body: self.body.clone(),
                assets: self.assets.clone(),
            }))
        }
        async fn download(&self, _: &RunnerVersion) -> Result<FakeDownload, ToolsDiskError> {
            self.downloads.fetch_add(1, Ordering::Relaxed);
            Ok(FakeDownload(
                self.served.chunks(self.chunk).map(<[u8]>::to_vec).collect(),
            ))
        }
    }

    /// Serves `len` zero bytes in 1 MiB chunks without holding them.
    struct Huge(u64);

    impl Download for Huge {
        async fn chunk(&mut self) -> Result<Option<Vec<u8>>, ToolsDiskError> {
            if self.0 == 0 {
                return Ok(None);
            }
            let n = self.0.min(1 << 20);
            self.0 -= n;
            Ok(Some(vec![0u8; n as usize]))
        }
    }

    struct HugeSource;

    impl RunnerSource for HugeSource {
        type Download = Huge;
        async fn release(
            &self,
            _: &RunnerVersion,
        ) -> Result<Option<RunnerRelease>, ToolsDiskError> {
            Ok(None)
        }
        async fn download(&self, _: &RunnerVersion) -> Result<Huge, ToolsDiskError> {
            Ok(Huge(MAX_TARBALL + 1))
        }
    }

    /// Concatenates the staged files into `out`, in name order, and counts calls.
    #[derive(Default)]
    struct FakeBuilder {
        calls: AtomicUsize,
        /// The source folder's mode and each file's, as `hdiutil` would copy them.
        modes: std::sync::Mutex<Vec<(String, u32)>>,
    }

    impl DiskBuilder for FakeBuilder {
        async fn build(&self, src_dir: &Path, out: &Path) -> anyhow::Result<()> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            {
                let mut modes = self.modes.lock().unwrap();
                modes.push((".".into(), mode(src_dir)));
                for e in std::fs::read_dir(src_dir)? {
                    let e = e?;
                    modes.push((
                        e.file_name().to_string_lossy().into_owned(),
                        mode(&e.path()),
                    ));
                }
            }
            let mut names: Vec<_> = std::fs::read_dir(src_dir)?
                .map(|e| e.map(|e| e.file_name()))
                .collect::<Result<_, _>>()?;
            names.sort();
            let mut all = Vec::new();
            for n in names {
                all.extend(n.to_string_lossy().as_bytes());
                all.extend(std::fs::read(src_dir.join(&n))?);
            }
            std::fs::write(out, all)?;
            Ok(())
        }
    }

    fn agent(dir: &Path, version: &str, bytes: &[u8]) -> AgentInput {
        let p = dir.join(format!("agent-{}", TMP_SEQ.fetch_add(1, Ordering::Relaxed)));
        std::fs::write(&p, bytes).unwrap();
        AgentInput::read(&p, version).unwrap()
    }

    fn mode(p: &Path) -> u32 {
        std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
    }

    // --- version choice ---

    #[test]
    fn the_pin_wins_and_loses_its_v() {
        assert_eq!(
            runner_version_for(Some("2.330.0"), "2.329.0").unwrap(),
            v("2.330.0")
        );
        assert_eq!(
            runner_version_for(Some("v2.330.0"), "2.329.0").unwrap(),
            v("2.330.0")
        );
        assert_eq!(runner_version_for(None, "v2.329.0").unwrap(), v("2.329.0"));
        assert!(RunnerVersion::parse("vv2.330.0").is_none());
    }

    #[test]
    fn an_invalid_pin_is_refused_not_defaulted_and_not_quoted() {
        let pin = "2.330.0;rm-rf";
        let err = runner_version_for(Some(pin), "2.329.0").unwrap_err();
        assert!(matches!(err, ToolsDiskError::InvalidPin { len } if len == pin.len()));
        let msg = err.to_string();
        assert!(!msg.contains("rm-rf") && !msg.contains("2.330.0"), "{msg}");
        assert!(msg.contains(&format!("{} bytes", pin.len())));
    }

    #[test]
    fn an_invalid_default_is_refused() {
        assert!(matches!(
            runner_version_for(None, "latest"),
            Err(ToolsDiskError::InvalidDefault { len: 6 })
        ));
    }

    // --- the published hash ---

    fn asset(digest: Option<String>) -> PublishedAsset {
        PublishedAsset {
            name: tarball_name(&v("2.330.0")),
            digest,
        }
    }

    #[test]
    fn the_body_hash_is_read() {
        let h = sha(TARBALL);
        assert_eq!(
            published_sha256(&body_for(&hex(&h)), &[], &v("2.330.0")).unwrap(),
            h
        );
        // Uppercase hex is the same hash.
        let upper = hex(&h).to_uppercase();
        assert_eq!(
            published_sha256(&body_for(&upper), &[], &v("2.330.0")).unwrap(),
            h
        );
    }

    #[test]
    fn a_missing_duplicated_or_malformed_body_hash_is_refused() {
        let h = hex(&sha(TARBALL));
        let ver = v("2.330.0");
        assert!(published_sha256("no hashes here", &[], &ver).is_err());
        let twice = format!("{}{}", body_for(&h), body_for(&h));
        assert!(published_sha256(&twice, &[], &ver).is_err());
        assert!(published_sha256(&body_for(&h[..63]), &[], &ver).is_err());
        let not_hex = format!("{}z", &h[..63]);
        assert!(published_sha256(&body_for(&not_hex), &[], &ver).is_err());
        assert!(published_sha256(&body_for(&format!(" {h}")), &[], &ver).is_err());
        let long = format!("{}{}", body_for(&h), " ".repeat(MAX_BODY));
        assert!(published_sha256(&long, &[], &ver).is_err());
    }

    #[test]
    fn the_asset_digest_must_agree_when_present() {
        let h = hex(&sha(TARBALL));
        let ver = v("2.330.0");
        let body = body_for(&h);
        assert!(published_sha256(&body, &[asset(Some(format!("sha256:{h}")))], &ver).is_ok());
        assert!(published_sha256(&body, &[asset(None)], &ver).is_ok());
        let other = hex(&sha(b"other"));
        let err =
            published_sha256(&body, &[asset(Some(format!("sha256:{other}")))], &ver).unwrap_err();
        assert!(!err.to_string().contains(&other));
        assert!(published_sha256(&body, &[asset(Some(h.clone()))], &ver).is_err());
        let dup = [asset(None), asset(None)];
        assert!(published_sha256(&body, &dup, &ver).is_err());
    }

    // --- download ---

    #[tokio::test]
    async fn matching_bytes_are_kept() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("t.tar.gz");
        fetch_verified(&FakeSource::honest(), &v("2.330.0"), &sha(TARBALL), &dest)
            .await
            .unwrap();
        assert_eq!(std::fs::read(&dest).unwrap(), TARBALL);
    }

    #[tokio::test]
    async fn tampered_bytes_are_refused_and_removed() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("t.tar.gz");
        let mut src = FakeSource::honest();
        src.served = b"pretend this is a runner tarbalL".to_vec();
        let err = fetch_verified(&src, &v("2.330.0"), &sha(TARBALL), &dest)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolsDiskError::HashMismatch { .. }));
        assert!(!dest.exists());
    }

    #[tokio::test]
    async fn an_oversize_tarball_is_refused() {
        let tmp = TempDir::new();
        let dest = tmp.path().join("t.tar.gz");
        let err = fetch_verified(&HugeSource, &v("2.330.0"), &[0; 32], &dest)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolsDiskError::TooLarge { .. }));
        assert!(!dest.exists());
    }

    // --- agent and name ---

    #[test]
    fn the_agent_must_be_a_regular_file() {
        let tmp = TempDir::new();
        let real = tmp.path().join("agent");
        std::fs::write(&real, b"x").unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert!(matches!(
            AgentInput::read(&link, "0.4.0"),
            Err(ToolsDiskError::Agent(_))
        ));
        assert!(matches!(
            AgentInput::read(tmp.path(), "0.4.0"),
            Err(ToolsDiskError::Agent(_))
        ));
        assert_eq!(
            AgentInput::read(&real, "0.4.0").unwrap().sha256(),
            &sha(b"x")
        );
    }

    #[test]
    fn the_name_is_deterministic_and_follows_the_agent_bytes() {
        let tmp = TempDir::new();
        let a = agent(tmp.path(), "0.4.0", b"agent one");
        let a2 = agent(tmp.path(), "0.4.0", b"agent one");
        let b = agent(tmp.path(), "0.4.0", b"agent two");
        let n = disk_name(&v("2.330.0"), &a).unwrap();
        assert_eq!(n, disk_name(&v("2.330.0"), &a2).unwrap());
        assert_ne!(n, disk_name(&v("2.330.0"), &b).unwrap());
        let expected = format!(
            "runner-2.330.0-agent-0.4.0-{}",
            &hex(&sha(b"agent one"))[..8]
        );
        assert_eq!(n.as_str(), expected);
        assert!(ImageName::try_from(n.as_str()).is_ok());
    }

    #[test]
    fn the_hdiutil_cdr_path_appends_to_the_dotted_name() {
        let out = Path::new("/var/cache/runner-2.330.0-agent-0.4.0-abcd1234.img");
        let (base, cdr) = hdiutil_paths(out);
        assert_eq!(
            base,
            Path::new("/var/cache/runner-2.330.0-agent-0.4.0-abcd1234")
        );
        assert_eq!(
            cdr,
            Path::new("/var/cache/runner-2.330.0-agent-0.4.0-abcd1234.cdr")
        );
    }

    #[test]
    fn build_metadata_maps_and_bad_versions_are_refused() {
        let tmp = TempDir::new();
        let a = agent(tmp.path(), "0.4.0+gabc1234", b"a");
        assert!(disk_name(&v("2.330.0"), &a)
            .unwrap()
            .as_str()
            .starts_with("runner-2.330.0-agent-0.4.0.gabc1234-"));
        for bad in ["0.4.0/x", "0.4.0 x", "0.4.0-RC", ""] {
            let a = agent(tmp.path(), bad, b"a");
            assert!(
                matches!(disk_name(&v("2.330.0"), &a), Err(ToolsDiskError::Name(_))),
                "{bad:?}"
            );
        }
        let a = agent(tmp.path(), &"1".repeat(40), b"a");
        assert!(matches!(
            disk_name(&v("2.330.0"), &a),
            Err(ToolsDiskError::Name(_))
        ));
    }

    // --- build and cache ---

    #[tokio::test]
    async fn a_second_ensure_is_a_hit() {
        let tmp = TempDir::new();
        let cache = ToolsDiskCache::new(tmp.path().join("tools-disks"));
        let src = FakeSource::honest();
        let builder = FakeBuilder::default();
        let ag = agent(tmp.path(), "0.4.0", b"agent");
        let n1 = cache
            .ensure(&src, &builder, &v("2.330.0"), &ag)
            .await
            .unwrap();
        let n2 = cache
            .ensure(&src, &builder, &v("2.330.0"), &ag)
            .await
            .unwrap();
        assert_eq!(n1, n2);
        assert_eq!(builder.calls.load(Ordering::Relaxed), 1);
        assert_eq!(src.downloads.load(Ordering::Relaxed), 1);

        let img = cache.image_path(n1.as_str());
        let marker = cache.marker_path(n1.as_str());
        assert_eq!(mode(cache.root()), 0o755);
        assert_eq!(mode(&img), 0o444);
        assert_eq!(mode(&marker), 0o444);
        // The disk holds exactly the tarball, the agent and the manifest.
        let content = std::fs::read(&img).unwrap();
        let text = String::from_utf8_lossy(&content);
        assert!(text.starts_with("actions-runner-osx-arm64-2.330.0.tar.gz"));
        assert!(text.contains(AGENT_FILE) && text.contains(MANIFEST_FILE));
        assert!(text.contains(&hex(&sha(TARBALL))));
        // Nothing left in staging.
        assert_eq!(std::fs::read_dir(cache.staging_root()).unwrap().count(), 0);
    }

    /// The guest's `runner` is "other" to every file on the disk: the folder the
    /// volume's root copies must be searchable, the agent executable, the tarball
    /// and the manifest readable, whatever the umask that wrote them.
    #[tokio::test]
    async fn the_disk_is_built_from_a_tree_runner_can_read() {
        let tmp = TempDir::new();
        let cache = ToolsDiskCache::new(tmp.path().join("tools-disks"));
        let (src, builder) = (FakeSource::honest(), FakeBuilder::default());
        let ag = agent(tmp.path(), "0.4.0", b"agent");
        cache
            .ensure(&src, &builder, &v("2.330.0"), &ag)
            .await
            .unwrap();
        let mut modes = builder.modes.lock().unwrap().clone();
        modes.sort();
        assert_eq!(
            modes,
            vec![
                (".".to_string(), 0o755),
                ("actions-runner-osx-arm64-2.330.0.tar.gz".to_string(), 0o444),
                (AGENT_FILE.to_string(), 0o555),
                (MANIFEST_FILE.to_string(), 0o444),
            ]
        );
    }

    // --- one job's disk (#260) ---

    /// The installed agent at `D/bin/faber-agent`, as setup and the updater
    /// leave it.
    fn installed_agent(tmp: &TempDir, bytes: &[u8]) -> PathBuf {
        let bin = tmp.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let p = bin.join(AGENT_FILE);
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[tokio::test]
    async fn a_job_on_a_miss_builds_its_disk_from_the_installed_agent() {
        let tmp = TempDir::new();
        let cache = ToolsDiskCache::new(tmp.path().join("tools-disks"));
        let (src, builder) = (FakeSource::honest(), FakeBuilder::default());
        let agent_path = installed_agent(&tmp, b"the installed agent");
        let name = tools_for_job(
            &cache,
            &src,
            &builder,
            &agent_path,
            "0.4.0+gabc1234",
            None,
            "2.330.0",
        )
        .await
        .unwrap();
        let want = format!(
            "runner-2.330.0-agent-0.4.0.gabc1234-{}",
            &hex(&sha(b"the installed agent"))[..8]
        );
        assert_eq!(name.as_str(), want, "derived, never configured");
        assert_eq!(builder.calls.load(Ordering::Relaxed), 1);
        let disk = std::fs::read(cache.image_path(name.as_str())).unwrap();
        assert!(
            disk.windows(19).any(|w| w == b"the installed agent"),
            "the disk carries the installed agent's bytes"
        );
    }

    #[tokio::test]
    async fn a_job_on_a_hit_reuses_the_disk_and_downloads_nothing() {
        let tmp = TempDir::new();
        let cache = ToolsDiskCache::new(tmp.path().join("tools-disks"));
        let (src, builder) = (FakeSource::honest(), FakeBuilder::default());
        let agent_path = installed_agent(&tmp, b"agent");
        let job = || {
            tools_for_job(
                &cache,
                &src,
                &builder,
                &agent_path,
                "0.4.0",
                None,
                "2.330.0",
            )
        };
        let first = job().await.unwrap();
        let second = job().await.unwrap();
        assert_eq!(first, second);
        assert_eq!(builder.calls.load(Ordering::Relaxed), 1);
        assert_eq!(src.downloads.load(Ordering::Relaxed), 1);

        // A new agent (an update) is a new disk, never the old one reused.
        std::fs::write(&agent_path, b"agent, updated").unwrap();
        let third = job().await.unwrap();
        assert_ne!(third, first);
        assert_eq!(builder.calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn a_job_refuses_a_tampered_cache_and_builds_nothing_over_it() {
        let tmp = TempDir::new();
        let cache = ToolsDiskCache::new(tmp.path().join("tools-disks"));
        let (src, builder) = (FakeSource::honest(), FakeBuilder::default());
        let agent_path = installed_agent(&tmp, b"agent");
        let job = || {
            tools_for_job(
                &cache,
                &src,
                &builder,
                &agent_path,
                "0.4.0",
                None,
                "2.330.0",
            )
        };
        let name = job().await.unwrap();
        // Something other than the daemon made the image writable to others.
        set_mode(&cache.image_path(name.as_str()), 0o666).unwrap();
        assert!(matches!(job().await, Err(ToolsDiskError::Unsafe(_))));
        assert_eq!(
            builder.calls.load(Ordering::Relaxed),
            1,
            "not rebuilt over it"
        );
    }

    #[tokio::test]
    async fn a_job_without_an_installed_agent_says_how_to_install_one() {
        let tmp = TempDir::new();
        let cache = ToolsDiskCache::new(tmp.path().join("tools-disks"));
        let (src, builder) = (FakeSource::honest(), FakeBuilder::default());
        let missing = tmp.path().join("bin").join(AGENT_FILE);
        let err = tools_for_job(&cache, &src, &builder, &missing, "0.4.0", None, "2.330.0")
            .await
            .unwrap_err();
        assert!(matches!(err, ToolsDiskError::NoAgent(_)), "{err}");
        assert!(err.to_string().contains("sudo faber setup"), "{err}");
        assert_eq!(builder.calls.load(Ordering::Relaxed), 0);
        // And an invalid host runner version is refused before anything is read.
        let err = tools_for_job(&cache, &src, &builder, &missing, "0.4.0", None, "latest")
            .await
            .unwrap_err();
        assert!(
            matches!(err, ToolsDiskError::InvalidDefault { .. }),
            "{err}"
        );
    }

    #[tokio::test]
    async fn a_marker_without_its_image_is_dropped_and_rebuilt() {
        let tmp = TempDir::new();
        let cache = ToolsDiskCache::new(tmp.path().join("tools-disks"));
        let src = FakeSource::honest();
        let builder = FakeBuilder::default();
        let ag = agent(tmp.path(), "0.4.0", b"agent");
        let n = cache
            .ensure(&src, &builder, &v("2.330.0"), &ag)
            .await
            .unwrap();
        // Eviction removed the image, then failed to remove the marker.
        std::fs::remove_file(cache.image_path(n.as_str())).unwrap();
        assert!(cache.marker_path(n.as_str()).exists());
        let again = cache
            .ensure(&src, &builder, &v("2.330.0"), &ag)
            .await
            .unwrap();
        assert_eq!(again, n);
        assert_eq!(builder.calls.load(Ordering::Relaxed), 2);
        assert!(cache.image_path(n.as_str()).exists());
        assert!(cache.marker_path(n.as_str()).exists());
    }

    #[test]
    fn directories_are_created_with_their_mode_and_a_fresh_one_must_be_new() {
        let tmp = TempDir::new();
        let root = tmp.path().join("a").join("b");
        create_dir_mode(&root, 0o755).unwrap();
        assert_eq!(mode(&root), 0o755);
        // An existing cache dir is accepted.
        create_dir_mode(&root, 0o755).unwrap();
        let fresh = tmp.path().join("fresh");
        create_fresh_dir(&fresh, 0o700).unwrap();
        assert_eq!(mode(&fresh), 0o700);
        let err = create_fresh_dir(&fresh, 0o700).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
    }

    #[tokio::test]
    async fn a_hash_mismatch_builds_nothing() {
        let tmp = TempDir::new();
        let cache = ToolsDiskCache::new(tmp.path().join("tools-disks"));
        let mut src = FakeSource::honest();
        src.served = b"tampered".to_vec();
        let builder = FakeBuilder::default();
        let ag = agent(tmp.path(), "0.4.0", b"agent");
        let err = cache
            .ensure(&src, &builder, &v("2.330.0"), &ag)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolsDiskError::HashMismatch { .. }));
        assert_eq!(builder.calls.load(Ordering::Relaxed), 0);
        assert_eq!(std::fs::read_dir(cache.staging_root()).unwrap().count(), 0);
    }

    #[tokio::test]
    async fn a_group_writable_image_is_refused_not_rebuilt() {
        let tmp = TempDir::new();
        let cache = ToolsDiskCache::new(tmp.path().join("tools-disks"));
        let src = FakeSource::honest();
        let builder = FakeBuilder::default();
        let ag = agent(tmp.path(), "0.4.0", b"agent");
        let n = cache
            .ensure(&src, &builder, &v("2.330.0"), &ag)
            .await
            .unwrap();
        set_mode(&cache.image_path(n.as_str()), 0o464).unwrap();
        let err = cache
            .ensure(&src, &builder, &v("2.330.0"), &ag)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolsDiskError::Unsafe(_)));
        assert_eq!(builder.calls.load(Ordering::Relaxed), 1);

        set_mode(&cache.image_path(n.as_str()), 0o444).unwrap();
        set_mode(cache.root(), 0o775).unwrap();
        assert!(cache
            .ensure(&src, &builder, &v("2.330.0"), &ag)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_mismatched_marker_is_refused() {
        let tmp = TempDir::new();
        let cache = ToolsDiskCache::new(tmp.path().join("tools-disks"));
        let src = FakeSource::honest();
        let builder = FakeBuilder::default();
        let ag = agent(tmp.path(), "0.4.0", b"agent");
        let n = cache
            .ensure(&src, &builder, &v("2.330.0"), &ag)
            .await
            .unwrap();
        let mp = cache.marker_path(n.as_str());
        let mut m = read_marker(&mp).unwrap();
        m.agent_sha256 = "0".repeat(64);
        set_mode(&mp, 0o644).unwrap();
        std::fs::write(&mp, serde_json::to_vec(&m).unwrap()).unwrap();
        set_mode(&mp, 0o444).unwrap();
        let err = cache
            .ensure(&src, &builder, &v("2.330.0"), &ag)
            .await
            .unwrap_err();
        assert!(matches!(err, ToolsDiskError::Unsafe(_)));
        assert_eq!(builder.calls.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn the_safety_check_passes_own_files_and_refuses_shared_write() {
        let tmp = TempDir::new();
        let f = tmp.path().join("f");
        std::fs::write(&f, b"x").unwrap();
        set_mode(&f, 0o444).unwrap();
        set_mode(tmp.path(), 0o755).unwrap();
        // Created by this process, so owned by its euid.
        check_not_writable_by_others(&f).unwrap();
        check_not_writable_by_others(tmp.path()).unwrap();
        for bad in [0o464, 0o446] {
            set_mode(&f, bad).unwrap();
            assert!(matches!(
                check_not_writable_by_others(&f),
                Err(ToolsDiskError::Unsafe(_))
            ));
        }
        let link = tmp.path().join("l");
        std::os::unix::fs::symlink(&f, &link).unwrap();
        assert!(matches!(
            check_not_writable_by_others(&link),
            Err(ToolsDiskError::Unsafe(_))
        ));
    }

    // --- eviction ---

    #[tokio::test]
    async fn eviction_reads_markers_not_names() {
        let tmp = TempDir::new();
        let cache = ToolsDiskCache::new(tmp.path().join("tools-disks"));
        let src = FakeSource::honest();
        let builder = FakeBuilder::default();
        let idle = cache
            .ensure(
                &src,
                &builder,
                &v("2.330.0"),
                &agent(tmp.path(), "0.4.0", b"one"),
            )
            .await
            .unwrap();
        let busy = cache
            .ensure(
                &src,
                &builder,
                &v("2.330.0"),
                &agent(tmp.path(), "0.4.0", b"two"),
            )
            .await
            .unwrap();

        // A look-alike with no marker, and one whose marker names another file.
        let stray = cache.root().join("runner-2.330.0-agent-0.4.0-deadbeef.img");
        std::fs::write(&stray, b"not ours").unwrap();
        let mut foreign = read_marker(&cache.marker_path(idle.as_str())).unwrap();
        foreign.name = "runner-other".to_string();
        let fm = cache
            .root()
            .join(format!("runner-mislabelled{MARKER_SUFFIX}"));
        let fi = cache.root().join("runner-mislabelled.img");
        std::fs::write(&fm, serde_json::to_vec(&foreign).unwrap()).unwrap();
        std::fs::write(&fi, b"x").unwrap();

        // A staging orphan, aged past the reap threshold by evicting "later".
        let orphan = cache.staging_root().join("orphan");
        std::fs::create_dir_all(&orphan).unwrap();

        let later = SystemTime::now() + Duration::from_secs(2 * 3600);
        let in_use: HashSet<_> = [busy.clone()].into();
        let evicted = cache.evict(&in_use, later, Duration::from_secs(3600)).await;
        assert_eq!(evicted, vec![idle.clone()]);
        assert!(!cache.image_path(idle.as_str()).exists());
        assert!(!cache.marker_path(idle.as_str()).exists());
        assert!(cache.image_path(busy.as_str()).exists());
        assert!(stray.exists());
        assert!(fm.exists() && fi.exists());
        assert!(!orphan.exists());

        // Not idle long enough: nothing goes.
        let none = cache
            .evict(
                &HashSet::new(),
                SystemTime::now(),
                Duration::from_secs(3600),
            )
            .await;
        assert!(none.is_empty());
    }
}
