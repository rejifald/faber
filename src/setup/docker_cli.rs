//! The Docker CLI the SYSTEM daemon runs on macOS (ADR-0026, amended
//! 2026-10-21): a pinned static build, installed by `sudo faber setup` at
//! [`DOCKER_CLI`], `root:wheel` 0755.
//!
//! The daemon's `PATH` holds nothing an operator can write (AGENTS.md), and the
//! only `docker` a Mac usually has is Docker Desktop's, under `/usr/local/bin`
//! and `/Applications`, which is the operator's, not faber's. So the daemon
//! runs this one by absolute path ([`crate::platform::docker_cli`]); a per-user
//! (LEGACY) faber and Linux keep `docker` from `PATH`.
//!
//! The CLI itself executes one more kind of program: a CLI plugin, such as
//! `buildx` for `docker build`. It looks for plugins in `_faber`'s own
//! `~/.docker/cli-plugins` (its config directory, `_faber`'s to write) and in
//! the fixed system directories [`PLUGIN_DIRS`]. So the daemon runs the pinned
//! CLI and, for `docker build`, only plugins from those system directories
//! while each is root's alone (none exists on a stock Mac), or from `_faber`'s
//! own home. Setup refuses a host where one of them exists and is not root's
//! alone ([`plugin_dir_steps`]).
//!
//! # The pin
//!
//! [`PIN`] names one release of Docker's static macOS build: its URL, the
//! archive's SHA-256 and size, and the SHA-256 and size of the one member
//! setup keeps, `docker/docker`. Docker signs that binary ad hoc only (no team
//! ID), so no signature says who built it: the archive's SHA-256, fetched over
//! HTTPS, is the trust anchor, and the binary's own SHA-256 is checked again
//! after unpacking. Moving to another Docker release is a source change to
//! [`PIN`], reviewed like the Debian base's ([`super::linux_image::BASE`]).
//!
//! # What setup does
//!
//! 1. Plans `ok` when the file at [`DOCKER_CLI`] is the pinned binary,
//!    `root:wheel` 0755, and `will create` / `will change` otherwise. Every
//!    directory above it must be root's and writable by nobody else, or the
//!    step is refused: the daemon runs the file, and whoever could rename a
//!    directory on its path could choose what it runs.
//! 2. Downloads the archive over HTTPS only, refusing anything longer than the
//!    pinned size, and checks its SHA-256 before anything reads it.
//! 3. Decompresses it (`/usr/bin/gzip -dc`, on bytes whose digest is already
//!    the pin's) and walks its tar headers itself ([`binary_from_tar`]): the
//!    one regular member `docker/docker` and its directory, and nothing else:
//!    no other member, no link, no absolute or `..` path.
//! 4. Checks the binary's SHA-256, stages it `root:wheel` 0755 beside its
//!    destination, runs the staged copy's `--version` as `_faber` (never as
//!    root), and renames it into place.
//!
//! The pin is Apple-silicon only: on another Mac the step refuses rather than
//! install a binary that cannot run.
//!
//! `sudo faber uninstall` removes the file and, when nothing else is in it,
//! its directory, through descriptors walked from `/` that refuse a directory
//! root does not own alone.

#[cfg(target_os = "macos")]
use std::path::Path;

use std::path::PathBuf;

use crate::platform::layout::{DOCKER_CLI, FABER_ACCOUNT};

use super::exe::PathFacts;
use super::linux_image::{BuildError, Chunks};
use super::plan::{step, unknown, Action, BinProbe, HostState, Section, Status, Step};
use super::vm_runtime::VmAction;

/// One pinned release of Docker's static macOS CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DockerCliPin {
    /// What `docker --version` reports: `Docker version <version>, build …`.
    pub version: &'static str,
    /// The archive, a gzipped tar.
    pub url: &'static str,
    /// The archive's SHA-256, lowercase hex.
    pub sha256: &'static str,
    /// The archive's length in bytes.
    pub size: u64,
    /// The one member setup keeps.
    pub member: &'static str,
    /// That member's SHA-256, lowercase hex.
    pub binary_sha256: &'static str,
    /// That member's length in bytes.
    pub binary_size: u64,
}

impl DockerCliPin {
    /// The member's directory, which the archive also lists: `docker/`.
    fn member_dir(&self) -> &'static str {
        self.member.rsplit_once('/').map_or("", |(dir, _)| dir)
    }

    /// The most the archive may unpack to: the binary, its headers, padding
    /// and the end-of-archive blocks, with room to spare.
    pub fn max_tar(&self) -> u64 {
        self.binary_size + (1 << 20)
    }
}

/// This release's pin: Docker 29.9.2, the version of the Linux VM's Docker
/// Engine.
///
/// `binary_sha256` and `binary_size` were read from the archive below, once
/// its SHA-256 matched `sha256`: `gzip -dc` it, `tar -xf` the result, then
/// `shasum -a 256` and `stat -f %z` on `docker/docker`. Re-derive both the same
/// way when bumping the pin.
pub const PIN: DockerCliPin = DockerCliPin {
    version: "29.9.2",
    url: "https://download.docker.com/mac/static/stable/aarch64/docker-29.9.2.tgz",
    sha256: "ea8a2ce5ac31f9f5f3fb711a7a485e1e46cc7a69798a629a70306f6e6af25b7a",
    size: 19_654_797,
    member: "docker/docker",
    binary_sha256: "cbcda5a0eb03c42037e90fff45ceb78763c8f80509c0f5009c117e0ef7779b96",
    binary_size: 43_033_906,
};

/// The system directories the pinned CLI executes plugins from, besides its
/// config directory's `cli-plugins` (`_faber`'s own): docker/cli's
/// `defaultSystemPluginDirs` in `cli-plugins/manager/manager_unix.go`, the list
/// every non-Windows build uses. Confirmed against the pinned 29.9.2 binary,
/// whose strings hold these four and no other `cli-plugins` path. Re-check it
/// when bumping [`PIN`].
pub const PLUGIN_DIRS: [&str; 4] = [
    "/usr/local/lib/docker/cli-plugins",
    "/usr/local/libexec/docker/cli-plugins",
    "/usr/lib/docker/cli-plugins",
    "/usr/libexec/docker/cli-plugins",
];

/// What the plan knows of the installed CLI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DockerCliProbe {
    /// The file at [`DOCKER_CLI`].
    pub installed: BinProbe,
    /// Each directory on its path that exists, from `/` down, its own
    /// directory included.
    pub parents: Result<Vec<PathFacts>, String>,
    /// Whether this host can run the pinned build: an Apple-silicon Mac.
    pub apple_silicon: bool,
    /// Each of [`PLUGIN_DIRS`], present or not ([`probe_plugin_dir`]). Empty
    /// when not probed.
    pub plugin_dirs: Vec<PluginDirProbe>,
}

impl DockerCliProbe {
    /// Nothing installed, and nothing known against it: a fresh
    /// Apple-silicon host whose plugin directories were not probed.
    pub fn nothing() -> Self {
        Self {
            installed: BinProbe::Missing,
            parents: Ok(Vec::new()),
            apple_silicon: true,
            plugin_dirs: Vec::new(),
        }
    }
}

/// What one entry of a plugin directory is, from `lstat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Symlink,
    Dir,
    /// A FIFO, socket, device.
    Other,
}

/// One entry of a plugin directory, as `lstat` reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryFacts {
    pub name: String,
    pub kind: EntryKind,
    pub uid: u32,
    /// The full `st_mode`.
    pub mode: u32,
    pub nlink: u64,
}

/// One of [`PLUGIN_DIRS`], as found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PluginDirProbe {
    pub dir: PathBuf,
    /// Every component of `dir` that exists, from `/` down: up to the last
    /// existing ancestor when it is absent, `dir` itself when present.
    pub chain: Result<Vec<PathFacts>, String>,
    /// Its entries, `None` when `dir` is not there (or not a directory, which
    /// `chain` refuses).
    pub entries: Result<Option<Vec<EntryFacts>>, String>,
}

/// The most entries a plugin directory may hold before it is refused unread.
pub const MAX_PLUGIN_ENTRIES: usize = 256;

/// Why the CLI must not load plugins from `p`, `None` when it may: every
/// existing directory on its path, absent or not, is root's alone (so no other
/// account can create it later, or rename one above it), and every entry in it
/// is a regular file root owns, writable by nobody else, with one link. A
/// symlink is refused whatever it points at: Docker Desktop's system install
/// puts symlinks into `/Applications/Docker.app` here. `Err` when it could not
/// be read. Pure.
pub fn plugin_dir_problem(p: &PluginDirProbe) -> Result<Option<String>, String> {
    let chain = p.chain.as_ref().map_err(Clone::clone)?;
    if let Some(problem) = parents_problem(chain) {
        return Ok(Some(problem));
    }
    let Some(entries) = p.entries.as_ref().map_err(Clone::clone)? else {
        return Ok(None);
    };
    Ok(entries.iter().find_map(|e| {
        let at = p.dir.join(&e.name);
        let at = at.display();
        match e.kind {
            EntryKind::Symlink => Some(format!("{at} is a symlink")),
            EntryKind::Dir => Some(format!("{at} is a directory")),
            EntryKind::Other => Some(format!("{at} is not a regular file")),
            EntryKind::File if e.uid != 0 => {
                Some(format!("{at} is owned by uid {}, not root", e.uid))
            }
            EntryKind::File if e.mode & 0o022 != 0 => Some(format!(
                "{at} is group- or world-writable (mode {:o})",
                e.mode & 0o7777
            )),
            EntryKind::File if e.nlink != 1 => {
                Some(format!("{at} has {} hard links, not one", e.nlink))
            }
            EntryKind::File => None,
        }
    }))
}

/// Read one plugin directory: the chain from `/` to it, without following a
/// symlink, and, when it is a real directory, each entry's `lstat`.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn probe_plugin_dir(dir: &std::path::Path) -> PluginDirProbe {
    use std::os::unix::fs::MetadataExt;
    let mut chain = Vec::new();
    let mut at = PathBuf::from("/");
    let mut parts = vec![at.clone()];
    for c in dir.components().skip(1) {
        at.push(c);
        parts.push(at.clone());
    }
    let mut chain_err = None;
    let mut present = false;
    for (i, path) in parts.iter().enumerate() {
        match std::fs::symlink_metadata(path) {
            Ok(m) => {
                present = i + 1 == parts.len() && m.file_type().is_dir();
                chain.push(PathFacts {
                    path: path.clone(),
                    uid: m.uid(),
                    gid: m.gid(),
                    mode: m.mode(),
                });
                if !m.file_type().is_dir() {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
            Err(e) => {
                chain_err = Some(format!("{}: {e}", path.display()));
                break;
            }
        }
    }
    let entries = if !present {
        Ok(None)
    } else {
        (|| {
            let mut out = Vec::new();
            for entry in std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))? {
                let entry = entry.map_err(|e| format!("{}: {e}", dir.display()))?;
                if out.len() == MAX_PLUGIN_ENTRIES {
                    return Err(format!(
                        "{} holds more than {MAX_PLUGIN_ENTRIES} entries",
                        dir.display()
                    ));
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let m = std::fs::symlink_metadata(entry.path())
                    .map_err(|e| format!("{}: {e}", entry.path().display()))?;
                let t = m.file_type();
                let kind = if t.is_symlink() {
                    EntryKind::Symlink
                } else if t.is_dir() {
                    EntryKind::Dir
                } else if t.is_file() {
                    EntryKind::File
                } else {
                    EntryKind::Other
                };
                out.push(EntryFacts {
                    name,
                    kind,
                    uid: m.uid(),
                    mode: m.mode(),
                    nlink: m.nlink(),
                });
            }
            Ok(Some(out))
        })()
    };
    PluginDirProbe {
        dir: dir.to_path_buf(),
        chain: match chain_err {
            Some(e) => Err(e),
            None => Ok(chain),
        },
        entries,
    }
}

/// One step per plugin directory probed ([`PLUGIN_DIRS`]), present or absent:
/// `ok` when [`plugin_dir_problem`] finds nothing, refused naming what it
/// found. Pure.
pub fn plugin_dir_steps(s: &HostState, probe: &DockerCliProbe) -> Vec<Step> {
    let sec = Section::VmRuntime;
    probe
        .plugin_dirs
        .iter()
        .map(|p| {
            let subject = p.dir.display().to_string();
            match plugin_dir_problem(p) {
                Err(e) => unknown(s, sec, subject, &e),
                Ok(None) => step(
                    sec,
                    Status::Ok,
                    subject,
                    match &p.entries {
                        Ok(Some(e)) => format!(
                            "a Docker CLI plugin directory, root's alone, with {} plugin(s), each \
                             a root-owned regular file: `docker build` may run them",
                            e.len()
                        ),
                        _ => "absent, and every directory above it is root's alone, so only \
                              root can create it"
                            .to_string(),
                    },
                ),
                Ok(Some(problem)) => step(
                    sec,
                    Status::Refused,
                    subject.clone(),
                    format!(
                        "{problem}. The Docker CLI faber runs executes plugins (buildx, for \
                         `docker build`) from {subject}, so whoever could write it or what is \
                         in it, or rename a directory above it, could choose what the daemon \
                         runs. Remove it (`sudo rm -rf \"{subject}\"`) if nothing needs it, or \
                         make it, every directory above it and every plugin in it root's \
                         regular files and directories (`sudo chown -R root:wheel`, `sudo chmod \
                         -R go-w`, symlinks replaced by the files), and re-run"
                    ),
                ),
            }
        })
        .collect()
}

/// Why the SYSTEM daemon must not run a Docker command that loads CLI plugins
/// now: the first of [`PLUGIN_DIRS`] that fails [`plugin_dir_problem`], or
/// could not be read. `None` elsewhere (LEGACY, Linux) and when every one is
/// fit. Re-read at most once a minute.
///
/// The commands that load plugins, per docker/cli: `build`, `image build`,
/// `builder` and `bake` (`processBuilder` in `cmd/docker/builder.go`, which
/// looks up buildx), any subcommand the CLI does not know itself (`compose`,
/// `buildx`: `tryPluginRun` in `cmd/docker/docker.go`), and help. Hooks also
/// run plugins, but only with a terminal on stdout, which faber's calls never
/// have. faber runs two of them: the variant build and `builder prune`.
pub fn plugin_gate() -> Option<String> {
    #[cfg(target_os = "macos")]
    if crate::platform::layout() == crate::platform::Layout::System {
        use std::sync::Mutex;
        use std::time::{Duration, Instant};
        static CACHE: Mutex<Option<(Instant, Option<String>)>> = Mutex::new(None);
        let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, answer)) = cache.as_ref() {
            if at.elapsed() < Duration::from_secs(60) {
                return answer.clone();
            }
        }
        let answer = live_plugin_problem();
        *cache = Some((Instant::now(), answer.clone()));
        return answer;
    }
    None
}

/// [`plugin_gate`]'s reading, uncached: the message for the first unfit
/// plugin directory.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn live_plugin_problem() -> Option<String> {
    PLUGIN_DIRS.iter().find_map(|d| {
        let p = probe_plugin_dir(std::path::Path::new(d));
        let why = match plugin_dir_problem(&p) {
            Ok(None) => return None,
            Ok(Some(why)) => why,
            Err(e) => format!("it cannot be read ({e})"),
        };
        Some(format!(
            "Docker CLI plugin directory {d}: {why}. The Docker CLI would run plugins from it, \
             so faber refuses `docker build` (runner-image variants) and `docker builder prune` \
             until it is removed or made root's alone; every other Docker call goes on \
             (ADR-0026)"
        ))
    })
}

/// Why the directories on the CLI's path (`chain`, those that exist) are not
/// fit for it: each must be root's and writable by nobody else. Pure.
pub fn parents_problem(chain: &[PathFacts]) -> Option<String> {
    const S_IFMT: u32 = 0o170_000;
    const S_IFDIR: u32 = 0o040_000;
    chain.iter().find_map(|f| {
        super::fsx::root_dir_problem(&f.path, f.mode & S_IFMT == S_IFDIR, f.uid, f.mode)
    })
}

/// The plan's step for the CLI (macOS, SYSTEM). Pure.
pub fn plan_step(s: &HostState, probe: &DockerCliProbe, pin: &DockerCliPin) -> Step {
    let sec = Section::VmRuntime;
    let subject = DOCKER_CLI.to_string();
    if !probe.apple_silicon {
        return step(
            sec,
            Status::Refused,
            subject,
            format!(
                "this release pins the Apple-silicon Docker CLI only ({}), and this Mac is not \
                 Apple silicon: the daemon would have no Docker CLI it can run",
                pin.url
            ),
        );
    }
    match &probe.parents {
        Err(e) => return unknown(s, sec, subject, e),
        Ok(chain) => {
            if let Some(problem) = parents_problem(chain) {
                return step(
                    sec,
                    Status::Refused,
                    subject,
                    format!(
                        "{problem}. Every directory above the Docker CLI the daemon runs must be \
                         root's and writable by nobody else, or whoever could rename one could \
                         choose what it runs (an operator-owned /usr/local from some older \
                         Homebrew setups allows that). Fix it (`sudo chown root:wheel`, `sudo \
                         chmod go-w`) and re-run"
                    ),
                );
            }
        }
    }
    let fetched = format!(
        "Docker CLI {} from {}: the archive's SHA-256 checked against the pin before it is \
         unpacked, only docker/docker kept and its own SHA-256 checked, root:wheel 0755. The \
         daemon runs it by this path; its staged copy is run as {FABER_ACCOUNT} (never as root) \
         and must report Docker version {}",
        pin.version, pin.url, pin.version
    );
    match &probe.installed {
        BinProbe::Missing => step(sec, Status::WillCreate, subject, fetched)
            .doing(Action::Vm(VmAction::InstallDockerCli)),
        BinProbe::File {
            sha256,
            uid,
            gid,
            mode,
            ..
        } if sha256 == pin.binary_sha256 => {
            if (*uid, *gid, mode & 0o7777) == (0, 0, 0o755) {
                step(
                    sec,
                    Status::Ok,
                    subject,
                    format!(
                        "Docker CLI {} (the pinned SHA-256), root:wheel 0755",
                        pin.version
                    ),
                )
            } else {
                step(
                    sec,
                    Status::WillChange,
                    subject,
                    format!(
                        "the pinned binary, but {uid}:{gid} {:o}: installed again; {fetched}",
                        mode & 0o7777
                    ),
                )
                .doing(Action::Vm(VmAction::InstallDockerCli))
            }
        }
        BinProbe::File { .. } => step(
            sec,
            Status::WillChange,
            subject,
            format!("not the pinned binary (its SHA-256 differs): replaced; {fetched}"),
        )
        .doing(Action::Vm(VmAction::InstallDockerCli)),
        BinProbe::NotRegular => step(
            sec,
            Status::Refused,
            subject,
            "not a regular file; move it aside and re-run",
        ),
        BinProbe::Unknown(e) => unknown(s, sec, subject, e),
    }
}

/// Whether the staged CLI's `--version` output is the pinned release's.
pub fn version_problem(stdout: &str, pin: &DockerCliPin) -> Option<String> {
    let want = format!("Docker version {}, build ", pin.version);
    let line = stdout.trim();
    (!line.starts_with(&want) || line.lines().count() != 1).then(|| {
        format!(
            "it reports {:?}, not \"{want}<commit>\"",
            super::tools::sanitize_child_output(line, 200)
        )
    })
}

/// Why the CLI was not installed. No variant carries downloaded bytes.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CliError {
    #[error("downloading the Docker CLI failed: {0}")]
    Download(String),
    #[error("the Docker CLI archive is longer than the {0} bytes this release pins; refused")]
    TooLarge(u64),
    #[error("the Docker CLI archive is {got} bytes, not the {want} this release pins; refused")]
    WrongSize { got: u64, want: u64 },
    #[error(
        "the Docker CLI archive does not match the SHA-256 this release pins; refused before \
         unpacking it"
    )]
    HashMismatch,
    #[error("decompressing the Docker CLI archive failed: {0}")]
    Unpack(String),
    #[error("the Docker CLI archive is refused: {0}")]
    Archive(String),
    #[error("the unpacked Docker CLI does not match the SHA-256 this release pins; refused")]
    BinaryMismatch,
}

impl From<BuildError> for CliError {
    fn from(e: BuildError) -> Self {
        CliError::Download(e.to_string())
    }
}

/// The effects of fetching the CLI. The real one is macOS only; tests
/// substitute a fake.
pub trait CliHost {
    type Chunks: Chunks;
    /// Start downloading `url`, over HTTPS only.
    fn download(
        &self,
        url: &'static str,
    ) -> impl std::future::Future<Output = Result<Self::Chunks, BuildError>>;
    /// Decompress `gz`, a gzip stream, refusing output longer than `cap`.
    fn gunzip(
        &self,
        gz: Vec<u8>,
        cap: u64,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, CliError>>;
}

/// The SHA-256 of `bytes`, lowercase hex.
fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(bytes))
}

/// Download the pinned archive, check it, and return the binary in it, checked
/// too. Nothing reads the archive's contents before its SHA-256 is the pin's.
pub async fn fetch_binary<H: CliHost>(host: &H, pin: &DockerCliPin) -> Result<Vec<u8>, CliError> {
    let mut chunks = host.download(pin.url).await?;
    let mut archive = Vec::new();
    while let Some(chunk) = chunks.next_chunk().await? {
        if (archive.len() + chunk.len()) as u64 > pin.size {
            return Err(CliError::TooLarge(pin.size));
        }
        archive.extend_from_slice(&chunk);
    }
    if archive.len() as u64 != pin.size {
        return Err(CliError::WrongSize {
            got: archive.len() as u64,
            want: pin.size,
        });
    }
    if sha256_hex(&archive) != pin.sha256 {
        return Err(CliError::HashMismatch);
    }
    let tar = host.gunzip(archive, pin.max_tar()).await?;
    let binary = binary_from_tar(&tar, pin)?;
    if binary.len() as u64 != pin.binary_size || sha256_hex(binary) != pin.binary_sha256 {
        return Err(CliError::BinaryMismatch);
    }
    Ok(binary.to_vec())
}

/// The pinned member of `tar`, a plain tar archive, after walking every
/// header: the archive may hold the member and its directory and nothing else,
/// each at most once. Any other member, any link or special file, any extended
/// header, and any absolute or `..` path refuses the whole archive. Pure.
pub fn binary_from_tar<'a>(tar: &'a [u8], pin: &DockerCliPin) -> Result<&'a [u8], CliError> {
    use super::vm_images::{tar_size, tar_text};
    let refuse = |why: String| Err(CliError::Archive(why));
    let dir = pin.member_dir();
    let mut found: Option<&[u8]> = None;
    let mut saw_dir = false;
    let mut at: usize = 0;
    loop {
        let Some(header) = tar.get(at..at + 512) else {
            return refuse(format!("it ends at byte {at} with no end-of-archive block"));
        };
        if header.iter().all(|&b| b == 0) {
            break;
        }
        if &header[257..262] != b"ustar" {
            return refuse(format!("the header at byte {at} is not a ustar header"));
        }
        let stored = tar_size(&header[148..156])
            .ok_or_else(|| CliError::Archive(format!("the header at byte {at} has no checksum")))?;
        let sum: u64 = header
            .iter()
            .enumerate()
            .map(|(i, &b)| u64::from(if (148..156).contains(&i) { b' ' } else { b }))
            .sum();
        if sum != stored {
            return refuse(format!("the header at byte {at} fails its checksum"));
        }
        let size = tar_size(&header[124..136]).ok_or_else(|| {
            CliError::Archive(format!("the header at byte {at} has an unreadable size"))
        })?;
        let mut name = tar_text(&header[..100]);
        // POSIX ustar ("ustar\0") has a name prefix; GNU's ("ustar  \0") uses
        // those bytes for other fields.
        if &header[257..263] == b"ustar\0" {
            let prefix = tar_text(&header[345..500]);
            if !prefix.is_empty() {
                name = format!("{prefix}/{name}");
            }
        }
        let shown = name.escape_debug().to_string();
        if name.starts_with('/') || name.split('/').any(|c| c == "..") {
            return refuse(format!(
                "its member \"{shown}\" is an absolute or `..` path"
            ));
        }
        let start = at + 512;
        let body = usize::try_from(size)
            .ok()
            .and_then(|s| s.checked_next_multiple_of(512))
            .ok_or_else(|| CliError::Archive(format!("its member \"{shown}\" is too large")))?;
        let end = start
            .checked_add(body)
            .filter(|end| *end <= tar.len())
            .ok_or_else(|| {
                CliError::Archive(format!("its member \"{shown}\" runs past the end"))
            })?;
        match header[156] {
            b'0' | 0 if name == pin.member => {
                if found.is_some() {
                    return refuse(format!("it lists \"{shown}\" twice"));
                }
                found = Some(&tar[start..start + size as usize]);
            }
            b'5' if name.trim_end_matches('/') == dir && !dir.is_empty() => {
                if saw_dir {
                    return refuse(format!("it lists \"{shown}\" twice"));
                }
                saw_dir = true;
            }
            b'1' | b'2' => return refuse(format!("its member \"{shown}\" is a link")),
            b'0' | 0 | b'5' => {
                return refuse(format!(
                    "it holds \"{shown}\", and only {} and {} belong in it",
                    pin.member_dir(),
                    pin.member
                ))
            }
            other => {
                return refuse(format!(
                    "its member \"{shown}\" has entry type {:?}, not a file or directory",
                    char::from(other)
                ))
            }
        }
        at = end;
    }
    found.ok_or_else(|| CliError::Archive(format!("it has no {}", pin.member)))
}

/// Why the CLI at `path` is not the one the SYSTEM daemon should run, for its
/// boot warning and the dashboard's notice: missing, not root's alone, or not
/// the pinned build. `None` when it is.
#[cfg(target_os = "macos")]
pub fn installed_problem(path: &Path, pin: &DockerCliPin) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let fix = "every Docker call fails until `sudo faber setup` installs it (ADR-0026)";
    let shown = path.display();
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Some(format!(
                "the Docker CLI faber runs, {shown}, is missing: {fix}"
            ))
        }
        Err(e) => {
            return Some(format!(
                "the Docker CLI faber runs, {shown}, cannot be read ({e}): {fix}"
            ))
        }
    };
    if !meta.file_type().is_file() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
        return Some(format!(
            "the Docker CLI faber runs, {shown}, is not a root-owned file only root can write: {fix}"
        ));
    }
    match super::fsx::file_digest(path) {
        Ok(Some(d)) if d.sha256 == pin.binary_sha256 => None,
        Ok(_) => Some(format!(
            "the Docker CLI faber runs, {shown}, is not the pinned Docker {}: {fix}",
            pin.version
        )),
        Err(e) => Some(format!(
            "the Docker CLI faber runs, {shown}, cannot be read ({e}): {fix}"
        )),
    }
}

/// Installing the CLI, on macOS, as root.
#[cfg(target_os = "macos")]
pub mod apply {
    use std::io::{Read, Write};
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::time::Duration;

    use anyhow::{anyhow, bail, Context, Result};

    use super::*;
    use crate::platform::record::Account;
    use crate::setup::fsx::{self, ROOT};
    use crate::setup::linux_image::{https_client, https_get, HttpChunks};
    use crate::setup::{tools, vm_images};

    /// The real [`CliHost`].
    pub struct MacCliHost {
        http: reqwest::Client,
    }

    impl MacCliHost {
        pub fn new() -> Result<Self> {
            Ok(Self {
                http: https_client(Duration::from_secs(10 * 60))?,
            })
        }
    }

    impl CliHost for MacCliHost {
        type Chunks = HttpChunks;

        async fn download(&self, url: &'static str) -> Result<HttpChunks, BuildError> {
            https_get(&self.http, url, "the Docker CLI archive").await
        }

        async fn gunzip(&self, gz: Vec<u8>, cap: u64) -> Result<Vec<u8>, CliError> {
            tokio::task::spawn_blocking(move || gunzip(&gz, cap))
                .await
                .map_err(|e| CliError::Unpack(e.to_string()))?
        }
    }

    /// `/usr/bin/gzip -dc`, fed `gz` and read to at most `cap` bytes. Runs as
    /// root on bytes whose SHA-256 is already the pin's.
    fn gunzip(gz: &[u8], cap: u64) -> Result<Vec<u8>, CliError> {
        let fail = |e: std::io::Error| CliError::Unpack(e.to_string());
        let mut child = Command::new("/usr/bin/gzip")
            .arg("-dc")
            .env_clear()
            .current_dir("/")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(fail)?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| CliError::Unpack("no stdin".into()))?;
        let input = gz.to_vec();
        let writer = std::thread::spawn(move || stdin.write_all(&input));
        let mut out = Vec::new();
        let read = child
            .stdout
            .take()
            .ok_or_else(|| CliError::Unpack("no stdout".into()))?
            .take(cap + 1)
            .read_to_end(&mut out);
        if read.is_err() || out.len() as u64 > cap {
            let _ = child.kill();
            let _ = child.wait();
            let _ = writer.join();
            return Err(match read {
                Err(e) => fail(e),
                Ok(_) => CliError::Unpack(format!("it unpacks to more than {cap} bytes")),
            });
        }
        let status = child.wait().map_err(fail)?;
        let _ = writer.join();
        if !status.success() {
            return Err(CliError::Unpack(format!("gzip exited {status}")));
        }
        Ok(out)
    }

    /// Download, check and install the pinned CLI at [`DOCKER_CLI`]
    /// (see the module docs). Nothing lands there unless every check passed.
    pub async fn install(s: &HostState, faber: Account) -> Result<()> {
        let host = MacCliHost::new()?;
        println!("  downloading Docker CLI {} from {}", PIN.version, PIN.url);
        let binary = fetch_binary(&host, &PIN).await?;
        place(&binary, &PIN, faber, &s.layout.state_dir)
    }

    /// Stage `binary` beside [`DOCKER_CLI`], root's 0755, check its version as
    /// `_faber`, and rename it into place.
    fn place(binary: &[u8], pin: &DockerCliPin, faber: Account, home: &Path) -> Result<()> {
        let path = Path::new(DOCKER_CLI);
        let (parent, name) = fsx::split(path)?;
        // Root's all the way up, created root's 0755 where missing, walked
        // without following a symlink.
        let dir = fsx::root_dir_chain(&vm_images::canonical(parent))
            .with_context(|| format!("preparing {}", parent.display()))?;
        // faber's own directory: searchable by `_faber`, whatever it was.
        dir.set_owner_mode(ROOT, 0o755)?;
        let stage = format!(".{name}.setup-stage");
        let _ = dir.unlink(&stage);
        dir.write_atomic_expecting(&stage, &mut &binary[..], ROOT, 0o755, pin.binary_sha256)
            .with_context(|| format!("staging {}", path.display()))?;
        let staged = dir.path().join(&stage);
        let checked = tools::run_as(
            &staged,
            &[std::ffi::OsStr::new("--version")],
            None,
            Some(faber),
            home,
            Duration::from_secs(30),
        )
        .with_context(|| format!("running the staged Docker CLI's --version as {FABER_ACCOUNT}"))
        .and_then(|out| {
            if !out.success {
                bail!("its --version failed: {}", out.sanitized(400));
            }
            match version_problem(&out.stdout, pin) {
                Some(why) => Err(anyhow!("{why}")),
                None => Ok(()),
            }
        });
        if let Err(e) = checked {
            let _ = dir.unlink(&stage);
            return Err(e.context(format!("{} was not installed", path.display())));
        }
        dir.rename(&stage, name)?;
        println!(
            "  installed {} (Docker CLI {})",
            path.display(),
            pin.version
        );
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::setup::plan::tests::{fresh_mac, set_up_mac};
    use crate::setup::plan::{counts, plan};
    use std::cell::RefCell;
    use std::collections::VecDeque;

    // --- the plan ----------------------------------------------------------

    fn cli_step(s: &HostState) -> Step {
        plan(s)
            .into_iter()
            .find(|st| st.subject == DOCKER_CLI)
            .expect("a Docker CLI step on macOS")
    }

    pub(crate) fn root_parents() -> Vec<PathFacts> {
        [
            "/",
            "/usr",
            "/usr/local",
            "/usr/local/libexec",
            "/usr/local/libexec/faber",
        ]
        .iter()
        .map(|p| PathFacts {
            path: p.into(),
            uid: 0,
            gid: 0,
            mode: 0o040_755,
        })
        .collect()
    }

    fn installed(sha: &str, uid: u32, mode: u32) -> BinProbe {
        BinProbe::File {
            sha256: sha.into(),
            uid,
            gid: 0,
            mode: 0o100_000 | mode,
            version: None,
        }
    }

    #[test]
    fn a_fresh_mac_installs_it() {
        let st = cli_step(&fresh_mac());
        assert_eq!(st.status, Status::WillCreate);
        assert_eq!(st.action, Some(Action::Vm(VmAction::InstallDockerCli)));
        assert!(st.detail.contains(PIN.url), "{}", st.detail);
        assert!(st.detail.contains("never as root"), "{}", st.detail);
    }

    /// A set-up host with the pinned binary plans nothing for it, and nothing
    /// at all.
    #[test]
    fn the_pinned_binary_is_ok_and_a_re_run_plans_nothing() {
        let s = set_up_mac();
        let st = cli_step(&s);
        assert_eq!((st.status, st.action.clone()), (Status::Ok, None));
        let (create, change, refused) = counts(&plan(&s));
        assert_eq!((create, change, refused), (0, 0, 0));
    }

    #[test]
    fn another_binary_or_the_wrong_owner_is_changed() {
        let mut s = set_up_mac();
        s.vm_runtime.docker_cli.installed = installed(&"0".repeat(64), 0, 0o755);
        let st = cli_step(&s);
        assert_eq!(st.status, Status::WillChange);
        assert_eq!(st.action, Some(Action::Vm(VmAction::InstallDockerCli)));
        assert!(st.detail.contains("SHA-256 differs"), "{}", st.detail);

        s.vm_runtime.docker_cli.installed = installed(PIN.binary_sha256, 501, 0o755);
        assert_eq!(cli_step(&s).status, Status::WillChange);
        s.vm_runtime.docker_cli.installed = installed(PIN.binary_sha256, 0, 0o775);
        assert_eq!(cli_step(&s).status, Status::WillChange);
    }

    #[test]
    fn a_directory_root_does_not_own_alone_refuses() {
        let mut s = set_up_mac();
        let mut chain = root_parents();
        chain[2].uid = 501; // an operator-owned /usr/local
        s.vm_runtime.docker_cli.parents = Ok(chain);
        let st = cli_step(&s);
        assert_eq!(st.status, Status::Refused);
        assert!(
            st.detail.contains("/usr/local is owned by uid 501"),
            "{}",
            st.detail
        );

        let mut chain = root_parents();
        chain[3].mode = 0o040_775;
        s.vm_runtime.docker_cli.parents = Ok(chain);
        assert_eq!(cli_step(&s).status, Status::Refused);

        let mut chain = root_parents();
        chain[4].mode = 0o120_755; // a symlink
        s.vm_runtime.docker_cli.parents = Ok(chain);
        assert_eq!(cli_step(&s).status, Status::Refused);
    }

    /// The pin is Apple-silicon only: another Mac is refused with that reason,
    /// whatever is installed, and nothing is fetched.
    #[test]
    fn a_mac_that_is_not_apple_silicon_refuses() {
        for mut s in [fresh_mac(), set_up_mac()] {
            s.vm_runtime.docker_cli.apple_silicon = false;
            let st = cli_step(&s);
            assert_eq!((st.status, st.action.clone()), (Status::Refused, None));
            assert!(
                st.detail
                    .contains("this release pins the Apple-silicon Docker CLI only"),
                "{}",
                st.detail
            );
        }
    }

    /// `dir` and every directory above it, root's 0755; only the first
    /// `depth` components (from `/`) when it is partly absent.
    pub(crate) fn plugin_chain(dir: &str, depth: Option<usize>) -> Vec<PathFacts> {
        let mut at = PathBuf::from("/");
        let mut chain = vec![at.clone()];
        for c in std::path::Path::new(dir).components().skip(1) {
            at.push(c);
            chain.push(at.clone());
        }
        chain.truncate(depth.unwrap_or(usize::MAX));
        chain
            .into_iter()
            .map(|path| PathFacts {
                path,
                uid: 0,
                gid: 0,
                mode: 0o040_755,
            })
            .collect()
    }

    /// An absent plugin directory whose existing ancestors are root's.
    pub(crate) fn absent_plugin_dir(dir: &str) -> PluginDirProbe {
        PluginDirProbe {
            dir: dir.into(),
            chain: Ok(plugin_chain(dir, Some(3))),
            entries: Ok(None),
        }
    }

    fn present(dir: &str, entries: Vec<EntryFacts>) -> PluginDirProbe {
        PluginDirProbe {
            dir: dir.into(),
            chain: Ok(plugin_chain(dir, None)),
            entries: Ok(Some(entries)),
        }
    }

    fn entry(name: &str, kind: EntryKind, uid: u32, mode: u32, nlink: u64) -> EntryFacts {
        EntryFacts {
            name: name.into(),
            kind,
            uid,
            mode,
            nlink,
        }
    }

    fn buildx() -> EntryFacts {
        entry("docker-buildx", EntryKind::File, 0, 0o100_755, 1)
    }

    fn plugin_steps(s: &HostState) -> Vec<Step> {
        plan(s)
            .into_iter()
            .filter(|st| PLUGIN_DIRS.contains(&st.subject.as_str()))
            .collect()
    }

    /// Every plugin directory gets a step, absent ones too: `ok` while the
    /// directories that hold it (or would) are root's alone, so no other
    /// account can create it later.
    #[test]
    fn absent_plugin_directories_are_judged_by_what_would_hold_them() {
        let mut s = set_up_mac();
        let steps = plugin_steps(&s);
        assert_eq!(steps.len(), PLUGIN_DIRS.len());
        assert!(steps.iter().all(|st| st.status == Status::Ok));
        assert!(steps[0].detail.contains("absent"), "{}", steps[0].detail);
        assert_eq!(counts(&plan(&s)), (0, 0, 0));

        // `/usr/local/lib` there and operator-owned, `docker/` not: refused.
        let dir = PLUGIN_DIRS[0];
        let mut chain = plugin_chain(dir, Some(4));
        chain[3].uid = 501;
        s.vm_runtime.docker_cli.plugin_dirs[0] = PluginDirProbe {
            dir: dir.into(),
            chain: Ok(chain),
            entries: Ok(None),
        };
        let st = &plugin_steps(&s)[0];
        assert_eq!(st.status, Status::Refused);
        assert!(
            st.detail.contains("/usr/local/lib is owned by uid 501"),
            "{}",
            st.detail
        );
    }

    /// A present plugin directory: refused when it or a directory above it is
    /// not root's alone, naming the directory and what to do.
    #[test]
    fn a_plugin_directory_root_does_not_own_alone_refuses() {
        let mut s = set_up_mac();
        let dir = PLUGIN_DIRS[0];
        s.vm_runtime.docker_cli.plugin_dirs = vec![present(dir, vec![buildx()])];
        let steps = plugin_steps(&s);
        assert_eq!(steps[0].status, Status::Ok, "{}", steps[0].detail);
        assert!(
            steps[0].detail.contains("1 plugin(s)"),
            "{}",
            steps[0].detail
        );

        for (i, uid, mode) in [(5, 501, 0o040_755), (2, 501, 0o040_755), (5, 0, 0o040_777)] {
            let mut p = present(dir, vec![]);
            if let Ok(chain) = &mut p.chain {
                chain[i].uid = uid;
                chain[i].mode = mode;
            }
            s.vm_runtime.docker_cli.plugin_dirs = vec![p];
            let st = &plugin_steps(&s)[0];
            assert_eq!(st.status, Status::Refused, "{i} {uid} {mode:o}");
            assert!(
                st.detail.contains(&format!("sudo rm -rf \"{dir}\"")),
                "{}",
                st.detail
            );
            assert!(st.detail.contains("buildx"), "{}", st.detail);
        }
    }

    /// Every entry must be a root-owned regular file nobody else can write,
    /// with one link: a symlink (Docker Desktop's install points them into
    /// `/Applications/Docker.app`), a directory, a FIFO, another owner, a
    /// writable mode or a hard link refuses, naming the entry.
    #[test]
    fn a_plugin_that_is_not_a_root_only_regular_file_refuses() {
        let dir = PLUGIN_DIRS[1];
        let bad = [
            (
                entry("docker-compose", EntryKind::Symlink, 0, 0o120_755, 1),
                "is a symlink",
            ),
            (
                entry("docker-x", EntryKind::Dir, 0, 0o040_755, 2),
                "is a directory",
            ),
            (
                entry("docker-x", EntryKind::Other, 0, 0o010_644, 1),
                "not a regular file",
            ),
            (
                entry("docker-x", EntryKind::File, 501, 0o100_755, 1),
                "uid 501",
            ),
            (
                entry("docker-x", EntryKind::File, 0, 0o100_775, 1),
                "writable",
            ),
            (
                entry("docker-x", EntryKind::File, 0, 0o100_757, 1),
                "writable",
            ),
            (
                entry("docker-x", EntryKind::File, 0, 0o100_755, 2),
                "hard links",
            ),
        ];
        for (e, want) in bad {
            let name = e.name.clone();
            let why = plugin_dir_problem(&present(dir, vec![buildx(), e.clone()]))
                .unwrap()
                .expect("refused");
            assert!(why.contains(&format!("{dir}/{name}")), "{why}");
            assert!(why.contains(want), "{why}");

            let mut s = set_up_mac();
            s.vm_runtime.docker_cli.plugin_dirs = vec![present(dir, vec![buildx(), e])];
            let st = &plugin_steps(&s)[0];
            assert_eq!(st.status, Status::Refused);
            assert!(st.detail.contains(&name), "{}", st.detail);
        }
        assert_eq!(
            plugin_dir_problem(&present(dir, vec![])),
            Ok(None),
            "empty is fine"
        );
        assert_eq!(plugin_dir_problem(&present(dir, vec![buildx()])), Ok(None));
    }

    /// The live reading on this machine, through the real `lstat`s: an absent
    /// directory reads as absent with its existing ancestors, never as an
    /// error.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn probing_an_absent_plugin_directory_reads_its_ancestors() {
        let p = probe_plugin_dir(std::path::Path::new(
            "/nonexistent-faber/docker/cli-plugins",
        ));
        assert_eq!(p.entries, Ok(None));
        let chain = p.chain.unwrap();
        assert_eq!(chain.len(), 1, "only / exists");
        assert_eq!(chain[0].path, PathBuf::from("/"));
    }

    /// Entries are read with `lstat`: a symlink in a real directory is seen as
    /// one, not as what it points at.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn probing_reads_entries_without_following_symlinks() {
        let tmp = std::env::temp_dir().join(format!("faber-plugins-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        std::fs::write(tmp.join("docker-buildx"), b"x").unwrap();
        std::os::unix::fs::symlink(tmp.join("docker-buildx"), tmp.join("docker-compose")).unwrap();
        // The walk follows no symlink, and macOS's temp directory is under one
        // (`/var`).
        let tmp = std::fs::canonicalize(&tmp).unwrap();
        let p = probe_plugin_dir(&tmp);
        let mut entries = p.entries.clone().unwrap().unwrap();
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(entries[0].kind, EntryKind::File);
        assert_eq!(entries[1].kind, EntryKind::Symlink);
        // A temp directory is not root's alone, so it is refused either way.
        assert!(plugin_dir_problem(&p).unwrap().is_some());
        std::fs::remove_dir_all(&tmp).unwrap();
    }

    #[test]
    fn the_plugin_dirs_are_docker_s_unix_defaults() {
        assert_eq!(
            PLUGIN_DIRS,
            [
                "/usr/local/lib/docker/cli-plugins",
                "/usr/local/libexec/docker/cli-plugins",
                "/usr/lib/docker/cli-plugins",
                "/usr/libexec/docker/cli-plugins",
            ]
        );
    }

    #[test]
    fn something_else_at_the_path_refuses() {
        let mut s = fresh_mac();
        s.vm_runtime.docker_cli.installed = BinProbe::NotRegular;
        assert_eq!(cli_step(&s).status, Status::Refused);
    }

    #[test]
    fn linux_plans_no_docker_cli() {
        let mut s = fresh_mac();
        s.os = crate::platform::layout::Os::Linux;
        assert!(plan(&s).iter().all(|st| st.subject != DOCKER_CLI));
    }

    #[test]
    fn the_version_must_be_the_pinned_one() {
        assert_eq!(
            version_problem("Docker version 29.9.2, build a82e528\n", &PIN),
            None
        );
        assert!(version_problem("Docker version 29.9.20, build x", &PIN).is_some());
        assert!(version_problem("Docker version 28.0.0, build x", &PIN).is_some());
        assert!(version_problem("", &PIN).is_some());
    }

    #[test]
    fn the_pin_is_well_formed() {
        for hex in [PIN.sha256, PIN.binary_sha256] {
            assert_eq!(hex.len(), 64);
            assert!(hex
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        }
        assert!(PIN.url.starts_with("https://download.docker.com/"));
        assert!(PIN.url.ends_with(&format!("docker-{}.tgz", PIN.version)));
        assert_eq!(PIN.member_dir(), "docker");
        assert!(PIN.max_tar() > PIN.binary_size);
    }

    // --- the archive -------------------------------------------------------

    /// One ustar header, POSIX or GNU flavour, with a valid checksum.
    fn header(name: &str, kind: u8, size: usize, gnu: bool) -> [u8; 512] {
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..107].copy_from_slice(b"0000755");
        h[124..135].copy_from_slice(format!("{size:011o}").as_bytes());
        h[156] = kind;
        if gnu {
            h[257..265].copy_from_slice(b"ustar  \0");
        } else {
            h[257..263].copy_from_slice(b"ustar\0");
            h[263..265].copy_from_slice(b"00");
        }
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        h
    }

    fn archive(entries: &[(&str, u8, &[u8])], gnu: bool) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, kind, body) in entries {
            out.extend_from_slice(&header(name, *kind, body.len(), gnu));
            out.extend_from_slice(body);
            out.resize(out.len().next_multiple_of(512), 0);
        }
        out.extend_from_slice(&[0u8; 1024]);
        out
    }

    const BODY: &[u8] = b"\xcf\xfa\xed\xfe not really a Mach-O";

    #[test]
    fn the_one_member_is_found_in_either_tar_flavour() {
        for gnu in [true, false] {
            let tar = archive(
                &[("docker/", b'5', b""), ("docker/docker", b'0', BODY)],
                gnu,
            );
            assert_eq!(binary_from_tar(&tar, &PIN), Ok(BODY));
        }
        // Without its directory entry too.
        let tar = archive(&[("docker/docker", b'0', BODY)], true);
        assert_eq!(binary_from_tar(&tar, &PIN), Ok(BODY));
    }

    fn refused(tar: &[u8]) -> String {
        match binary_from_tar(tar, &PIN) {
            Err(CliError::Archive(why)) => why,
            other => panic!("not refused: {other:?}"),
        }
    }

    #[test]
    fn extra_members_links_and_traversal_are_refused() {
        let ok = ("docker/docker", b'0', BODY);
        let why = refused(&archive(&[ok, ("docker/docker-compose", b'0', BODY)], true));
        assert!(why.contains("docker-compose"), "{why}");
        let why = refused(&archive(&[("docker/evil", b'2', b""), ok], true));
        assert!(why.contains("link"), "{why}");
        let why = refused(&archive(&[ok, ("docker/docker", b'1', b"")], true));
        assert!(why.contains("link"), "{why}");
        let why = refused(&archive(&[("/docker/docker", b'0', BODY)], true));
        assert!(why.contains("absolute"), "{why}");
        let why = refused(&archive(&[("docker/../../etc/x", b'0', BODY)], true));
        assert!(why.contains("`..`"), "{why}");
        let why = refused(&archive(&[ok, ok], true));
        assert!(why.contains("twice"), "{why}");
        let why = refused(&archive(&[("other/", b'5', b""), ok], true));
        assert!(why.contains("only"), "{why}");
        let why = refused(&archive(&[("pax", b'x', b"x"), ok], false));
        assert!(why.contains("entry type"), "{why}");
        let why = refused(&archive(&[("docker/", b'5', b"")], true));
        assert!(why.contains("no docker/docker"), "{why}");
    }

    #[test]
    fn a_damaged_archive_is_refused() {
        let mut tar = archive(&[("docker/docker", b'0', BODY)], true);
        tar[0] = b'D'; // the checksum no longer holds
        assert!(refused(&tar).contains("checksum"));
        let tar = archive(&[("docker/docker", b'0', BODY)], true);
        assert!(refused(&tar[..512]).contains("runs past the end"));
        assert!(refused(&tar[..1024]).contains("end-of-archive"));
        assert!(refused(&[1u8; 512]).contains("not a ustar"));
    }

    // --- the download ------------------------------------------------------

    struct FakeChunks(VecDeque<Vec<u8>>);

    impl Chunks for FakeChunks {
        async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, BuildError> {
            Ok(self.0.pop_front())
        }
    }

    /// Serves `archive` in two chunks; "decompresses" by returning `tar`.
    struct FakeHost {
        archive: Vec<u8>,
        tar: Vec<u8>,
        gunzipped: RefCell<bool>,
    }

    impl CliHost for FakeHost {
        type Chunks = FakeChunks;

        async fn download(&self, url: &'static str) -> Result<FakeChunks, BuildError> {
            assert!(url.starts_with("https://"));
            let (a, b) = self.archive.split_at(self.archive.len() / 2);
            Ok(FakeChunks(VecDeque::from([a.to_vec(), b.to_vec()])))
        }

        async fn gunzip(&self, _gz: Vec<u8>, cap: u64) -> Result<Vec<u8>, CliError> {
            *self.gunzipped.borrow_mut() = true;
            if self.tar.len() as u64 > cap {
                return Err(CliError::Unpack("too long".into()));
            }
            Ok(self.tar.clone())
        }
    }

    /// A pin over the fake's bytes.
    fn pin_for(host: &FakeHost, binary: &[u8]) -> DockerCliPin {
        DockerCliPin {
            sha256: Box::leak(sha256_hex(&host.archive).into_boxed_str()),
            size: host.archive.len() as u64,
            binary_sha256: Box::leak(sha256_hex(binary).into_boxed_str()),
            binary_size: binary.len() as u64,
            ..PIN
        }
    }

    fn host() -> FakeHost {
        FakeHost {
            archive: b"\x1f\x8b pretend gzip".to_vec(),
            tar: archive(
                &[("docker/", b'5', b""), ("docker/docker", b'0', BODY)],
                true,
            ),
            gunzipped: RefCell::new(false),
        }
    }

    #[tokio::test]
    async fn a_pinned_archive_yields_the_pinned_binary() {
        let h = host();
        let pin = pin_for(&h, BODY);
        assert_eq!(fetch_binary(&h, &pin).await, Ok(BODY.to_vec()));
    }

    #[tokio::test]
    async fn a_wrong_hash_is_refused_before_anything_is_unpacked() {
        let h = host();
        let pin = DockerCliPin {
            sha256: "0000000000000000000000000000000000000000000000000000000000000000",
            ..pin_for(&h, BODY)
        };
        assert_eq!(fetch_binary(&h, &pin).await, Err(CliError::HashMismatch));
        assert!(
            !*h.gunzipped.borrow(),
            "unpacked before its hash was checked"
        );
    }

    #[tokio::test]
    async fn an_oversize_or_short_download_is_refused() {
        let h = host();
        let pin = DockerCliPin {
            size: h.archive.len() as u64 - 1,
            ..pin_for(&h, BODY)
        };
        assert_eq!(
            fetch_binary(&h, &pin).await,
            Err(CliError::TooLarge(h.archive.len() as u64 - 1))
        );
        let pin = DockerCliPin {
            size: h.archive.len() as u64 + 1,
            ..pin_for(&h, BODY)
        };
        assert!(matches!(
            fetch_binary(&h, &pin).await,
            Err(CliError::WrongSize { .. })
        ));
        assert!(!*h.gunzipped.borrow());
    }

    #[tokio::test]
    async fn a_binary_that_is_not_the_pinned_one_is_refused() {
        let h = host();
        let pin = pin_for(&h, b"some other binary");
        assert_eq!(fetch_binary(&h, &pin).await, Err(CliError::BinaryMismatch));
    }

    /// The real pin, end to end: download it over HTTPS, check it, unpack it
    /// with `/usr/bin/gzip` and walk its headers. Needs the network, so it is
    /// ignored by default; run it when bumping [`PIN`]:
    /// `cargo test --lib live_the_pin_downloads_and_unpacks -- --ignored`.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    #[ignore = "downloads the pinned archive"]
    async fn live_the_pin_downloads_and_unpacks() {
        let host = apply::MacCliHost::new().unwrap();
        let binary = fetch_binary(&host, &PIN).await.unwrap();
        assert_eq!(binary.len() as u64, PIN.binary_size);
        assert_eq!(&binary[..4], b"\xcf\xfa\xed\xfe", "a 64-bit Mach-O");
    }

    #[tokio::test]
    async fn an_archive_with_more_in_it_is_refused() {
        let mut h = host();
        h.tar = archive(
            &[("docker/docker", b'0', BODY), ("docker/extra", b'0', BODY)],
            true,
        );
        let pin = pin_for(&h, BODY);
        assert!(matches!(
            fetch_binary(&h, &pin).await,
            Err(CliError::Archive(_))
        ));
    }
}
