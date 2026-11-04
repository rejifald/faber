//! The two images an operator imports with `sudo faber setup` (ADR-0026), on
//! macOS only: the golden macOS image the per-job VMs clone, and the
//! runner-image tarball faber's Linux VM loads.
//!
//! ```text
//! sudo faber setup --golden-image PATH [--golden-name NAME] [--replace-golden]
//! sudo faber setup --runner-image-tar PATH
//! ```
//!
//! Both land in [`IMAGE_STORE`], root's, and from then on belong to root. Both
//! are **local-only artifacts** (AGENTS.md: "No image or package is published
//! publicly"): a golden image takes hours to prepare, and the runner image's
//! only other copy is one VM disk. So nothing in faber deletes either:
//! uninstall leaves the whole store, and says so ([`super::uninstall`]).
//!
//! # The golden image
//!
//! A bundle directory holding the four files the launcher boots a macOS
//! guest from (`launcher/Sources/Bundle.swift`): [`GOLDEN_FILES`], and
//! optionally a `manifest.json` ([`GOLDEN_OPTIONAL`], ADR-0049 §6). It is
//! read as root, so every file must be a regular file with one link, opened
//! without following a symlink from the bundle directory, itself opened
//! without following one. Anything else in the bundle is refused rather than
//! skipped: a stray `disk.raw` would make the launcher read the bundle as
//! Linux, and a stray `faber-clone` marker would make it a clone the sweep may
//! delete.
//!
//! The import clones each file (APFS `clonefile`, so a 40 GB image costs no
//! space until a file changes) into a root-only staging directory in the
//! store, clears any ACL and extended attribute the clone carried
//! over, checks each clone's SHA-256 against the plan's, and makes it
//! `root:wheel` 0444 in a 0755 bundle directory. Only then is the bundle
//! renamed to `IMAGE_STORE/<name>`. `_faber_vm` can read it, since
//! every directory on the way is 0755 and every file 0444, and nobody but
//! root can change it. A profile names it as `image = "<name>"`.
//!
//! The store itself is the supervisor's `--image-dir`, which resolves every
//! image name, macOS or Linux, as `IMAGE_STORE/<name>`: the Linux engine's
//! `linux` ([`crate::vm::engine::ENGINE_IMAGE`]) sits beside the golden
//! images. So a golden name may not be one the store already uses for
//! something else ([`RESERVED_NAMES`]).
//!
//! A bundle already at that name is left alone when every file's SHA-256
//! matches, and refused when one differs, unless `--replace-golden` is given.
//! A replaced bundle is renamed aside and removed only once the new one is in
//! place. That is the one deletion here, and the operator asked for it by
//! name.
//!
//! # The runner-image tarball
//!
//! A `docker save` archive: a regular file, opened without following a
//! symlink, whose ustar headers are walked (capped at [`MAX_TAR_ENTRIES`]
//! headers, never its contents) until a top-level `manifest.json` is found.
//! It is copied to `IMAGE_STORE/runner/<file name>`, `root:_faber` 0440, in a
//! directory `root:_faber` 0750: `_faber` can read it and stream it to the
//! Linux VM's `docker load`, and only root can change it. An existing file of
//! that name is kept when its SHA-256 matches and refused when it differs: it
//! is the image's backup (ADR-0026, "The runner image moves first"), and a
//! backup is never overwritten.
//!
//! The daemon loads it into the Linux VM each time the engine comes up (#254).
//! The seam is [`RUNNER_DIR`]: the loader reads the tarballs there, as `_faber`,
//! through the group.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::platform::layout::IMAGE_STORE;

/// The files a macOS bundle must hold.
pub const GOLDEN_FILES: [&str; 4] = ["disk", "vz-aux", "vz-hwmodel", "vz-identifier"];

/// The files a bundle may also hold (ADR-0049 §6). Each is hashed, carried and
/// imported like a required one when present, but its absence is not a problem.
/// Nothing in setup reads their contents.
pub const GOLDEN_OPTIONAL: [&str; 1] = ["manifest.json"];

/// The largest an optional member may be: setup does not parse it, so a bound
/// keeps a stray large file from being hashed and copied as one.
pub const MAX_OPTIONAL_BYTES: u64 = 1 << 20;

/// Names a golden image may not take, because the store, or a directory
/// beside it under `/var/db/faber`, uses them for something else: the Linux
/// image ([`super::linux_image::LINUX_DIR`]), the runner tarballs
/// ([`RUNNER_DIR`]), and the store's siblings, so a name never reads as one of
/// them. `macos` was the golden images' own directory in an earlier draft. A
/// name starting with `.` (staging, set-aside bundles) is refused by the
/// grammar itself.
pub const RESERVED_NAMES: [&str; 6] = ["linux", "runner", "macos", "bin", "tools-disks", "wall"];

/// The runner-image tarballs' directory in [`IMAGE_STORE`]: where the Linux
/// VM's loader reads them.
pub const RUNNER_DIR: &str = "runner";

/// The most ustar headers read looking for `manifest.json`: a `docker save`
/// archive of a large image has a few thousand entries.
pub const MAX_TAR_ENTRIES: usize = 1 << 16;

/// The longest file name a runner tarball may have.
const MAX_TAR_NAME: usize = 128;

/// Where golden images live: [`IMAGE_STORE`] itself, the supervisor's
/// `--image-dir`.
pub fn golden_dir() -> PathBuf {
    PathBuf::from(IMAGE_STORE)
}

/// `IMAGE_STORE/runner`.
pub fn runner_dir() -> PathBuf {
    Path::new(IMAGE_STORE).join(RUNNER_DIR)
}

/// `path` as macOS resolves it: `/var/…` and `/etc/…` under `/private`, where
/// they are root's real directories. A walk that refuses every symlink
/// ([`super::fsx::root_dir_chain`]) needs this form. Pure.
pub fn canonical(path: &Path) -> PathBuf {
    for top in ["/var", "/etc", "/tmp"] {
        if let Ok(rest) = path.strip_prefix(top) {
            return Path::new("/private")
                .join(top.trim_start_matches('/'))
                .join(rest);
        }
    }
    path.to_path_buf()
}

/// Whether `name` may name a golden image: `[a-z0-9][a-z0-9._-]{0,63}`, no
/// `..`, and none of [`RESERVED_NAMES`]. That is the supervisor protocol's image-name grammar
/// (`docs/vm-supervisor-protocol.md`), so the name a profile gives is one the
/// supervisor accepts, and it is one path component.
pub fn golden_name_problem(name: &str) -> Option<String> {
    let b = name.as_bytes();
    let ok = !b.is_empty()
        && b.len() <= 64
        && matches!(b[0], b'a'..=b'z' | b'0'..=b'9')
        && b.iter()
            .all(|c| matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'))
        && !name.contains("..");
    if ok && RESERVED_NAMES.contains(&name) {
        return Some(format!(
            "{name} is reserved: {IMAGE_STORE}/{name} is not a golden image. Pass --golden-name \
             with another name"
        ));
    }
    (!ok).then(|| {
        format!(
            "{} is not a golden image name: lowercase letters, digits, '.', '_' and '-', \
             starting with a letter or digit, at most 64 bytes, and no \"..\". Pass \
             --golden-name",
            name.escape_debug()
        )
    })
}

/// Whether `name` may name a runner tarball in the store: printable ASCII
/// letters, digits, `.`, `_`, `-` and `+`, not starting with `.`, at most
/// [`MAX_TAR_NAME`] bytes.
pub fn tar_name_problem(name: &str) -> Option<String> {
    let ok = !name.is_empty()
        && name.len() <= MAX_TAR_NAME
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-' | b'+'));
    (!ok).then(|| {
        format!(
            "{} is not a file name setup stores: letters, digits, '.', '_', '-' and '+', not \
             starting with '.', at most {MAX_TAR_NAME} bytes. Rename it and re-run",
            name.escape_debug()
        )
    })
}

/// Why the entries of a bundle directory are not [`GOLDEN_FILES`], plus
/// optionally [`GOLDEN_OPTIONAL`], and nothing else.
pub fn bundle_entries_problem(entries: &[String]) -> Option<String> {
    let mut extra: Vec<&str> = entries
        .iter()
        .map(String::as_str)
        .filter(|e| !GOLDEN_FILES.contains(e) && !GOLDEN_OPTIONAL.contains(e))
        .collect();
    extra.sort_unstable();
    let missing: Vec<&str> = GOLDEN_FILES
        .iter()
        .copied()
        .filter(|f| !entries.iter().any(|e| e == f))
        .collect();
    if missing.is_empty() && extra.is_empty() {
        return None;
    }
    let mut msg = format!(
        "a golden image bundle holds {} (required) and may hold {} (optional), and nothing else",
        GOLDEN_FILES.join(", "),
        GOLDEN_OPTIONAL.join(", ")
    );
    if !missing.is_empty() {
        msg.push_str(&format!("; missing required: {}", missing.join(", ")));
    }
    if !extra.is_empty() {
        msg.push_str(&format!(
            "; also there: {}",
            extra
                .iter()
                .map(|e| e.escape_debug().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    Some(msg)
}

/// The members of a bundle whose entries are `entries`, in the order they are
/// hashed and compared: [`GOLDEN_FILES`], then each present [`GOLDEN_OPTIONAL`].
pub fn bundle_members(entries: &[String]) -> Vec<&'static str> {
    GOLDEN_FILES
        .iter()
        .copied()
        .chain(
            GOLDEN_OPTIONAL
                .iter()
                .copied()
                .filter(|o| entries.iter().any(|e| e == o)),
        )
        .collect()
}

/// One ustar header's size field: octal, or GNU's base-256 when its first
/// byte has the high bit set (sizes of 8 GiB and more).
pub(super) fn tar_size(field: &[u8]) -> Option<u64> {
    if field.first().is_some_and(|b| b & 0x80 != 0) {
        let mut n: u64 = u64::from(field[0] & 0x7f);
        for &b in &field[1..] {
            n = n.checked_mul(256)?.checked_add(u64::from(b))?;
        }
        return Some(n);
    }
    let text = std::str::from_utf8(field).ok()?;
    let digits = text.trim_matches(|c: char| c == '\0' || c == ' ');
    if digits.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(digits, 8).ok()
}

/// A NUL-terminated header field as text.
pub(super) fn tar_text(field: &[u8]) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// Walk the ustar headers of an archive `len` bytes long until a top-level
/// regular `manifest.json`, which every `docker save` archive has. Reads only
/// headers (at most [`MAX_TAR_ENTRIES`] of them) and seeks over contents, so
/// a multi-gigabyte archive costs a few reads. A compressed archive, a bad
/// header checksum, an entry running past the end, or no manifest is an
/// error.
pub fn check_docker_archive<R: Read + Seek>(r: &mut R, len: u64) -> Result<(), String> {
    let mut header = [0u8; 512];
    let mut at: u64 = 0;
    for _ in 0..MAX_TAR_ENTRIES {
        if at + 512 > len {
            return Err("it ends before a manifest.json entry: not a `docker save` archive".into());
        }
        r.seek(SeekFrom::Start(at)).map_err(|e| e.to_string())?;
        r.read_exact(&mut header).map_err(|e| e.to_string())?;
        if at == 0 && header[..2] == [0x1f, 0x8b] {
            return Err(
                "it is gzip-compressed; `docker save` writes a plain tar: decompress it first"
                    .into(),
            );
        }
        if header.iter().all(|&b| b == 0) {
            return Err("it has no manifest.json entry: not a `docker save` archive".into());
        }
        if &header[257..262] != b"ustar" {
            return Err(format!("the header at byte {at} is not a ustar header"));
        }
        let stored = tar_size(&header[148..156])
            .ok_or_else(|| format!("the header at byte {at} has no checksum"))?;
        let sum: u64 = header
            .iter()
            .enumerate()
            .map(|(i, &b)| {
                if (148..156).contains(&i) {
                    u64::from(b' ')
                } else {
                    u64::from(b)
                }
            })
            .sum();
        if sum != stored {
            return Err(format!("the header at byte {at} fails its checksum"));
        }
        let size = tar_size(&header[124..136])
            .ok_or_else(|| format!("the header at byte {at} has an unreadable size"))?;
        let mut name = tar_text(&header[..100]);
        let prefix = tar_text(&header[345..500]);
        if !prefix.is_empty() {
            name = format!("{prefix}/{name}");
        }
        let regular = matches!(header[156], b'0' | 0);
        if regular && matches!(name.as_str(), "manifest.json" | "./manifest.json") {
            return Ok(());
        }
        let body = size
            .div_ceil(512)
            .checked_mul(512)
            .ok_or("an entry is too large")?;
        at = at
            .checked_add(512)
            .and_then(|a| a.checked_add(body))
            .ok_or("an entry is too large")?;
        if at > len {
            return Err(format!("an entry runs past the end of the {len}-byte file"));
        }
    }
    Err(format!(
        "no manifest.json in its first {MAX_TAR_ENTRIES} entries: not a `docker save` archive"
    ))
}

/// What the plan knows about a golden image to import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoldenProbe {
    /// The bundle directory as given.
    pub path: PathBuf,
    /// The name it is stored under, or why that name is refused.
    pub name: Result<String, String>,
    /// Each of [`GOLDEN_FILES`], then a present [`GOLDEN_OPTIONAL`], with its
    /// SHA-256, in that order, or why the bundle is refused.
    pub source: Result<Vec<(String, String)>, String>,
    /// What is in the store under that name already.
    pub installed: StoredBundle,
}

/// A bundle in the store, as found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoredBundle {
    Missing,
    /// Its files and their SHA-256, or why it could not be read as a bundle.
    Present(Result<Vec<(String, String)>, String>),
}

/// What the plan knows about a runner tarball to import.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TarProbe {
    pub path: PathBuf,
    /// The file name it is stored under, or why it is refused.
    pub name: Result<String, String>,
    /// Its SHA-256 and length, or why it is refused.
    pub source: Result<(String, u64), String>,
    /// The SHA-256 of a file already stored under that name, if any, or why
    /// it could not be read.
    pub installed: Option<Result<String, String>>,
}

/// The golden image step's verdict: `Ok(None)` when it is in place,
/// `Ok(Some(replace))` when it is to be imported, or a refusal.
pub fn golden_verdict(g: &GoldenProbe, replace_ok: bool) -> Result<Option<bool>, String> {
    let name = g.name.as_ref().map_err(Clone::clone)?;
    let files = g
        .source
        .as_ref()
        .map_err(|why| format!("{}: {why}", g.path.display()))?;
    match &g.installed {
        StoredBundle::Missing => Ok(Some(false)),
        StoredBundle::Present(Ok(have)) if have == files => Ok(None),
        StoredBundle::Present(_) if replace_ok => Ok(Some(true)),
        StoredBundle::Present(Ok(_)) => Err(format!(
            "{} holds a different image under the name {name}: pass --replace-golden to replace \
             it, or --golden-name to import this one beside it",
            golden_dir().display()
        )),
        StoredBundle::Present(Err(why)) => Err(format!(
            "{} is there and is not a bundle setup recognises ({why}): pass --replace-golden to \
             replace it, or --golden-name to import beside it",
            golden_dir().join(name).display()
        )),
    }
}

/// The runner tarball step's verdict: `Ok(false)` when it is in place,
/// `Ok(true)` when it is to be copied, or a refusal.
pub fn tar_verdict(t: &TarProbe) -> Result<bool, String> {
    let name = t.name.as_ref().map_err(Clone::clone)?;
    let (sha, _) = t
        .source
        .as_ref()
        .map_err(|why| format!("{}: {why}", t.path.display()))?;
    match &t.installed {
        None => Ok(true),
        Some(Ok(have)) if have == sha => Ok(false),
        Some(Ok(_)) => Err(format!(
            "{} holds a different file under that name. It is a runner image's backup and is \
             never overwritten: rename this one and re-run",
            runner_dir().join(name).display()
        )),
        Some(Err(why)) => Err(format!(
            "{} is there and could not be read ({why})",
            runner_dir().join(name).display()
        )),
    }
}

/// The probes, as root on macOS. Each reads only; a refusal is carried into
/// the plan as its reason.
#[cfg(target_os = "macos")]
pub mod probe {
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};

    use super::super::fsx::{self, Dir};
    use super::*;

    /// Which copy of a file a member is read from.
    #[derive(Clone, Copy)]
    pub(crate) enum Origin {
        /// An import source: a regular file with one link, so a link planted
        /// to some other file is refused.
        Source,
        /// The copy in the store, which setup made `owner`'s and read-only
        /// (root, in setup). `_faber_vm` can hard-link any file there into its own
        /// home, which adds a link but changes nothing in the inode, so
        /// links are fine. The inode must still be `owner`'s with no write
        /// bit, or something other than setup wrote it.
        Installed { owner: u32 },
    }

    /// The SHA-256 of the regular file `name` in `dir`, opened without
    /// following a symlink, and judged as [`Origin`] says.
    pub(crate) fn hash_member(dir: &Dir, name: &str, copy: Origin) -> Result<String, String> {
        let file = dir
            .open_file(name)
            .map_err(|e| format!("{name}: {e} (a symlink is refused)"))?;
        let meta = file.metadata().map_err(|e| format!("{name}: {e}"))?;
        if !meta.file_type().is_file() {
            return Err(format!("{name} is not a regular file"));
        }
        if GOLDEN_OPTIONAL.contains(&name) && meta.len() > MAX_OPTIONAL_BYTES {
            return Err(format!(
                "{name} is {} bytes; setup takes at most {MAX_OPTIONAL_BYTES}",
                meta.len()
            ));
        }
        match copy {
            Origin::Source if meta.nlink() != 1 => {
                return Err(format!(
                    "{name} has {} links; setup imports only a file with one",
                    meta.nlink()
                ));
            }
            Origin::Installed { owner } if meta.uid() != owner || meta.mode() & 0o222 != 0 => {
                return Err(format!(
                    "{name} is owned by uid {} at mode {:o}, not by uid {owner} without a write bit: \
                     something other than setup changed it",
                    meta.uid(),
                    meta.mode() & 0o7777
                ));
            }
            _ => {}
        }
        if GOLDEN_OPTIONAL.contains(&name) {
            // Bound the read itself: the file may have grown since `metadata`.
            use std::io::Read;
            let mut bounded = (&file).take(MAX_OPTIONAL_BYTES + 1);
            let sha = fsx::sha256_of(&mut bounded).map_err(|e| format!("reading {name}: {e}"))?;
            if bounded.limit() == 0 {
                return Err(format!(
                    "{name} is over {MAX_OPTIONAL_BYTES} bytes; setup takes at most that"
                ));
            }
            return Ok(sha);
        }
        fsx::sha256_of(&mut &file).map_err(|e| format!("reading {name}: {e}"))
    }

    /// The entry names of `dir`, as UTF-8.
    fn entries(path: &Path) -> Result<Vec<String>, String> {
        let mut out = Vec::new();
        for e in std::fs::read_dir(path).map_err(|e| e.to_string())? {
            let e = e.map_err(|e| e.to_string())?;
            out.push(
                e.file_name()
                    .into_string()
                    .map_err(|n| format!("{n:?} is not UTF-8"))?,
            );
        }
        Ok(out)
    }

    /// A bundle's files and their SHA-256, in [`bundle_members`] order.
    pub(crate) fn bundle(path: &Path, copy: Origin) -> Result<Vec<(String, String)>, String> {
        let dir = fsx::open_leaf_dir(path).map_err(|e| format!("{e} (a symlink is refused)"))?;
        let entries = entries(path)?;
        if let Some(problem) = bundle_entries_problem(&entries) {
            return Err(problem);
        }
        bundle_members(&entries)
            .into_iter()
            .map(|f| Ok((f.to_string(), hash_member(&dir, f, copy)?)))
            .collect()
    }

    /// `--golden-image PATH [--golden-name NAME]`.
    pub fn golden(path: &Path, name: Option<&str>) -> GoldenProbe {
        let name = match name {
            Some(n) => Ok(n.to_string()),
            None => path
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string)
                .ok_or_else(|| {
                    format!(
                        "{} has no directory name to use: pass --golden-name",
                        path.display()
                    )
                }),
        }
        .and_then(|n| golden_name_problem(&n).map_or(Ok(n), Err));
        let source = if path.is_absolute() {
            bundle(path, Origin::Source)
        } else {
            Err("give the bundle as an absolute path".into())
        };
        let installed = match &name {
            Ok(n) => {
                let at = golden_dir().join(n);
                if std::fs::symlink_metadata(&at).is_err() {
                    StoredBundle::Missing
                } else {
                    StoredBundle::Present(bundle(
                        &at,
                        Origin::Installed {
                            owner: fsx::ROOT.uid,
                        },
                    ))
                }
            }
            Err(_) => StoredBundle::Missing,
        };
        GoldenProbe {
            path: path.to_path_buf(),
            name,
            source,
            installed,
        }
    }

    /// The tarball at `path`, judged and hashed.
    fn tarball(path: &Path) -> Result<(String, u64), String> {
        if !path.is_absolute() {
            return Err("give the tarball as an absolute path".into());
        }
        let (parent, name) = fsx::split(path).map_err(|e| e.to_string())?;
        let dir = Dir::open(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
        let mut file = dir
            .open_file(name)
            .map_err(|e| format!("{e} (a symlink is refused)"))?;
        let meta = file.metadata().map_err(|e| e.to_string())?;
        if !meta.file_type().is_file() {
            return Err("not a regular file".into());
        }
        check_docker_archive(&mut file, meta.len())?;
        file.seek(std::io::SeekFrom::Start(0))
            .map_err(|e| e.to_string())?;
        let sha = fsx::sha256_of(&mut file).map_err(|e| e.to_string())?;
        Ok((sha, meta.len()))
    }

    /// `--runner-image-tar PATH`.
    pub fn runner_tar(path: &Path) -> TarProbe {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string)
            .ok_or_else(|| format!("{} has no file name", path.display()))
            .and_then(|n| tar_name_problem(&n).map_or(Ok(n), Err));
        let installed = name.as_ref().ok().and_then(|n| {
            let at: PathBuf = runner_dir().join(n);
            std::fs::symlink_metadata(&at).ok()?;
            Some((|| {
                let dir = fsx::open_leaf_dir(&runner_dir()).map_err(|e| e.to_string())?;
                hash_member(
                    &dir,
                    n,
                    Origin::Installed {
                        owner: fsx::ROOT.uid,
                    },
                )
            })())
        });
        TarProbe {
            path: path.to_path_buf(),
            name,
            source: tarball(path),
            installed,
        }
    }
}

/// The imports, as root on macOS.
#[cfg(target_os = "macos")]
pub mod apply {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::path::Path;

    use anyhow::{anyhow, bail, Context, Result};

    use super::super::fsx::{self, Account, Dir, ROOT};
    use super::*;

    /// Errors a clone falls back to a copy on: another volume, or no clones.
    const EXDEV: i32 = 18;
    const ENOTSUP: i32 = 45;

    /// Put the source file `name` (from `src`) into the staging directory
    /// `stage` (at `stage_path`), root:wheel 0444, as exactly the bytes whose
    /// SHA-256 is `want`: a clone when the volume allows, else a copy.
    fn land_member(
        src: &Dir,
        stage: &Dir,
        stage_path: &Path,
        name: &str,
        want: &str,
    ) -> Result<()> {
        let source = src
            .open_file(name)
            .with_context(|| format!("opening {name}"))?;
        let meta = source.metadata()?;
        if !meta.file_type().is_file() || meta.nlink() != 1 {
            bail!("{name} is no longer a regular file with one link");
        }
        if let Err(e) = fsx::clone_file(&source, stage, name) {
            if !matches!(e.raw_os_error(), Some(EXDEV | ENOTSUP)) {
                return Err(anyhow!(e).context(format!("cloning {name}")));
            }
            // A copy root writes into a fresh file of its own carries no ACL.
            let mut from = &source;
            stage
                .write_atomic_expecting(name, &mut from, ROOT, 0o444, want)
                .with_context(|| format!("copying {name}"))?;
            return Ok(());
        }
        let clone = stage.open_file(name)?;
        let cm = clone.metadata()?;
        // This process's own new file: root's, in setup.
        let me = crate::platform::record::current_euid();
        if let Some(p) = fsx::append_problem(
            &stage_path.join(name),
            cm.is_file(),
            cm.uid(),
            cm.nlink(),
            me,
        ) {
            bail!("the clone is not this process's own new file: {p}");
        }
        fsx::acl::strip(&clone).context("clearing the clone's ACL and attributes")?;
        let got = fsx::sha256_of(&mut &clone)?;
        if got != want {
            bail!("{name} changed after the plan was made (its SHA-256 is now {got})");
        }
        std::os::unix::fs::fchown(&clone, Some(0), Some(0))?;
        clone.set_permissions(std::fs::Permissions::from_mode(0o444))?;
        clone.sync_all()?;
        Ok(())
    }

    /// Import the golden image `g` the plan judged (see the module docs).
    /// `replace` is the plan's: a bundle there already is renamed aside and
    /// removed once the new one is in place.
    pub fn import_golden(g: &GoldenProbe, replace: bool, ts: &str) -> Result<String> {
        let name = g.name.clone().map_err(|e| anyhow!(e))?;
        let files = g.source.clone().map_err(|e| anyhow!(e))?;
        let store_path = canonical(&golden_dir());
        let store = fsx::root_dir_chain(&store_path)
            .with_context(|| format!("preparing {}", store_path.display()))?;
        let src =
            fsx::open_leaf_dir(&g.path).with_context(|| format!("opening {}", g.path.display()))?;
        let staging = format!(".import-{name}-{ts}");
        if !store.mkdir(&staging)? {
            bail!("{} exists already", store_path.join(&staging).display());
        }
        let stage_path = store_path.join(&staging);
        let landed = (|| -> Result<()> {
            let stage = store.subdir(&staging)?;
            for (file, sha) in &files {
                land_member(&src, &stage, &stage_path, file, sha)?;
            }
            stage.set_owner_mode(ROOT, 0o755)?;
            let aside = format!(".replaced-{name}-{ts}");
            let had = store.exists(&name)?;
            if had && !replace {
                bail!(
                    "{} appeared while setup ran; not replacing it",
                    store_path.join(&name).display()
                );
            }
            if had {
                store.move_aside(&name, &aside)?;
            }
            swap_in(&store, &staging, &name, had.then_some(aside.as_str()))?;
            if had {
                std::fs::remove_dir_all(store_path.join(&aside)).with_context(|| {
                    format!(
                        "removing the replaced image {}",
                        store_path.join(&aside).display()
                    )
                })?;
            }
            Ok(())
        })();
        if landed.is_err() {
            let _ = std::fs::remove_dir_all(&stage_path);
        }
        landed?;
        Ok(name)
    }

    /// Rename `staging` to `name`. When that fails and the old image was moved
    /// to `aside`, put it back through the same descriptor, so a failed
    /// replace never leaves nothing at `name`; both errors are reported when
    /// the restore fails too.
    pub(super) fn swap_in(
        store: &fsx::Dir,
        staging: &str,
        name: &str,
        aside: Option<&str>,
    ) -> Result<()> {
        let Err(e) = store.rename(staging, name) else {
            return Ok(());
        };
        let Some(aside) = aside else {
            return Err(e.into());
        };
        match store.rename(aside, name) {
            Ok(()) => Err(anyhow!(
                "installing {name} failed ({e}); the previous image is back in place"
            )),
            Err(r) => Err(anyhow!(
                "installing {name} failed ({e}), and putting the previous image back from {aside} \
                 failed too ({r}); rename it by hand (`sudo mv`) before re-running"
            )),
        }
    }

    /// Copy the runner tarball `t` the plan judged into
    /// `IMAGE_STORE/runner`, `root:_faber` 0440 in a `root:_faber` 0750
    /// directory, as exactly the bytes the plan hashed.
    pub fn import_runner_tar(t: &TarProbe, faber_gid: u32) -> Result<String> {
        let name = t.name.clone().map_err(|e| anyhow!(e))?;
        let (sha, _) = t.source.clone().map_err(|e| anyhow!(e))?;
        let store_path = canonical(Path::new(IMAGE_STORE));
        let store = fsx::root_dir_chain(&store_path)
            .with_context(|| format!("preparing {}", store_path.display()))?;
        let group = Account {
            uid: 0,
            gid: faber_gid,
        };
        let (dir, _) = fsx::ensure_dir(&store, RUNNER_DIR, group, 0o750, &[0])
            .with_context(|| format!("preparing {}", runner_dir().display()))?;
        if dir.exists(&name)? {
            bail!(
                "{} appeared while setup ran; a backup is never overwritten",
                runner_dir().join(&name).display()
            );
        }
        let (parent, file) = fsx::split(&t.path)?;
        let src_dir = Dir::open(parent)?;
        let src = src_dir
            .open_file(file)
            .with_context(|| format!("opening {}", t.path.display()))?;
        if !src.metadata()?.file_type().is_file() {
            bail!("{} is no longer a regular file", t.path.display());
        }
        let mut from = &src;
        dir.write_atomic_expecting(&name, &mut from, group, 0o440, &sha)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::InvalidData => anyhow!(
                    "{} changed after the plan was made ({e}); nothing was stored",
                    t.path.display()
                ),
                _ => anyhow!(e),
            })?;
        Ok(name)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::setup::fsx::tests::Scratch;

        /// A replace whose final rename fails puts the old image back; with
        /// nothing to put back, the error is just the rename's.
        #[test]
        fn a_failed_swap_restores_the_old_image() {
            let s = Scratch::new("golden-swap");
            std::fs::create_dir(s.path("img.replaced")).unwrap();
            std::fs::write(s.path("img.replaced/disk"), b"old").unwrap();
            let store = s.dir();
            // The staging directory is gone: the rename must fail.
            let e = swap_in(&store, "missing-stage", "img", Some("img.replaced")).unwrap_err();
            assert!(e.to_string().contains("back in place"), "{e}");
            assert_eq!(std::fs::read(s.path("img/disk")).unwrap(), b"old");
            assert!(!s.path("img.replaced").exists());
            // Nothing moved aside, nothing to restore.
            let e = swap_in(&store, "missing-stage", "other", None).unwrap_err();
            assert!(!e.to_string().contains("back"), "{e}");
            // The restore failing too reports both.
            let e = swap_in(&store, "missing-stage", "x", Some("missing-aside")).unwrap_err();
            assert!(e.to_string().contains("failed too"), "{e}");
        }

        /// The manifest lands through the same path as a required member.
        #[test]
        fn the_manifest_lands_like_the_other_members() {
            use std::os::unix::fs::PermissionsExt;
            let s = Scratch::new("golden-manifest-land");
            std::fs::create_dir(s.path("src")).unwrap();
            std::fs::create_dir(s.path("stage")).unwrap();
            std::fs::write(s.path("src/manifest.json"), b"{}").unwrap();
            let src = Dir::open_nofollow(&s.path("src")).unwrap();
            let stage = Dir::open_nofollow(&s.path("stage")).unwrap();
            let want = fsx::sha256_of(&mut &b"{}"[..]).unwrap();
            // A non-root test cannot chown to root, so the final mode is
            // only reached as root; the content always lands.
            let r = land_member(&src, &stage, &s.path("stage"), "manifest.json", &want);
            let landed = s.path("stage/manifest.json");
            assert_eq!(std::fs::read(&landed).unwrap(), b"{}");
            if r.is_ok() {
                let mode = std::fs::metadata(&landed).unwrap().permissions().mode();
                assert_eq!(mode & 0o7777, 0o444);
            }
        }

        /// A clone of a file carrying an ACL entry and attributes loses them
        /// all before it is given its mode.
        #[test]
        fn a_clone_is_stripped_of_acls_and_attributes() {
            let s = Scratch::new("golden-strip");
            std::fs::create_dir(s.path("src")).unwrap();
            std::fs::create_dir(s.path("stage")).unwrap();
            std::fs::write(s.path("src/disk"), b"golden").unwrap();
            // An attribute on the source, which a clone carries over.
            let f = std::fs::File::open(s.path("src/disk")).unwrap();
            use std::ffi::CString;
            use std::os::fd::AsRawFd;
            use std::os::raw::{c_char, c_int};
            extern "C" {
                fn fsetxattr(
                    fd: c_int,
                    name: *const c_char,
                    value: *const std::ffi::c_void,
                    size: usize,
                    position: u32,
                    options: c_int,
                ) -> c_int;
            }
            let n = CString::new("org.example.planted").unwrap();
            // SAFETY: test-only FFI on an open descriptor and live strings.
            let rc = unsafe { fsetxattr(f.as_raw_fd(), n.as_ptr(), b"v".as_ptr().cast(), 1, 0, 0) };
            assert_eq!(rc, 0, "{}", std::io::Error::last_os_error());
            let src = Dir::open_nofollow(&s.path("src")).unwrap();
            let stage = Dir::open_nofollow(&s.path("stage")).unwrap();
            let want = fsx::sha256_of(&mut &b"golden"[..]).unwrap();
            // As a non-root test the final chown to root fails; everything
            // before it is what this checks.
            let r = land_member(&src, &stage, &s.path("stage"), "disk", &want);
            let landed = s.path("stage/disk");
            assert!(landed.exists(), "{r:?}");
            assert_eq!(std::fs::read(&landed).unwrap(), b"golden");
            let clone = std::fs::File::open(&landed).unwrap();
            assert!(
                !fsx::acl::xattr_names(clone.as_raw_fd())
                    .unwrap()
                    .iter()
                    .any(|n| n.to_bytes() == b"org.example.planted"),
                "the planted attribute survived the strip"
            );
            // A source that changed since the plan is refused.
            std::fs::remove_file(&landed).unwrap();
            let e = land_member(&src, &stage, &s.path("stage"), "disk", &"0".repeat(64));
            assert!(format!("{:#}", e.unwrap_err()).contains("changed after the plan"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// One ustar header for `name` of `size` bytes, with a valid checksum.
    fn header(name: &str, size: u64, kind: u8) -> [u8; 512] {
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..107].copy_from_slice(b"0000644");
        h[124..135].copy_from_slice(format!("{size:011o}").as_bytes());
        h[156] = kind;
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        h
    }

    fn archive(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut out = Vec::new();
        for (name, body) in entries {
            out.extend_from_slice(&header(name, body.len() as u64, b'0'));
            out.extend_from_slice(body);
            out.resize(out.len().div_ceil(512) * 512, 0);
        }
        out.extend_from_slice(&[0u8; 1024]);
        out
    }

    fn check(bytes: &[u8]) -> Result<(), String> {
        check_docker_archive(&mut Cursor::new(bytes), bytes.len() as u64)
    }

    #[test]
    fn a_docker_save_archive_has_a_manifest() {
        let ok = archive(&[
            ("abc123/layer.tar", &[7u8; 1500][..]),
            ("abc123.json", b"{}"),
            ("manifest.json", b"[]"),
        ]);
        assert_eq!(check(&ok), Ok(()));
        let dotted = archive(&[("./manifest.json", b"[]")]);
        assert_eq!(check(&dotted), Ok(()));
    }

    #[test]
    fn anything_else_is_refused() {
        let none = archive(&[("repositories", b"{}"), ("x/manifest.json", b"[]")]);
        assert!(check(&none).unwrap_err().contains("no manifest.json"));
        assert!(check(&[0x1f, 0x8b, 8, 0]).is_err());
        let mut gz = vec![0x1f, 0x8b];
        gz.resize(4096, 0);
        assert!(check(&gz).unwrap_err().contains("gzip"));
        assert!(check(&[0u8; 100]).is_err());
        let mut bad = archive(&[("manifest.json", b"[]")]);
        bad[0] = b'M';
        assert!(check(&bad).unwrap_err().contains("checksum"));
        // An entry that claims more bytes than the file has.
        let mut long = header("big.tar", 1 << 30, b'0').to_vec();
        long.extend_from_slice(&[0u8; 1024]);
        assert!(check(&long).unwrap_err().contains("past the end"));
        // A directory entry named manifest.json is not the manifest.
        let mut dir = header("manifest.json", 0, b'5').to_vec();
        dir.extend_from_slice(&[0u8; 1024]);
        assert!(check(&dir).is_err());
    }

    #[test]
    fn gnu_base256_sizes_are_read() {
        let mut f = [0u8; 12];
        f[0] = 0x80;
        f[11] = 0x02;
        f[10] = 0x01;
        assert_eq!(tar_size(&f), Some(0x0102));
        assert_eq!(tar_size(b"00000000017\0"), Some(15));
        assert_eq!(tar_size(b"zz"), None);
    }

    #[test]
    fn golden_names_follow_the_image_grammar() {
        for ok in ["sonoma", "macos-27.0.1", "a", "x_y", &"a".repeat(64)] {
            assert_eq!(golden_name_problem(ok), None, "{ok}");
        }
        for reserved in RESERVED_NAMES {
            let why = golden_name_problem(reserved).expect(reserved);
            assert!(why.contains("reserved"), "{why}");
        }
        // The store's own entries: the Linux image, the tarballs, staging.
        for bad in [
            "linux",
            "runner",
            ".staging",
            ".import-x-1",
            "",
            "Sonoma",
            "-x",
            ".x",
            "a..b",
            "a/b",
            "a b",
            &"a".repeat(65),
            "é",
        ] {
            assert!(golden_name_problem(bad).is_some(), "{bad}");
        }
    }

    #[test]
    fn tar_names_are_plain_file_names() {
        assert_eq!(tar_name_problem("runner-2.330.0+x.tar"), None);
        for bad in ["", ".hidden.tar", "a/b.tar", "a b.tar", &"a".repeat(129)] {
            assert!(tar_name_problem(bad).is_some(), "{bad}");
        }
    }

    #[test]
    fn a_bundle_holds_exactly_the_four_files() {
        let all: Vec<String> = GOLDEN_FILES.iter().map(|s| s.to_string()).collect();
        assert_eq!(bundle_entries_problem(&all), None);
        let mut extra = all.clone();
        extra.push("disk.raw".into());
        assert!(bundle_entries_problem(&extra)
            .unwrap()
            .contains("also there: disk.raw"));
        let missing = &all[1..];
        assert!(bundle_entries_problem(missing)
            .unwrap()
            .contains("missing required: disk"));
    }

    #[test]
    fn a_bundle_may_also_hold_a_manifest() {
        let req: Vec<String> = GOLDEN_FILES.iter().map(|s| s.to_string()).collect();
        let mut with = req.clone();
        with.push("manifest.json".into());
        assert_eq!(bundle_entries_problem(&with), None);
        assert_eq!(bundle_members(&req), GOLDEN_FILES.to_vec());
        let mut want = GOLDEN_FILES.to_vec();
        want.push("manifest.json");
        assert_eq!(bundle_members(&with), want);
        // The manifest does not stand in for a required file, and strays stay refused.
        let mut no_disk = with[1..].to_vec();
        let p = bundle_entries_problem(&no_disk).unwrap();
        assert!(p.contains("missing required: disk"), "{p}");
        no_disk.push("notes.txt".into());
        assert!(bundle_entries_problem(&no_disk)
            .unwrap()
            .contains("also there: notes.txt"));
    }

    #[test]
    fn a_manifest_alone_changes_the_verdict() {
        let mut with = files("a");
        with.push(("manifest.json".into(), "m".repeat(64)));
        // The source has a manifest, the stored image has none: a different bundle.
        let mut g = golden(StoredBundle::Present(Ok(files("a"))));
        g.source = Ok(with.clone());
        assert!(golden_verdict(&g, false)
            .unwrap_err()
            .contains("--replace-golden"));
        assert_eq!(golden_verdict(&g, true), Ok(Some(true)));
        // The reverse.
        let mut g = golden(StoredBundle::Present(Ok(with.clone())));
        assert_eq!(golden_verdict(&g, true), Ok(Some(true)));
        // Only the manifest's hash differs.
        let mut other = with.clone();
        other[4].1 = "n".repeat(64);
        g.source = Ok(with.clone());
        g.installed = StoredBundle::Present(Ok(other));
        assert_eq!(golden_verdict(&g, true), Ok(Some(true)));
        // The same manifest: in place.
        g.installed = StoredBundle::Present(Ok(with));
        assert_eq!(golden_verdict(&g, false), Ok(None));
    }

    #[test]
    fn canonical_puts_var_and_etc_under_private() {
        assert_eq!(
            canonical(Path::new("/var/db/faber/images")),
            PathBuf::from("/private/var/db/faber/images")
        );
        assert_eq!(
            canonical(Path::new("/etc/faber/wall")),
            PathBuf::from("/private/etc/faber/wall")
        );
        assert_eq!(
            canonical(Path::new("/Library/x")),
            PathBuf::from("/Library/x")
        );
        assert_eq!(canonical(Path::new("/variant")), PathBuf::from("/variant"));
    }

    fn files(sha: &str) -> Vec<(String, String)> {
        GOLDEN_FILES
            .iter()
            .map(|f| (f.to_string(), sha.to_string()))
            .collect()
    }

    fn golden(installed: StoredBundle) -> GoldenProbe {
        GoldenProbe {
            path: "/Users/owner/golden".into(),
            name: Ok("golden".into()),
            source: Ok(files("a")),
            installed,
        }
    }

    #[test]
    fn the_golden_verdict() {
        assert_eq!(
            golden_verdict(&golden(StoredBundle::Missing), false),
            Ok(Some(false))
        );
        let same = golden(StoredBundle::Present(Ok(files("a"))));
        assert_eq!(golden_verdict(&same, false), Ok(None));
        assert_eq!(golden_verdict(&same, true), Ok(None));
        let other = golden(StoredBundle::Present(Ok(files("b"))));
        assert!(golden_verdict(&other, false)
            .unwrap_err()
            .contains("--replace-golden"));
        assert_eq!(golden_verdict(&other, true), Ok(Some(true)));
        let broken = golden(StoredBundle::Present(Err("missing: disk".into())));
        assert!(golden_verdict(&broken, false).is_err());
        let mut bad_name = golden(StoredBundle::Missing);
        bad_name.name = Err("bad name".into());
        assert_eq!(golden_verdict(&bad_name, true), Err("bad name".into()));
        let mut bad_src = golden(StoredBundle::Missing);
        bad_src.source = Err("vz-aux is not a regular file".into());
        assert!(golden_verdict(&bad_src, true)
            .unwrap_err()
            .contains("vz-aux"));
    }

    #[test]
    fn the_tar_verdict_never_overwrites_a_backup() {
        let t = |installed| TarProbe {
            path: "/Users/owner/runner.tar".into(),
            name: Ok("runner.tar".into()),
            source: Ok(("a".into(), 10)),
            installed,
        };
        assert_eq!(tar_verdict(&t(None)), Ok(true));
        assert_eq!(tar_verdict(&t(Some(Ok("a".into())))), Ok(false));
        assert!(tar_verdict(&t(Some(Ok("b".into()))))
            .unwrap_err()
            .contains("never overwritten"));
        assert!(tar_verdict(&t(Some(Err("x".into())))).is_err());
    }
}

/// The installed copy tolerates extra links; the import source does not.
#[cfg(all(test, target_os = "macos"))]
mod installed_copy_tests {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    use super::probe::{bundle, hash_member, Origin};
    use super::{GOLDEN_FILES, MAX_OPTIONAL_BYTES};
    use crate::setup::fsx::tests::{me, Scratch};

    /// A manifest is hashed after the four, only when present, and not past 1 MiB.
    #[test]
    fn the_manifest_is_hashed_when_present_and_bounded() {
        let s = Scratch::new("golden-manifest");
        for f in GOLDEN_FILES {
            std::fs::write(s.path(f), f.as_bytes()).unwrap();
        }
        let got = bundle(&s.path("."), Origin::Source).unwrap();
        assert_eq!(got.len(), 4);
        std::fs::write(s.path("manifest.json"), b"{}").unwrap();
        let got = bundle(&s.path("."), Origin::Source).unwrap();
        assert_eq!(got.len(), 5);
        assert_eq!(got[4].0, "manifest.json");
        assert_eq!(
            got[4].1,
            crate::setup::fsx::sha256_of(&mut &b"{}"[..]).unwrap()
        );
        let big = vec![b' '; MAX_OPTIONAL_BYTES as usize + 1];
        std::fs::write(s.path("manifest.json"), big).unwrap();
        let e = bundle(&s.path("."), Origin::Source).unwrap_err();
        assert!(e.contains("manifest.json is"), "{e}");
        let ok = vec![b' '; MAX_OPTIONAL_BYTES as usize];
        std::fs::write(s.path("manifest.json"), ok).unwrap();
        assert!(bundle(&s.path("."), Origin::Source).is_ok());
    }

    /// A manifest that is a symlink, a multiply-linked file or a directory is
    /// refused like any other member.
    #[test]
    fn a_manifest_that_is_not_a_plain_file_is_refused() {
        let s = Scratch::new("golden-manifest-odd");
        for f in GOLDEN_FILES {
            std::fs::write(s.path(f), f.as_bytes()).unwrap();
        }
        std::fs::write(s.path("real"), b"{}").unwrap();
        std::fs::create_dir(s.path("elsewhere")).unwrap();
        std::fs::write(s.path("elsewhere/real"), b"{}").unwrap();
        // Entries beside the bundle would be strays, so keep them out of it.
        let b = s.path("bundle");
        std::fs::create_dir(&b).unwrap();
        for f in GOLDEN_FILES {
            std::fs::write(b.join(f), f.as_bytes()).unwrap();
        }
        let m_in = b.join("manifest.json");
        std::os::unix::fs::symlink(s.path("real"), &m_in).unwrap();
        let e = bundle(&b, Origin::Source).unwrap_err();
        assert!(e.contains("manifest.json"), "{e}");
        std::fs::remove_file(&m_in).unwrap();
        std::fs::write(&m_in, b"{}").unwrap();
        std::fs::hard_link(&m_in, s.path("elsewhere/link")).unwrap();
        let e = bundle(&b, Origin::Source).unwrap_err();
        assert!(e.contains("2 links"), "{e}");
        std::fs::remove_file(&m_in).unwrap();
        std::fs::create_dir(&m_in).unwrap();
        let e = bundle(&b, Origin::Source).unwrap_err();
        assert!(e.contains("manifest.json"), "{e}");
    }

    /// `_faber_vm` linking a stored image into its home must not stop setup
    /// reading the stored bundle (#248), and must not make a source with a
    /// planted link acceptable.
    #[test]
    fn a_hardlink_to_the_installed_copy_does_not_block_it() {
        let s = Scratch::new("golden-nlink");
        let owner = me(&s).uid;
        std::fs::write(s.path("disk"), b"golden").unwrap();
        std::fs::set_permissions(s.path("disk"), std::fs::Permissions::from_mode(0o444)).unwrap();
        let dir = s.dir();
        let want = crate::setup::fsx::sha256_of(&mut &b"golden"[..]).unwrap();
        let installed = Origin::Installed { owner };
        assert_eq!(hash_member(&dir, "disk", installed).unwrap(), want);
        std::fs::hard_link(s.path("disk"), s.path("planted")).unwrap();
        assert_eq!(std::fs::metadata(s.path("disk")).unwrap().nlink(), 2);
        assert_eq!(hash_member(&dir, "disk", installed).unwrap(), want);
        // The same file as an import source is still refused.
        let e = hash_member(&dir, "disk", Origin::Source).unwrap_err();
        assert!(e.contains("2 links"), "{e}");
    }

    /// The installed copy must still be the owner's and read-only: a file
    /// someone could have written is not what setup stored.
    #[test]
    fn an_installed_file_someone_can_write_is_refused() {
        let s = Scratch::new("golden-writable");
        let owner = me(&s).uid;
        std::fs::write(s.path("disk"), b"golden").unwrap();
        let dir = s.dir();
        let e = hash_member(&dir, "disk", Origin::Installed { owner }).unwrap_err();
        assert!(e.contains("something other than setup changed it"), "{e}");
        std::fs::set_permissions(s.path("disk"), std::fs::Permissions::from_mode(0o444)).unwrap();
        let e = hash_member(&dir, "disk", Origin::Installed { owner: owner + 1 }).unwrap_err();
        assert!(e.contains("owned by uid"), "{e}");
    }
}
