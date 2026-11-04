//! The Linux VM's image (ADR-0026, "The Linux VM runs Docker Engine under the
//! same launcher"): `sudo faber setup` builds it, so no hand step is needed.
//!
//! - **The base** is Debian 13's `genericcloud` arm64 image, pinned here by
//!   build, URL and SHA-512 ([`BASE`]). The pin is the trust anchor; Debian's
//!   published `SHA512SUMS` is the cross-check the ADR asks for, and both must
//!   agree before a byte of the tarball is used.
//! - **The provisioning** happens once, in a throwaway build VM booted by the
//!   launcher as `_faber_vm`, from a cloud-init seed built out of the guest
//!   files in `launcher/linux/`. They are compiled into this binary
//!   (`include_str!`), never read from a checkout at run time, and their digest,
//!   [`PROVISION_DIGEST`], is part of what makes an image current.
//! - **The result belongs to root**: `IMAGE_STORE/linux/disk.raw`, mode 0444,
//!   then its marker `image.json`, mode 0444, written last, so a marker only
//!   ever stands beside a whole image. The directories are root's, 0755, so
//!   `_faber_vm` can read the image and never write it. Builds stage under
//!   `IMAGE_STORE/.staging/` (root, 0700) and land by rename; a failed build
//!   leaves the previous image where it was.
//!
//! **The build VM runs under the wall** (#236), with the rules of a job VM
//! that has no grants: its network ([`build_network`]) is one more network in
//! the wall's plan, and it lists no profile, so no grant can ever apply to it.
//! Setup refuses to boot it unless the stored rules hold that network in
//! exactly that shape and the checker's verdict is green for them
//! ([`wall_verdict`]), and the guest fetches nothing until DNS to its gateway
//! goes unanswered, which shows the anchor is bound to its bridge
//! ([`WALL_DOWN_LINE`]).
//!
//! Root never executes or parses what the guest produced: the disk the build VM
//! wrote is opened without following a symlink, checked to be `_faber_vm`'s
//! own regular file with one link, and copied. The only thing read from the
//! guest is its console log, bounded, and matched only by an exact line prefix
//! ([`judge_console`]).
//!
//! A release that bumps the pin or changes a guest file shows the image out of
//! date ([`judge`]) until setup runs again. The daemon judges the installed
//! image the same way, every few minutes ([`watch`]), for `/api/state`.
//!
//! Every effect is behind [`ImageBuildHost`], so [`build`] is tested with a
//! fake; the real host is macOS only. Linux hosts run no VMs and plan nothing.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::net::Ipv4Addr;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};

use crate::limiter::ImageStanding;
use crate::platform::layout::{Os, SystemLayout};
use crate::platform::record::MetaFacts;
use crate::setup::exe::PathFacts;
use crate::wall::counters::Label;
use crate::wall::model::{VmNetwork, VmSlot};
use crate::wall::render::{ungranted_lines, IF_TOKEN};
use crate::wall::report::{judge_at, Judgement, VerdictFile};
use crate::wall::stored::{StoredNetwork, StoredWall};
use crate::wall::trust::root_only;

use super::walled_vm::{terminate_signal, watch_launch_until, WatchParams};

// ---------------------------------------------------------------------------
// The pin
// ---------------------------------------------------------------------------

/// The Debian cloud build this release pins.
pub const BASE_BUILD: &str = "20261015-2632";
/// The tarball (it holds one member, `disk.raw`).
pub const BASE_URL: &str = "https://cloud.debian.org/images/cloud/trixie/20261015-2632/debian-13-genericcloud-arm64-20261015-2632.tar.xz";
/// Debian's published checksums for that build directory.
pub const BASE_SUMS_URL: &str =
    "https://cloud.debian.org/images/cloud/trixie/20261015-2632/SHA512SUMS";
/// The tarball's SHA-512, lowercase hex.
pub const BASE_SHA512: &str = "4da671c34466a35b69125cf20d880dcba9a9d22205d4cd55a394068e3931ff66921b480956b1ea9a04d3367db4d1c97e18527cbdd108817412471292e19c8da9";

/// One pinned base image: what [`build`] downloads and checks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BasePin {
    pub build: &'static str,
    pub url: &'static str,
    pub sums_url: &'static str,
    pub sha512: &'static str,
}

impl BasePin {
    /// The tarball's file name: the last segment of its URL, which is the
    /// name `SHA512SUMS` lists it under.
    pub fn file_name(&self) -> &'static str {
        self.url.rsplit('/').next().unwrap_or(self.url)
    }
}

/// This release's pin.
pub const BASE: BasePin = BasePin {
    build: BASE_BUILD,
    url: BASE_URL,
    sums_url: BASE_SUMS_URL,
    sha512: BASE_SHA512,
};

// ---------------------------------------------------------------------------
// The guest files
// ---------------------------------------------------------------------------

/// The NoCloud seed's `meta-data`.
pub const META_DATA: &str = include_str!("../../launcher/linux/meta-data");
/// The NoCloud seed's `network-config` (cloud-init v2), for the build VM only.
pub const NETWORK_CONFIG: &str = include_str!("../../launcher/linux/network-config");
/// The `user-data` template; [`user_data`] appends `write_files`.
pub const USER_DATA_TEMPLATE: &str = include_str!("../../launcher/linux/user-data");
/// Docker's apt signing key, ASCII-armoured.
pub const DOCKER_KEY: &str = include_str!("../../launcher/linux/docker-archive-keyring.asc");
/// The fingerprint of [`DOCKER_KEY`], as Docker publishes it. Documentation and
/// a check for whoever replaces the key file: apt verifies by the key itself.
pub const DOCKER_KEY_FINGERPRINT: &str = "9DC858229FC7DD38854AE2D88D81803C0EBFCD88";

/// A file cloud-init writes into the build VM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GuestFile {
    /// Its name in `launcher/linux/`.
    pub name: &'static str,
    /// Where cloud-init writes it.
    pub guest_path: &'static str,
    pub mode: u32,
    pub bytes: &'static str,
}

/// Every file cloud-init writes, in a fixed order (the digest depends on it).
pub const GUEST_FILES: [GuestFile; 11] = [
    GuestFile {
        name: "faber-docker-relay",
        guest_path: "/usr/local/libexec/faber-docker-relay",
        mode: 0o755,
        bytes: include_str!("../../launcher/linux/faber-docker-relay"),
    },
    GuestFile {
        name: "faber-docker-vsock.socket",
        guest_path: "/etc/systemd/system/faber-docker-vsock.socket",
        mode: 0o644,
        bytes: include_str!("../../launcher/linux/faber-docker-vsock.socket"),
    },
    GuestFile {
        name: "faber-docker-vsock.service",
        guest_path: "/etc/systemd/system/faber-docker-vsock.service",
        mode: 0o644,
        bytes: include_str!("../../launcher/linux/faber-docker-vsock.service"),
    },
    GuestFile {
        name: "docker-archive-keyring.asc",
        guest_path: "/usr/local/share/faber/docker.asc",
        mode: 0o644,
        bytes: DOCKER_KEY,
    },
    GuestFile {
        name: "10-faber.network",
        guest_path: "/usr/local/share/faber/10-faber.network",
        mode: 0o644,
        bytes: include_str!("../../launcher/linux/10-faber.network"),
    },
    GuestFile {
        name: "faber-resolved.conf",
        guest_path: "/usr/local/share/faber/faber-resolved.conf",
        mode: 0o644,
        bytes: include_str!("../../launcher/linux/faber-resolved.conf"),
    },
    GuestFile {
        name: "provision.sh",
        guest_path: "/usr/local/libexec/faber-provision",
        mode: 0o755,
        bytes: include_str!("../../launcher/linux/provision.sh"),
    },
    GuestFile {
        name: "faber-wall-probe",
        guest_path: "/usr/local/libexec/faber-wall-probe",
        mode: 0o755,
        bytes: include_str!("../../launcher/linux/faber-wall-probe"),
    },
    GuestFile {
        name: "faber-wall-probe.socket",
        guest_path: "/etc/systemd/system/faber-wall-probe.socket",
        mode: 0o644,
        bytes: include_str!("../../launcher/linux/faber-wall-probe.socket"),
    },
    GuestFile {
        name: "faber-wall-probe@.service",
        guest_path: "/etc/systemd/system/faber-wall-probe@.service",
        mode: 0o644,
        bytes: include_str!("../../launcher/linux/faber-wall-probe@.service"),
    },
    GuestFile {
        name: "daemon.json",
        guest_path: "/usr/local/share/faber/daemon.json",
        mode: 0o644,
        bytes: include_str!("../../launcher/linux/daemon.json"),
    },
];

/// Standard base64 with padding: cloud-init's `encoding: b64`, so no file's
/// text is ever parsed as YAML.
fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = |i: usize| u32::from(chunk.get(i).copied().unwrap_or(0));
        let n = (b(0) << 16) | (b(1) << 8) | b(2);
        let at = |shift: u32| char::from(ALPHABET[((n >> shift) & 63) as usize]);
        out.push(at(18));
        out.push(at(12));
        out.push(if chunk.len() > 1 { at(6) } else { '=' });
        out.push(if chunk.len() > 2 { at(0) } else { '=' });
    }
    out
}

/// The seed's `user-data`: the template, then a `write_files` entry for every
/// [`GUEST_FILES`] member, base64-encoded.
pub fn user_data() -> String {
    let mut out = USER_DATA_TEMPLATE.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("write_files:\n");
    for f in GUEST_FILES {
        out.push_str(&format!(
            "  - path: {}\n    owner: root:root\n    permissions: \"{:04o}\"\n    encoding: b64\n    content: {}\n",
            f.guest_path,
            f.mode,
            base64(f.bytes.as_bytes())
        ));
    }
    out
}

/// SHA-256 over `parts`, each as its label's length, the label, its bytes'
/// length and the bytes (lengths big-endian u64), so no two different lists
/// hash alike by shifting a boundary.
fn digest_of(parts: &[(String, &[u8])]) -> String {
    let mut h = Sha256::new();
    for (label, bytes) in parts {
        h.update((label.len() as u64).to_be_bytes());
        h.update(label.as_bytes());
        h.update((bytes.len() as u64).to_be_bytes());
        h.update(bytes);
    }
    format!("{:x}", h.finalize())
}

/// The digest of everything the build VM is given: the seed's three files and
/// every guest file with its path and mode. A release that changes any of them
/// makes the installed image out of date, the same as a new base pin.
pub static PROVISION_DIGEST: LazyLock<String> = LazyLock::new(|| {
    let mut parts: Vec<(String, &[u8])> = vec![
        ("meta-data".into(), META_DATA.as_bytes()),
        ("network-config".into(), NETWORK_CONFIG.as_bytes()),
        ("user-data".into(), USER_DATA_TEMPLATE.as_bytes()),
    ];
    for f in GUEST_FILES {
        parts.push((format!("{} {:o}", f.guest_path, f.mode), f.bytes.as_bytes()));
    }
    digest_of(&parts)
});

// ---------------------------------------------------------------------------
// The store and its marker
// ---------------------------------------------------------------------------

/// The Linux image's directory under the store.
pub const LINUX_DIR: &str = "linux";
/// The image itself.
pub const DISK_FILE: &str = "disk.raw";
/// Its marker, written last.
pub const MARKER_FILE: &str = "image.json";
/// The marker's schema.
pub const SCHEMA: u32 = 1;
/// The largest marker read back.
const MAX_MARKER: u64 = 4096;
/// The disk's size once grown, sparse: room for Docker's images and volumes;
/// cloud-init's growpart grows the root partition into it at build.
pub const DISK_BYTES: u64 = 64 << 30;
/// The largest base tarball accepted. Debian's is a few hundred MiB.
pub const MAX_TARBALL: u64 = 2 << 30;
/// The largest `SHA512SUMS` accepted.
pub const MAX_SUMS: u64 = 64 << 10;
/// How much of the build VM's console log is read: its tail.
pub const MAX_CONSOLE: u64 = 1 << 20;
/// How long the build VM may take, from boot to power-off.
pub const LAUNCH_TIMEOUT: Duration = Duration::from_secs(30 * 60);
/// How long the wall may take to bind the build network once the launcher
/// starts (#243). The checker binds a new bridge within one 5 s tick; this is
/// well inside the guest's own [`WALL_GATE_SECS`], so the host gives up first.
pub const BIND_DEADLINE: Duration = Duration::from_secs(60);
/// How often setup reads the checker's verdict while the build VM runs.
pub const WALL_POLL: Duration = Duration::from_secs(2);
/// The launcher slot the build VM uses: the last, so a job VM is never moved
/// off its own MAC. It is also the build network's one slot in the wall's plan
/// ([`build_network`]).
pub const BUILD_SLOT: u8 = 15;
/// The build VM's vmnet subnet. Not one the wall's network plan hands to job
/// VMs, not Docker's `172.16.0.0/12` pool or Docker Desktop's
/// `192.168.65.0/24`, and rarely a LAN's.
pub const BUILD_SUBNET: &str = "192.168.239.0/24";
/// The build network's id in the wall's plan.
pub const BUILD_NETWORK_ID: u8 = 16;
/// The build network's gateway: vmnet's, always the subnet's second address.
pub const BUILD_GATEWAY: Ipv4Addr = Ipv4Addr::new(192, 168, 239, 1);
/// The build VM's address: the launcher's reservation, the subnet's third
/// address, as for every job slot.
pub const BUILD_GUEST: Ipv4Addr = Ipv4Addr::new(192, 168, 239, 2);
/// The build bundle's directory name under `_faber_vm`'s home.
pub const BUNDLE_NAME: &str = "linux-image-build";
/// The launcher's state directory (slot locks) under `_faber_vm`'s home.
pub const STATE_DIR_NAME: &str = "state";
/// The console log's name inside the build bundle.
pub const CONSOLE_FILE: &str = "console.log";
/// Where setup keeps the console of a failed build, in the log directory.
pub const FAILED_CONSOLE_FILE: &str = "linux-image-build.console.log";
/// The seed's name inside a bundle, as the launcher reads it.
pub const SEED_FILE: &str = "seed.iso";

/// What `image.json` records. Root writes it; anyone reads it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Marker {
    pub schema: u32,
    pub base_build: String,
    pub base_sha512: String,
    pub provision_digest: String,
    pub built_at_unix: u64,
    pub docker_version: String,
}

/// A Debian cloud build id: `YYYYMMDD-N`.
pub fn valid_build(s: &str) -> bool {
    let Some((date, n)) = s.split_once('-') else {
        return false;
    };
    date.len() == 8
        && date.bytes().all(|b| b.is_ascii_digit())
        && !n.is_empty()
        && n.len() <= 10
        && n.bytes().all(|b| b.is_ascii_digit())
}

/// A version token as the guest reports Docker's: `[0-9A-Za-z.+~-]{1,64}`.
pub fn valid_version(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+~-".contains(&b))
}

fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

impl Marker {
    /// Why this marker is not one this release could have written, if it is not.
    fn problem(&self) -> Option<&'static str> {
        if self.schema != SCHEMA {
            Some("its marker has another schema")
        } else if !valid_build(&self.base_build) {
            Some("its marker's base build is malformed")
        } else if !is_lower_hex(&self.base_sha512, 128) || !is_lower_hex(&self.provision_digest, 64)
        {
            Some("its marker's digests are malformed")
        } else if !valid_version(&self.docker_version) {
            Some("its marker's Docker version is malformed")
        } else {
            None
        }
    }
}

/// What is installed, as read with the trust chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Installed {
    /// No marker: never built, or a build that never finished.
    Absent,
    /// A marker that cannot be trusted or read: something other than root could
    /// have written it or its image, or it does not parse.
    Unusable(String),
    Found(Marker),
}

/// How the installed image stands against this release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    NotBuilt,
    Current,
    OutOfDate(String),
}

impl Verdict {
    /// What `/api/state` shows. Never built is `unknown`, not out of date: a host
    /// that has not run this release's setup, or runs no VMs, is not stale.
    pub fn standing(&self) -> ImageStanding {
        match self {
            Verdict::NotBuilt => ImageStanding::Unknown,
            Verdict::Current => ImageStanding::Current,
            Verdict::OutOfDate(_) => ImageStanding::OutOfDate,
        }
    }
}

/// Judge `installed` against `pin` and `provision_digest`. Pure.
pub fn judge_against(installed: &Installed, pin: &BasePin, provision_digest: &str) -> Verdict {
    match installed {
        Installed::Absent => Verdict::NotBuilt,
        Installed::Unusable(why) => {
            Verdict::OutOfDate(format!("the installed image is unusable: {why}"))
        }
        Installed::Found(m) => {
            if let Some(why) = m.problem() {
                Verdict::OutOfDate(format!("the installed image is unusable: {why}"))
            } else if m.base_sha512 != pin.sha512 {
                Verdict::OutOfDate(format!(
                    "built from Debian {}; this release pins {}",
                    m.base_build, pin.build
                ))
            } else if m.provision_digest != provision_digest {
                Verdict::OutOfDate("provisioned from another release's guest files".into())
            } else {
                Verdict::Current
            }
        }
    }
}

/// [`judge_against`] this release's pin and guest files.
pub fn judge(installed: &Installed) -> Verdict {
    judge_against(installed, &BASE, &PROVISION_DIGEST)
}

/// Read what is installed under `store`, trusting nothing root alone could not
/// have written: the marker and the image must be regular files without a
/// write bit, and every directory above each must pass
/// [`crate::wall::trust::root_only`]. Read-only; the daemon calls it as
/// `_faber`.
pub fn read_installed(store: &Path) -> Installed {
    let linux = store.join(LINUX_DIR);
    let marker = linux.join(MARKER_FILE);
    match std::fs::symlink_metadata(&marker) {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Installed::Absent,
        Err(e) => return Installed::Unusable(format!("{}: {e}", marker.display())),
    }
    for path in [&marker, &linux.join(DISK_FILE)] {
        if let Err(why) = root_only_file(path) {
            return Installed::Unusable(why);
        }
    }
    let text = match read_marker_text(&linux) {
        Ok(text) => text,
        Err(e) => return Installed::Unusable(format!("{}: {e}", marker.display())),
    };
    match serde_json::from_str::<Marker>(&text) {
        Ok(m) => match m.problem() {
            None => Installed::Found(m),
            Some(why) => Installed::Unusable(why.to_string()),
        },
        Err(_) => Installed::Unusable(format!("{} does not parse", marker.display())),
    }
}

/// `path` is a regular file with no write bit and, on macOS, no extended ACL
/// (an entry such as `user:_faber_vm allow write` outranks the mode bits), and
/// root alone could have written it and every directory above it.
fn root_only_file(path: &Path) -> Result<(), String> {
    let meta = std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.file_type().is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if meta.mode() & 0o222 != 0 {
        return Err(format!("{} is writable", path.display()));
    }
    #[cfg(target_os = "macos")]
    {
        let shown = |e: std::io::Error| format!("{}: {e}", path.display());
        let (parent, name) = super::fsx::split(path).map_err(shown)?;
        let file = super::fsx::Dir::open_nofollow(parent)
            .and_then(|dir| dir.open_file(name))
            .map_err(shown)?;
        if super::fsx::acl::has_extended(&file).map_err(shown)? {
            return Err(format!(
                "{} carries an ACL, which could grant what its mode does not",
                path.display()
            ));
        }
    }
    let chain = super::exe::chain_of(path).map_err(|e| format!("{}: {e}", path.display()))?;
    crate::wall::trust::root_only(&chain)
}

fn read_marker_text(linux: &Path) -> std::io::Result<String> {
    let dir = super::fsx::Dir::open_nofollow(linux)?;
    let file = dir.open_file(MARKER_FILE)?;
    if !file.metadata()?.file_type().is_file() {
        return Err(std::io::Error::other("not a regular file"));
    }
    let mut text = String::new();
    file.take(MAX_MARKER).read_to_string(&mut text)?;
    Ok(text)
}

/// `path` with its longest existing ancestor resolved and the rest appended:
/// `/var/db/faber/images` is `/private/var/db/faber/images` on macOS, where
/// `/var` is a symlink, and a walk that refuses symlinks
/// ([`super::fsx::root_dir_chain`]) needs the real path. Only system
/// directories root owns are resolved: the store's own components are not
/// created yet, or are root's.
pub fn resolve_existing_prefix(path: &Path) -> std::io::Result<PathBuf> {
    let mut rest = Vec::new();
    let mut at = path;
    loop {
        match std::fs::canonicalize(at) {
            Ok(real) => {
                return Ok(rest.iter().rev().fold(real, |p: PathBuf, c| p.join(c)));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let (Some(parent), Some(name)) = (at.parent(), at.file_name()) else {
                    return Err(e);
                };
                rest.push(name.to_os_string());
                at = parent;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Where the VM launcher, `faber-vm`, is installed: `_faber`'s own, so a
/// release can update it (ADR-0026), in [`LAUNCHER_DIR`] outside `_faber`'s
/// state directory, which `_faber_vm`, who runs it here, cannot traverse
/// ([`SystemLayout::launcher_bin`]).
///
/// [`LAUNCHER_DIR`]: crate::platform::layout::LAUNCHER_DIR
pub fn launcher_path(layout: &SystemLayout) -> PathBuf {
    layout.launcher_bin.clone()
}

const S_IFMT: u32 = 0o170_000;
const S_IFDIR: u32 = 0o040_000;
const STICKY: u32 = 0o1000;
const GROUP_OR_WORLD_WRITE: u32 = 0o022;

/// `Ok` if every entry of `chain` (the launcher and each directory above it)
/// is owned by root or by `faber_uid`, and none is group- or world-writable
/// (a sticky directory excepted, as in [`super::exe::verdict`]).
pub fn launcher_chain_verdict(chain: &[PathFacts], faber_uid: u32) -> Result<(), String> {
    for f in chain {
        let shown = f.path.display();
        if f.uid != 0 && f.uid != faber_uid {
            return Err(format!(
                "{shown} is owned by uid {}, neither root nor _faber",
                f.uid
            ));
        }
        // `_faber_vm` runs the launcher as "other" on these root and `_faber`
        // entries: a directory it cannot search, or a launcher it cannot
        // read and execute, fails the build only after the plan said all was well.
        let want = if f.mode & S_IFMT == S_IFDIR {
            0o001
        } else {
            0o005
        };
        if f.mode & want != want {
            return Err(format!(
                "{shown} (mode {:o}) is not {} by other accounts, so _faber_vm could not run \
                 the launcher",
                f.mode & 0o7777,
                if want == 0o001 {
                    "searchable"
                } else {
                    "readable and executable"
                }
            ));
        }
        let sticky_dir = f.mode & S_IFMT == S_IFDIR && f.mode & STICKY != 0;
        if !sticky_dir && f.mode & GROUP_OR_WORLD_WRITE != 0 {
            return Err(format!(
                "{shown} is group- or world-writable (mode {:o})",
                f.mode & 0o7777
            ));
        }
    }
    Ok(())
}

/// Whether the launcher at `path` is installed and trusted: a regular,
/// executable file with one link, owned by root or `_faber` (`faber_uid`),
/// under directories of the same. `None` means `_faber` does not exist yet, so
/// nothing is installed.
///
/// Setup never executes the launcher as root, only as `_faber_vm` through
/// `tools::run_as`. A launcher `_faber` replaced could only yield a bad
/// image, which `_faber` can already cause through the Docker socket into the
/// Linux VM. What must never be trusted is anything `_faber_vm`, the VM-escape
/// account, can write.
pub fn launcher_trusted(path: &Path, faber_uid: Option<u32>) -> Result<(), String> {
    let Some(faber_uid) = faber_uid else {
        return Err(format!(
            "the VM launcher is not installed at {} yet (no _faber account)",
            path.display()
        ));
    };
    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!(
                "the VM launcher is not installed at {} yet",
                path.display()
            ))
        }
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    if !meta.file_type().is_file() || meta.mode() & 0o111 == 0 {
        return Err(format!(
            "{} is not an executable regular file",
            path.display()
        ));
    }
    if meta.nlink() != 1 {
        return Err(format!("{} has more than one link", path.display()));
    }
    let chain = super::exe::chain_of(path).map_err(|e| format!("{}: {e}", path.display()))?;
    launcher_chain_verdict(&chain, faber_uid)
}

// ---------------------------------------------------------------------------
// The plan
// ---------------------------------------------------------------------------

/// What setup's probe found, for the plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxImageProbe {
    /// The launcher is installed and trusted, or why not.
    pub launcher: Result<(), String>,
    pub installed: Installed,
}

/// Probe this host. macOS only does anything: Linux hosts run no VMs.
pub fn probe(os: Os, layout: &SystemLayout, faber_uid: Option<u32>) -> LinuxImageProbe {
    match os {
        Os::Linux => LinuxImageProbe {
            launcher: Err("Linux hosts run no VMs".into()),
            installed: Installed::Absent,
        },
        Os::MacOs => LinuxImageProbe {
            launcher: launcher_trusted(&launcher_path(layout), faber_uid),
            installed: read_installed(Path::new(crate::platform::layout::IMAGE_STORE)),
        },
    }
}

/// What setup does about the Linux image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinuxImagePlan {
    Current,
    /// Build it: `fresh` when there is none yet.
    Build {
        reason: String,
        fresh: bool,
    },
    /// It needs building, and cannot be built yet. A warning: setup goes on.
    Blocked {
        why: String,
    },
}

/// The plan for `probe` on `os`; `None` where there is nothing to plan (Linux).
pub fn plan_for(os: Os, probe: &LinuxImageProbe) -> Option<LinuxImagePlan> {
    if os != Os::MacOs {
        return None;
    }
    let (reason, fresh) = match judge(&probe.installed) {
        Verdict::Current => return Some(LinuxImagePlan::Current),
        Verdict::NotBuilt => ("not built".to_string(), true),
        Verdict::OutOfDate(why) => (why, false),
    };
    Some(match &probe.launcher {
        Ok(()) => LinuxImagePlan::Build { reason, fresh },
        Err(why) => LinuxImagePlan::Blocked {
            why: format!("{reason}, and it cannot be built yet: {why}"),
        },
    })
}

// ---------------------------------------------------------------------------
// The wall (#236)
// ---------------------------------------------------------------------------

/// The build VM's network, as the wall's plan lists it: one slot, the build
/// VM's address, and **no profile**, so no grant and no `drop_grants` can ever
/// apply to it. It renders as every other slot does: DHCP to the gateway, DNS
/// to the listed resolvers only, the fence, non-global ranges dropped, public
/// unicast passed. The host, the LAN and the tailnet are refused.
///
/// The plan setup writes must include it; until it does, [`wall_verdict`]
/// refuses and no build VM boots.
pub fn build_network() -> VmNetwork {
    VmNetwork {
        id: BUILD_NETWORK_ID,
        gateway: BUILD_GATEWAY,
        subnet: BUILD_SUBNET.parse().expect("BUILD_SUBNET is a network"),
        slots: vec![VmSlot {
            id: BUILD_SLOT,
            addr: BUILD_GUEST,
            profiles: Vec::new(),
        }],
    }
}

/// How old the checker's verdict may be when setup reads it. The checker
/// rewrites it every few seconds; an older one means it has stopped.
pub const VERDICT_MAX_AGE_SECS: u64 = 60;

/// What setup read about the wall, for [`wall_verdict`]. Each chain is the
/// file and every directory above it up to `/`
/// ([`crate::setup::exe::chain_of`]).
#[derive(Debug, Clone, Copy)]
pub struct WallEvidence<'a> {
    /// The stored rule set's bytes ([`crate::wall::paths::STORED`]).
    pub stored: &'a [u8],
    pub stored_chain: &'a [PathFacts],
    /// The checker's verdict file's bytes ([`crate::wall::paths::VERDICT`]).
    pub verdict: &'a [u8],
    pub verdict_facts: &'a MetaFacts,
    pub verdict_chain: &'a [PathFacts],
    pub now_unix: u64,
}

/// `Ok` only when the wall covers `net`: root alone could have written the
/// stored set, it holds `net` exactly (gateway, subnet, slots, no profile)
/// with exactly the rules [`ungranted_lines`] gives it, and the checker's
/// verdict is green, current and for that stored set's digest. Otherwise why
/// not, in words for the operator. Pure, so it is tested on every OS.
pub fn wall_verdict(net: &VmNetwork, e: &WallEvidence<'_>) -> Result<(), String> {
    use crate::wall::paths::{STORED, VERDICT};
    root_only(e.stored_chain)
        .map_err(|why| format!("the stored wall ({STORED}) is not root's alone: {why}"))?;
    let stored = StoredWall::from_json(e.stored).map_err(|why| format!("{STORED}: {why}"))?;
    let Some(have) = stored.networks.iter().find(|n| n.id == net.id) else {
        return Err(format!(
            "the stored wall has no network {} for the build VM ({}); re-run `sudo faber setup` \
             with a release whose network plan includes it",
            net.id, net.subnet
        ));
    };
    let mut slots = net.slots.clone();
    slots.sort_by_key(|s| s.id);
    if have.gateway != net.gateway || have.subnet != net.subnet || have.slots != slots {
        return Err(format!(
            "the stored wall's network {} is not the build network ({}, gateway {}, one slot \
             with no profile)",
            net.id, net.subnet, net.gateway
        ));
    }
    if have.lines != ungranted_lines(net, &stored_resolvers(have), &stored.plan()) {
        return Err(format!(
            "the stored rules for the build network ({}) are not those of a slot with no grant",
            net.subnet
        ));
    }
    match judge_at(
        e.verdict,
        e.verdict_facts,
        e.verdict_chain,
        e.now_unix,
        VERDICT_MAX_AGE_SECS,
        crate::wall::RULES_VERSION,
    ) {
        Judgement::Green => {}
        Judgement::Red(why) => return Err(why),
        Judgement::OutOfDate { installed, wanted } => {
            return Err(format!(
                "the stored rules are version {installed}, and this faber renders version \
                 {wanted}; re-run `sudo faber setup`"
            ))
        }
    }
    // `judge_at` parsed it already; this reads the one field it does not judge.
    let file: VerdictFile = serde_json::from_slice(e.verdict)
        .map_err(|why| format!("the verdict file ({VERDICT}) does not parse: {why}"))?;
    if file.digest != stored.digest {
        return Err(
            "the checker's green verdict is for another stored rule set than the one on disk"
                .into(),
        );
    }
    Ok(())
}

/// Whether the anchor already binds a network the wall covers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Binding {
    /// The checker's green verdict lists the network as pending: its bridge
    /// did not exist at its last check, so no rule is bound to it yet. True of
    /// the build network until its VM boots.
    Pending,
    /// The checker's green verdict, for the stored set on disk, no longer
    /// lists it as pending: the rules are bound to its bridge (#243).
    Bound,
}

/// [`wall_verdict`], and then where the network stands: `Ok(Pending)` while
/// the green verdict still lists `net` in `pending_networks`, `Ok(Bound)` once
/// it does not. Every refusal of [`wall_verdict`] is an `Err` here too. Pure.
pub fn wall_binding(net: &VmNetwork, e: &WallEvidence<'_>) -> Result<Binding, String> {
    wall_verdict(net, e)?;
    let file: VerdictFile = serde_json::from_slice(e.verdict).map_err(|why| {
        format!(
            "the verdict file ({}) does not parse: {why}",
            crate::wall::paths::VERDICT
        )
    })?;
    Ok(if file.pending_networks.contains(&net.id) {
        Binding::Pending
    } else {
        Binding::Bound
    })
}

/// The `[dns] resolvers` `n`'s rules were rendered with, read back from its
/// DNS passes. Only the resolvers: [`wall_verdict`] re-renders every line from
/// them and compares, so a line this does not recognise is still caught.
fn stored_resolvers(n: &StoredNetwork) -> BTreeSet<Ipv4Addr> {
    let mut out = BTreeSet::new();
    for s in &n.slots {
        let head = format!(
            "pass in quick on {IF_TOKEN} inet proto udp from {} to ",
            s.addr
        );
        let tail = format!(
            " port = 53 keep state label \"{}\"",
            Label::Dns {
                net: n.id,
                slot: s.id
            }
        );
        for line in &n.lines {
            if let Some(addr) = line
                .strip_prefix(&head)
                .and_then(|rest| rest.strip_suffix(&tail))
                .and_then(|mid| mid.parse().ok())
            {
                out.insert(addr);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The console
// ---------------------------------------------------------------------------

/// The prefix of the guest's success line.
pub const OK_PREFIX: &str = "FABER-LINUX-IMAGE ok docker=";
/// The prefix of the guest's failure line.
pub const FAILED_PREFIX: &str = "FABER-LINUX-IMAGE failed step=";
/// The guest's line when DNS to its gateway was still answered at the gate's
/// deadline: the wall was not bound to its bridge, so it fetched nothing and
/// powered off.
pub const WALL_DOWN_LINE: &str = "FABER-LINUX-IMAGE wall-not-up";
/// How long the guest waits for the wall, as `provision.sh` states it.
pub const WALL_GATE_SECS: u64 = 120;

/// The Docker version the build VM reported, if it succeeded: the launcher
/// exited 0, and the console (guest-controlled, so matched only by an exact line
/// prefix, and never echoed) holds an ok line with a valid version and no
/// failure line. A [`WALL_DOWN_LINE`] outranks everything else.
pub fn judge_console(code: Option<i32>, console: &[u8]) -> Result<String, BuildError> {
    let text = String::from_utf8_lossy(console);
    let mut version = None;
    let mut failed = None;
    let mut wall_down = false;
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if line == WALL_DOWN_LINE {
            wall_down = true;
        } else if let Some(rest) = line.strip_prefix(OK_PREFIX) {
            if valid_version(rest) {
                version = Some(rest.to_string());
            }
        } else if let Some(rest) = line.strip_prefix(FAILED_PREFIX) {
            failed = Some(rest.parse::<u32>().ok().filter(|n| (1..=99).contains(n)));
        }
    }
    if wall_down {
        return Err(BuildError::WallNotUp { code });
    }
    match (code, failed, version) {
        (_, Some(step), _) => Err(BuildError::Failed { step, code }),
        (Some(0), None, Some(v)) => Ok(v),
        (_, None, _) => Err(BuildError::NoResult { code }),
    }
}

// ---------------------------------------------------------------------------
// The build
// ---------------------------------------------------------------------------

/// Why the image was not built. No variant carries downloaded or guest bytes.
#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error("downloading {what} failed: {detail}")]
    Download { what: &'static str, detail: String },
    #[error("{what} is larger than {cap} bytes; refused")]
    TooLarge { what: &'static str, cap: u64 },
    #[error("the base image does not match the SHA-512 this release pins; refused")]
    HashMismatch,
    #[error("Debian's SHA512SUMS does not confirm the pinned base image: {0}; refused")]
    Sums(&'static str),
    #[error("{0}")]
    Tool(String),
    #[error("preparing the build bundle failed: {0}")]
    Bundle(String),
    #[error("running the build VM failed: {0}")]
    Launcher(String),
    #[error("the build VM reported failed step={} (launcher exit {})", shown(.step), shown(.code))]
    Failed {
        step: Option<u32>,
        code: Option<i32>,
    },
    #[error("the build VM reported no result line (launcher exit {})", shown(.code))]
    NoResult { code: Option<i32> },
    #[error("the build VM would not run under the VM wall, so nothing was booted: {0}")]
    Wall(String),
    #[error(
        "the build VM saw no wall around it within {WALL_GATE_SECS}s of boot (DNS to its \
         gateway was answered or refused, or the guest had no address on its network yet), \
         so it fetched nothing and powered off (launcher exit {})",
        shown(.code)
    )]
    WallNotUp { code: Option<i32> },
    #[error(
        "the build VM never had an address on its network within {WALL_GATE_SECS}s of boot, \
         so the wall could not be judged; it fetched nothing and powered off (launcher exit {})",
        shown(.code)
    )]
    NoGuestNetwork { code: Option<i32> },
    #[error(
        "the VM wall did not hold around the running build VM, so setup killed it and \
         landed nothing: {0}"
    )]
    WallLost(String),
    #[error("interrupted by a signal: the build VM was stopped and nothing was landed")]
    Interrupted,
    #[error("landing the built image failed: {0}")]
    Land(String),
    #[error("image store io error: {0}")]
    Io(#[from] std::io::Error),
}

/// `value`, or `?` when there is none.
fn shown<T: std::fmt::Display>(value: &Option<T>) -> String {
    value.as_ref().map_or_else(|| "?".to_string(), T::to_string)
}

/// A download arriving in chunks.
pub trait Chunks {
    /// The next chunk, `None` at the end.
    fn next_chunk(
        &mut self,
    ) -> impl std::future::Future<Output = Result<Option<Vec<u8>>, BuildError>>;
}

/// Whether `_faber_vm`'s home is acceptable: its own, at 0700, or at 0710 in
/// group `_faber` (so `_faber` can reach the supervisor's socket). Pure.
pub fn vm_home_ok(uid: u32, gid: u32, mode: u32, vm_uid: u32, faber_gid: u32) -> bool {
    uid == vm_uid
        && match mode & 0o777 {
            0o700 => true,
            0o710 => gid == faber_gid,
            _ => false,
        }
}

/// Every effect of a build. The real one ([`MacHost`]) is macOS only; tests
/// substitute a fake.
pub trait ImageBuildHost {
    type Chunks: Chunks;
    /// `Ok` only when the wall covers `net` ([`wall_binding`], on the stored
    /// rules and the checker's verdict as they are now), saying whether the
    /// anchor binds it yet. Asked before anything is downloaded, again just
    /// before the build VM boots, and then every [`BuildSpec::wall_poll`]
    /// while it runs (#243).
    fn wall_covers(
        &self,
        net: &VmNetwork,
    ) -> impl std::future::Future<Output = Result<Binding, BuildError>>;
    /// Start downloading `url`.
    fn download(
        &self,
        url: &'static str,
    ) -> impl std::future::Future<Output = Result<Self::Chunks, BuildError>>;
    /// Extract exactly the member `disk.raw` of `tarball` into `dest_dir`.
    fn extract_disk(
        &self,
        tarball: &Path,
        dest_dir: &Path,
    ) -> impl std::future::Future<Output = Result<(), BuildError>>;
    /// Build a NoCloud seed ISO (volume `cidata`) of `src_dir` at `out`.
    fn make_seed_iso(
        &self,
        src_dir: &Path,
        out: &Path,
    ) -> impl std::future::Future<Output = Result<(), BuildError>>;
    /// A fresh bundle the launcher accepts, holding copies of `disk` and
    /// `seed`; returns its directory. `staging` is root's own 0700 directory
    /// the build works in: the bundle is finished there, where no other
    /// account can reach it, and only then moved into place.
    fn prepare_bundle(
        &self,
        staging: &Path,
        disk: &Path,
        seed: &Path,
    ) -> impl std::future::Future<Output = Result<PathBuf, BuildError>>;
    /// Boot the bundle and wait for the guest to power off, at most `timeout`
    /// (then kill it), or until `stop` is set (then kill it at once). Returns
    /// the launcher's exit code, `None` when it was killed.
    fn run_launcher(
        &self,
        bundle: &Path,
        timeout: Duration,
        stop: Arc<AtomicBool>,
    ) -> impl std::future::Future<Output = Result<Option<i32>, BuildError>>;
    /// The tail of the bundle's console log, at most `cap` bytes.
    fn read_console(
        &self,
        bundle: &Path,
        cap: u64,
    ) -> impl std::future::Future<Output = Result<Vec<u8>, BuildError>>;
    /// Copy the bundle's disk to `dest` (a new file in root's staging),
    /// never following a symlink in the bundle, and refusing, before copying
    /// anything, a disk longer than `max_bytes` (#230): its length is
    /// `_faber_vm`'s to set, and a copy of a huge sparse file could take hours.
    fn land(
        &self,
        bundle: &Path,
        dest: &Path,
        max_bytes: u64,
    ) -> impl std::future::Future<Output = Result<(), BuildError>>;
    /// Remove the bundle. Best effort.
    fn remove_bundle(&self, bundle: &Path) -> impl std::future::Future<Output = ()>;
    /// Resolves when the operator stops setup (SIGINT, SIGTERM, SIGHUP) while the build VM
    /// runs; tests substitute their own.
    fn interrupt(&self) -> impl std::future::Future<Output = ()> {
        terminate_signal()
    }
}

/// What one build uses: the pin, the sizes and the caps.
#[derive(Debug, Clone)]
pub struct BuildSpec {
    pub pin: BasePin,
    pub provision_digest: String,
    pub disk_bytes: u64,
    pub max_tarball: u64,
    pub max_sums: u64,
    pub max_console: u64,
    pub launch_timeout: Duration,
    /// How long after the launcher starts the wall may still list the build
    /// network as pending before setup kills the VM (#243).
    pub bind_deadline: Duration,
    /// How often setup reads the checker's verdict while the build VM runs.
    pub wall_poll: Duration,
    /// Where root keeps the build VM's console when the build fails, so the
    /// operator can read why (root-only, 0600). It is copied, never parsed or
    /// printed: guest bytes stay out of root's terminal.
    pub failed_console: Option<PathBuf>,
}

impl BuildSpec {
    /// This release's.
    pub fn release() -> Self {
        Self {
            pin: BASE,
            provision_digest: PROVISION_DIGEST.clone(),
            disk_bytes: DISK_BYTES,
            max_tarball: MAX_TARBALL,
            max_sums: MAX_SUMS,
            max_console: MAX_CONSOLE,
            launch_timeout: LAUNCH_TIMEOUT,
            bind_deadline: BIND_DEADLINE,
            wall_poll: WALL_POLL,
            failed_console: None,
        }
    }
}

static STAGING_SEQ: AtomicU64 = AtomicU64::new(0);

/// Create `path` with `mode` if missing, then require a real directory and set
/// its mode exactly.
fn ensure_store_dir(path: &Path, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    match std::fs::DirBuilder::new().mode(mode).create(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(e) => return Err(e),
    }
    if !std::fs::symlink_metadata(path)?.file_type().is_dir() {
        return Err(std::io::Error::other(format!(
            "{} is not a real directory",
            path.display()
        )));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

fn set_mode(path: &Path, mode: u32) -> std::io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

/// Build the Linux image into `store` and return its marker. On any failure
/// the previous image, if any, stays as it was; staging and the build bundle
/// are removed either way.
///
/// The caller has made `store` safe: on a real host, root's own all the way up
/// ([`super::fsx::root_dir_chain`]). This function creates `linux/` (0755) and
/// `.staging/` (0700) in it, and writes as the process it runs as.
pub async fn build<H: ImageBuildHost>(
    host: &H,
    store: &Path,
    spec: &BuildSpec,
    now_unix: u64,
) -> Result<Marker, BuildError> {
    ensure_store_dir(store, 0o755)?;
    let linux = store.join(LINUX_DIR);
    ensure_store_dir(&linux, 0o755)?;
    let staging_root = store.join(".staging");
    ensure_store_dir(&staging_root, 0o700)?;
    let staged = staging_root.join(format!(
        "{}-{}",
        std::process::id(),
        STAGING_SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&staged);
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&staged)?;
    }
    let result = build_in(host, &linux, &staged, spec, now_unix).await;
    let _ = std::fs::remove_dir_all(&staged);
    result
}

async fn build_in<H: ImageBuildHost>(
    host: &H,
    linux: &Path,
    staged: &Path,
    spec: &BuildSpec,
    now_unix: u64,
) -> Result<Marker, BuildError> {
    // 0. The wall covers the build VM's network, or nothing happens (#236).
    host.wall_covers(&build_network()).await?;

    // 1. The base: the pin is the anchor, Debian's sums the cross-check.
    let tarball = staged.join(spec.pin.file_name());
    let got = fetch_to_file(
        host,
        spec.pin.url,
        "the base image",
        &tarball,
        spec.max_tarball,
    )
    .await?;
    if got != spec.pin.sha512 {
        let _ = std::fs::remove_file(&tarball);
        return Err(BuildError::HashMismatch);
    }
    let sums = fetch_to_memory(host, spec.pin.sums_url, "SHA512SUMS", spec.max_sums).await?;
    check_sums(&sums, spec.pin.file_name(), spec.pin.sha512)?;

    // 2. The disk, grown (sparse) so growpart can grow root at build.
    host.extract_disk(&tarball, staged).await?;
    let _ = std::fs::remove_file(&tarball);
    let disk = staged.join(DISK_FILE);
    if !std::fs::symlink_metadata(&disk)?.file_type().is_file() {
        return Err(BuildError::Tool(
            "the tarball held no regular disk.raw".into(),
        ));
    }
    {
        let f = OpenOptions::new().write(true).open(&disk)?;
        if f.metadata()?.len() < spec.disk_bytes {
            f.set_len(spec.disk_bytes)?;
        }
        f.sync_all()?;
    }

    // 3. The seed, from the embedded guest files.
    let seed_src = staged.join("seed");
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(&seed_src)?;
    }
    for (name, text) in [
        ("meta-data", META_DATA.to_string()),
        ("user-data", user_data()),
        ("network-config", NETWORK_CONFIG.to_string()),
    ] {
        std::fs::write(seed_src.join(name), text)?;
    }
    let seed = staged.join(SEED_FILE);
    host.make_seed_iso(&seed_src, &seed).await?;

    // 4-7. Boot it as `_faber_vm`, judge the console, land the disk.
    let bundle = host.prepare_bundle(staged, &disk, &seed).await?;
    // The staged copies are no longer needed; the bundle has its own.
    let _ = std::fs::remove_file(&disk);
    let result = run_and_land(host, &bundle, linux, staged, spec, now_unix).await;
    host.remove_bundle(&bundle).await;
    result
}

async fn run_and_land<H: ImageBuildHost>(
    host: &H,
    bundle: &Path,
    linux: &Path,
    staged: &Path,
    spec: &BuildSpec,
    now_unix: u64,
) -> Result<Marker, BuildError> {
    // Judged again: the download took minutes, and the wall may have changed.
    host.wall_covers(&build_network()).await?;
    let code = run_watched(host, bundle, spec).await?;
    let console = host.read_console(bundle, spec.max_console).await?;
    let docker_version = match judge_console(code, &console) {
        Ok(v) => v,
        Err(e) => {
            if let Some(keep) = &spec.failed_console {
                let _ = keep_console(keep, &console);
            }
            return Err(e);
        }
    };

    let landed = staged.join("disk.raw.landed");
    host.land(bundle, &landed, spec.disk_bytes).await?;
    let meta = std::fs::symlink_metadata(&landed)?;
    if !meta.file_type().is_file() {
        return Err(BuildError::Land(
            "the landed disk is not a regular file".into(),
        ));
    }
    if meta.len() > spec.disk_bytes {
        return Err(BuildError::Land(format!(
            "the landed disk is {} bytes, more than the {} the image is made with",
            meta.len(),
            spec.disk_bytes
        )));
    }
    set_mode(&landed, 0o444)?;
    File::open(&landed)?.sync_all()?;

    let marker = Marker {
        schema: SCHEMA,
        base_build: spec.pin.build.to_string(),
        base_sha512: spec.pin.sha512.to_string(),
        provision_digest: spec.provision_digest.clone(),
        built_at_unix: now_unix,
        docker_version,
    };
    let staged_marker = staged.join(MARKER_FILE);
    {
        let mut f = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged_marker)?;
        let json =
            serde_json::to_vec(&marker).map_err(|e| BuildError::Land(format!("marker: {e}")))?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    set_mode(&staged_marker, 0o444)?;

    swap_in(linux, &landed, &staged_marker, &mut |from, to| {
        std::fs::rename(from, to)
    })?;
    if let Ok(dir) = File::open(linux) {
        let _ = dir.sync_all();
    }
    Ok(marker)
}

/// Copy the console root already read (bounded) to `path`, root's alone
/// (0600), replacing an earlier copy, never through a symlink at `path`.
fn keep_console(path: &Path, console: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let _ = std::fs::remove_file(path);
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(super::fsx::WRITE_NOFOLLOW)
        .open(path)?;
    f.write_all(console)?;
    f.sync_all()
}

/// Put back what an interrupted [`swap_in`] left aside. The disk goes aside
/// before the marker, so an aside disk says an old disk existed, and a marker
/// aside with none says there was none.
///
/// - Marker missing, its `.prev` there, and a disk `.prev` there: the old disk
///   goes back over whatever disk is in place (no marker describes it), then
///   the old marker beside it.
/// - Marker missing, its `.prev` there, no disk `.prev`: the old marker described
///   no disk (it was hand-deleted), so beside any disk now in place it would
///   describe the wrong one. It is discarded.
/// - Otherwise, a disk gone with its `.prev` there (killed between the two
///   asides) comes back.
fn recover_asides(
    disk: &Path,
    marker: &Path,
    disk_prev: &Path,
    marker_prev: &Path,
) -> std::io::Result<()> {
    let there = |p: &Path| std::fs::symlink_metadata(p).is_ok();
    let had_old_disk = there(disk_prev);
    if !there(marker) && there(marker_prev) {
        if had_old_disk {
            std::fs::rename(disk_prev, disk)?;
            std::fs::rename(marker_prev, marker)?;
        } else {
            std::fs::remove_file(marker_prev)?;
        }
    } else if had_old_disk && !there(disk) {
        std::fs::rename(disk_prev, disk)?;
    }
    Ok(())
}

/// Run the build VM while watching the wall around it (#243). The guest's own
/// gate cannot tell an anchor that drops its probe from a host that never
/// answers one, so setup proves the binding from the host: every
/// `spec.wall_poll` it reads the checker's verdict, and kills the launcher if
/// the wall stops covering the build network at any point, if the network is
/// still pending `spec.bind_deadline` after the launcher started, or if it
/// goes back to pending once bound. A launcher that exits before setup ever
/// saw the network bound fails the build too. Returns the launcher's exit
/// code.
async fn run_watched<H: ImageBuildHost>(
    host: &H,
    bundle: &Path,
    spec: &BuildSpec,
) -> Result<Option<i32>, BuildError> {
    let params = WatchParams {
        launch_timeout: spec.launch_timeout,
        bind_deadline: spec.bind_deadline,
        wall_poll: spec.wall_poll,
    };
    watch_launch_until(
        &params,
        |net| async move { host.wall_covers(&net).await },
        |timeout, stop| host.run_launcher(bundle, timeout, stop),
        host.interrupt(),
    )
    .await
}

/// Put the landed disk and its marker in `linux`, through `rename` (a seam,
/// so the tests can fail any step), such that a failure at any step leaves the
/// previous disk and its marker as they were (#230), and a marker never
/// stands beside a disk it does not describe.
///
/// The old disk goes aside first and the old marker after it (so a marker
/// aside with no disk aside means there was no old disk, [`recover_asides`]);
/// then the new disk goes in, and the new marker last. If a step fails, everything already
/// moved is moved back in the reverse order, and a new disk with no old one to
/// restore is removed; the old marker comes back only once its disk has. The
/// asides are `.prev` names in `linux`, root's own directory. A run killed
/// midway leaves its asides behind, possibly the only copy of the old image:
/// so before anything else they are put back where the marker is missing
/// ([`recover_asides`]), and only what is left over is removed.
fn swap_in(
    linux: &Path,
    landed: &Path,
    staged_marker: &Path,
    rename: &mut dyn FnMut(&Path, &Path) -> std::io::Result<()>,
) -> Result<(), BuildError> {
    let disk = linux.join(DISK_FILE);
    let marker = linux.join(MARKER_FILE);
    let disk_prev = linux.join(format!("{DISK_FILE}.prev"));
    let marker_prev = linux.join(format!("{MARKER_FILE}.prev"));
    recover_asides(&disk, &marker, &disk_prev, &marker_prev)?;
    for stale in [&disk_prev, &marker_prev] {
        match std::fs::remove_file(stale) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    let aside = |rename: &mut dyn FnMut(&Path, &Path) -> std::io::Result<()>,
                 from: &Path,
                 to: &Path|
     -> std::io::Result<bool> {
        match rename(from, to) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e),
        }
    };
    let had_disk = aside(rename, &disk, &disk_prev)?;
    // Undo in reverse: the disk first, then its marker, so the marker comes
    // back only beside the disk it describes.
    let restore = |rename: &mut dyn FnMut(&Path, &Path) -> std::io::Result<()>,
                   had_disk: bool,
                   had_marker: bool,
                   new_disk_in: bool| {
        let disk_back = if had_disk {
            rename(&disk_prev, &disk).is_ok()
        } else if new_disk_in {
            std::fs::remove_file(&disk).is_ok()
        } else {
            true
        };
        // Never beside a disk it does not describe: if the old disk could not
        // be put back, the marker stays aside, and the image reads as not
        // built rather than as current.
        if had_marker && disk_back {
            let _ = rename(&marker_prev, &marker);
        }
    };
    let had_marker = match aside(rename, &marker, &marker_prev) {
        Ok(had) => had,
        Err(e) => {
            restore(rename, had_disk, false, false);
            return Err(e.into());
        }
    };
    if let Err(e) = rename(landed, &disk) {
        restore(rename, had_disk, had_marker, false);
        return Err(e.into());
    }
    if let Err(e) = rename(staged_marker, &marker) {
        restore(rename, had_disk, had_marker, true);
        return Err(e.into());
    }
    for done in [&disk_prev, &marker_prev] {
        let _ = std::fs::remove_file(done);
    }
    Ok(())
}

/// Stream `url` into the new file `dest`, at most `cap` bytes, and return the
/// SHA-512 of what was written, in lowercase hex.
async fn fetch_to_file<H: ImageBuildHost>(
    host: &H,
    url: &'static str,
    what: &'static str,
    dest: &Path,
    cap: u64,
) -> Result<String, BuildError> {
    let mut chunks = host.download(url).await?;
    let mut file = OpenOptions::new().write(true).create_new(true).open(dest)?;
    let mut hasher = Sha512::new();
    let mut total: u64 = 0;
    while let Some(chunk) = chunks.next_chunk().await? {
        total = total.saturating_add(chunk.len() as u64);
        if total > cap {
            drop(file);
            let _ = std::fs::remove_file(dest);
            return Err(BuildError::TooLarge { what, cap });
        }
        hasher.update(&chunk);
        file.write_all(&chunk)?;
    }
    file.sync_all()?;
    Ok(format!("{:x}", hasher.finalize()))
}

async fn fetch_to_memory<H: ImageBuildHost>(
    host: &H,
    url: &'static str,
    what: &'static str,
    cap: u64,
) -> Result<Vec<u8>, BuildError> {
    let mut chunks = host.download(url).await?;
    let mut out = Vec::new();
    while let Some(chunk) = chunks.next_chunk().await? {
        if (out.len() + chunk.len()) as u64 > cap {
            return Err(BuildError::TooLarge { what, cap });
        }
        out.extend_from_slice(&chunk);
    }
    Ok(out)
}

/// `sums` (a `SHA512SUMS` file) lists `file` exactly once, with `sha512`.
/// `<hex>  <name>` and `<hex> *<name>` are both read; the hex in either case.
pub fn check_sums(sums: &[u8], file: &str, sha512: &str) -> Result<(), BuildError> {
    let text = std::str::from_utf8(sums).map_err(|_| BuildError::Sums("it is not text"))?;
    let mut found = None;
    for line in text.lines() {
        let Some((hex, name)) = line.split_once(' ') else {
            continue;
        };
        let name = name.strip_prefix(' ').or_else(|| name.strip_prefix('*'));
        if name != Some(file) {
            continue;
        }
        if found.is_some() {
            return Err(BuildError::Sums("it lists the file twice"));
        }
        found = Some(hex.to_ascii_lowercase());
    }
    match found {
        None => Err(BuildError::Sums("it does not list the file")),
        Some(hex) if hex == sha512 => Ok(()),
        Some(_) => Err(BuildError::Sums(
            "its digest for the file disagrees with the pin",
        )),
    }
}

// ---------------------------------------------------------------------------
// The daemon's view
// ---------------------------------------------------------------------------

/// How often the daemon re-judges the installed image.
pub const WATCH_EVERY: Duration = Duration::from_secs(5 * 60);

/// Judge the installed Linux image at once and every [`WATCH_EVERY`], and
/// publish its standing for `/api/state`. The golden image's standing is left
/// to its own owner.
#[cfg(target_os = "macos")]
pub async fn watch(limiter: std::sync::Arc<crate::limiter::Limiter>) {
    let mut last: Option<Verdict> = None;
    loop {
        let installed = tokio::task::spawn_blocking(|| {
            read_installed(Path::new(crate::platform::layout::IMAGE_STORE))
        })
        .await
        .unwrap_or_else(|_| Installed::Unusable("the check did not finish".into()));
        let verdict = judge(&installed);
        // Logged when it changes, not every round.
        if last.as_ref() != Some(&verdict) {
            if let Verdict::OutOfDate(why) = &verdict {
                tracing::warn!("Linux VM image out of date: {why}; run `sudo faber setup`");
            }
        }
        limiter.observe_linux_image(verdict.standing());
        last = Some(verdict);
        tokio::time::sleep(WATCH_EVERY).await;
    }
}

// ---------------------------------------------------------------------------
// The real host (macOS)
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub use mac::{https_client, https_get, HttpChunks, MacHost};

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::OsString;
    use std::fs::File;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Duration;

    use super::super::fsx::{self, Account, ROOT};
    use super::super::walled_vm::mac::{
        clone_or_copy, give, place_dir, root_own_dir, run_tool, wall_covers, VmHome,
    };
    use super::*;

    /// The real [`ImageBuildHost`]: runs as root in `sudo faber setup`.
    pub struct MacHost {
        homes: VmHome,
        launcher: PathBuf,
        http: reqwest::Client,
    }

    impl MacHost {
        pub fn new(
            vm: Account,
            faber: Account,
            vm_home: &Path,
            launcher: &Path,
        ) -> Result<Self, BuildError> {
            let http = https_client(Duration::from_secs(30 * 60))?;
            Ok(Self {
                homes: VmHome {
                    vm,
                    faber,
                    path: vm_home.to_path_buf(),
                },
                launcher: launcher.to_path_buf(),
                http,
            })
        }
    }

    /// The client every download of setup's goes through: HTTPS only, with
    /// faber's user agent, a 15 s connect timeout and `timeout` overall.
    pub fn https_client(timeout: Duration) -> Result<reqwest::Client, BuildError> {
        reqwest::Client::builder()
            .user_agent(concat!("faber/", env!("FABER_VERSION")))
            .https_only(true)
            .connect_timeout(Duration::from_secs(15))
            .timeout(timeout)
            .build()
            .map_err(|e| BuildError::Download {
                what: "anything",
                detail: crate::runner::transport_detail(&e),
            })
    }

    /// Start downloading `url` with `http`, refusing any status but a
    /// success. `what` names the body in a later read error.
    pub async fn https_get(
        http: &reqwest::Client,
        url: &'static str,
        what: &'static str,
    ) -> Result<HttpChunks, BuildError> {
        let fail = |detail| BuildError::Download { what: url, detail };
        let resp = http
            .get(url)
            .send()
            .await
            .map_err(|e| fail(crate::runner::transport_detail(&e)))?;
        if !resp.status().is_success() {
            return Err(fail(format!("status {}", resp.status().as_u16())));
        }
        Ok(HttpChunks(resp, what))
    }

    /// A download's body, chunk by chunk, and what it is.
    pub struct HttpChunks(reqwest::Response, &'static str);

    impl Chunks for HttpChunks {
        async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, BuildError> {
            self.0
                .chunk()
                .await
                .map(|c| c.map(|b| b.to_vec()))
                .map_err(|e| BuildError::Download {
                    what: self.1,
                    detail: crate::runner::transport_detail(&e),
                })
        }
    }

    impl ImageBuildHost for MacHost {
        type Chunks = HttpChunks;

        async fn wall_covers(&self, net: &VmNetwork) -> Result<Binding, BuildError> {
            wall_covers(net).await
        }

        async fn download(&self, url: &'static str) -> Result<HttpChunks, BuildError> {
            https_get(&self.http, url, "the base image").await
        }

        async fn extract_disk(&self, tarball: &Path, dest_dir: &Path) -> Result<(), BuildError> {
            let mut cmd = Command::new("/usr/bin/tar");
            cmd.args(["-x", "-S", "-f"])
                .arg(tarball)
                .arg("-C")
                .arg(dest_dir)
                .arg(DISK_FILE);
            tokio::task::spawn_blocking(move || run_tool(cmd, "tar"))
                .await
                .map_err(|e| BuildError::Tool(format!("tar: {e}")))?
        }

        async fn make_seed_iso(&self, src_dir: &Path, out: &Path) -> Result<(), BuildError> {
            let mut cmd = Command::new("/usr/bin/hdiutil");
            cmd.args([
                "makehybrid",
                "-quiet",
                "-iso",
                "-joliet",
                "-default-volume-name",
                "cidata",
                "-o",
            ])
            .arg(out)
            .arg(src_dir);
            tokio::task::spawn_blocking(move || run_tool(cmd, "hdiutil makehybrid"))
                .await
                .map_err(|e| BuildError::Tool(format!("hdiutil: {e}")))?
        }

        /// Everything root makes in `_faber_vm`'s home (the state directory,
        /// the bundle, its disk and seed) is made in root's own `staging`,
        /// given away through its own descriptor there, and moved in with
        /// `renameatx_np(RENAME_EXCL)`. Root never creates a name in the home
        /// and then reopens it: `_faber_vm` could swap that name, for a hard
        /// link to a system file say, between the two, and root would hand
        /// over whatever inode it found.
        async fn prepare_bundle(
            &self,
            staging: &Path,
            disk: &Path,
            seed: &Path,
        ) -> Result<PathBuf, BuildError> {
            let home = self.homes.home()?;
            let staging = root_own_dir(staging)?;
            // The launcher's slot locks live here, `_faber_vm`'s own at 0700.
            self.homes.ensure_state_dir(&home, &staging)?;
            let path = self.homes.path.join(BUNDLE_NAME);
            // A leftover from an interrupted build. `remove_dir_all` removes a
            // symlink in its place rather than following it.
            match std::fs::remove_dir_all(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(BuildError::Bundle(format!("{}: {e}", path.display()))),
            }
            // Fails, rather than replacing it, if `_faber_vm` put something
            // at the name since.
            place_dir(
                &staging,
                &home,
                BUNDLE_NAME,
                self.homes.vm,
                0o700,
                |bundle| {
                    for (src, name) in [(disk, DISK_FILE), (seed, SEED_FILE)] {
                        let src = File::open(src)?;
                        // Root's own staged files: no cap.
                        let dst = clone_or_copy(&src, bundle, name, u64::MAX)?;
                        give(&dst, bundle, name, self.homes.vm, 0o600)?;
                    }
                    Ok(())
                },
            )?;
            Ok(path)
        }

        async fn run_launcher(
            &self,
            bundle: &Path,
            timeout: Duration,
            stop: Arc<AtomicBool>,
        ) -> Result<Option<i32>, BuildError> {
            let state = self.homes.path.join(STATE_DIR_NAME);
            let console = bundle.join(CONSOLE_FILE);
            let args: Vec<OsString> = vec![
                "run".into(),
                bundle.into(),
                "--slot".into(),
                BUILD_SLOT.to_string().into(),
                "--state-dir".into(),
                state.into(),
                "--subnet".into(),
                BUILD_SUBNET.into(),
                // The address the wall's build network reserves, stated
                // rather than left to the launcher's default.
                "--reserve-ip".into(),
                BUILD_GUEST.to_string().into(),
                "--console-log".into(),
                console.into(),
                "--console-log-max-mib".into(),
                "16".into(),
                "--cpus".into(),
                "2".into(),
                "--memory-gib".into(),
                "4".into(),
            ];
            let run = self
                .homes
                .run_launcher(&self.launcher, args, timeout, stop)
                .await?;
            if !run.said.is_empty() {
                println!("  faber-vm said:\n{}", run.said);
            }
            Ok(run.code)
        }

        async fn read_console(&self, bundle: &Path, cap: u64) -> Result<Vec<u8>, BuildError> {
            self.homes.read_tail(bundle, BUNDLE_NAME, CONSOLE_FILE, cap)
        }

        async fn land(&self, bundle: &Path, dest: &Path, max_bytes: u64) -> Result<(), BuildError> {
            let src = self.homes.bundle_file(bundle, BUNDLE_NAME, DISK_FILE)?;
            // Judged on the descriptor that is then cloned or copied.
            let len = src.metadata()?.len();
            if len > max_bytes {
                return Err(BuildError::Land(format!(
                    "{} is {len} bytes, more than the {max_bytes} the image is made with; \
                     refused before copying anything",
                    bundle.join(DISK_FILE).display()
                )));
            }
            let (parent, name) = fsx::split(dest)?;
            let dir = root_own_dir(parent)?;
            let dst = clone_or_copy(&src, &dir, name, max_bytes)?;
            // The clone carried `_faber_vm`'s ACL and attributes, if any: an
            // ACL entry would survive the image's 0444.
            give(&dst, &dir, name, ROOT, 0o600)?;
            Ok(())
        }

        async fn remove_bundle(&self, bundle: &Path) {
            if bundle == self.homes.path.join(BUNDLE_NAME) {
                let _ = std::fs::remove_dir_all(bundle);
            }
        }
    }
}

#[cfg(test)]
mod tests {

    fn fact(path: &str, uid: u32, mode: u32) -> PathFacts {
        PathFacts {
            path: path.into(),
            uid,
            gid: 0,
            mode,
        }
    }

    #[test]
    fn launcher_chain_trusts_root_and_faber_only() {
        const FABER: u32 = 301;
        let file = 0o100_755;
        let dir = 0o040_755;
        let ok = |uid| {
            launcher_chain_verdict(
                &[fact("/a/bin/faber-vm", uid, file), fact("/a/bin", 0, dir)],
                FABER,
            )
        };
        assert!(ok(0).is_ok());
        assert!(ok(FABER).is_ok());
        assert!(ok(302).unwrap_err().contains("neither root nor _faber"));
        let vm_dir = launcher_chain_verdict(
            &[fact("/a/bin/faber-vm", 0, file), fact("/a/bin", 302, dir)],
            FABER,
        );
        assert!(vm_dir.is_err());
        let group_w = launcher_chain_verdict(
            &[
                fact("/a/bin/faber-vm", 0, file),
                fact("/a/bin", 0, 0o040_775),
            ],
            FABER,
        );
        assert!(group_w.unwrap_err().contains("group- or world-writable"));
        let sticky = launcher_chain_verdict(
            &[fact("/a/bin/faber-vm", 0, file), fact("/tmp", 0, 0o041_777)],
            FABER,
        );
        assert!(sticky.is_ok());
    }

    /// `_faber_vm` is "other" to every entry on the launcher's path: a directory
    /// it cannot search or a launcher it cannot execute fails the build late.
    #[test]
    fn launcher_chain_must_be_reachable_by_other_accounts() {
        let closed_dir = launcher_chain_verdict(
            &[
                fact("/a/bin/faber-vm", 0, 0o100_755),
                fact("/a/bin", 0, 0o040_755),
                fact("/a", 0, 0o040_750),
            ],
            301,
        );
        assert!(closed_dir.unwrap_err().contains("not searchable"));
        let closed_file = launcher_chain_verdict(&[fact("/a/bin/faber-vm", 0, 0o100_750)], 301);
        assert!(closed_file.unwrap_err().contains("readable and executable"));
    }

    #[test]
    fn launcher_without_a_faber_account_is_not_installed() {
        let err = launcher_trusted(Path::new("/nonexistent/faber-vm"), None).unwrap_err();
        assert!(err.contains("not installed"));
    }

    #[test]
    fn launcher_path_is_outside_the_state_dir() {
        let layout = SystemLayout::for_os(Os::MacOs);
        assert_eq!(
            launcher_path(&layout),
            PathBuf::from("/var/db/faber/bin/faber-vm")
        );
    }
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

    /// Where the provisioning script finds the files it installs itself.
    const SHARE: &str = "/usr/local/share/faber";

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "faber-linux-image-test-{}-{}",
                std::process::id(),
                TMP_SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sha512_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha512::digest(bytes))
    }

    // --- the pin ---

    #[test]
    fn the_pin_is_well_formed() {
        assert!(is_lower_hex(BASE_SHA512, 128));
        assert!(valid_build(BASE_BUILD));
        let dir = format!("https://cloud.debian.org/images/cloud/trixie/{BASE_BUILD}/");
        assert!(BASE_URL.starts_with(&dir), "{BASE_URL}");
        assert_eq!(
            BASE.file_name(),
            format!("debian-13-genericcloud-arm64-{BASE_BUILD}.tar.xz")
        );
        assert_eq!(BASE_URL, format!("{dir}{}", BASE.file_name()));
        assert_eq!(BASE_SUMS_URL, format!("{dir}SHA512SUMS"));
    }

    #[test]
    fn build_ids_are_date_dash_number() {
        assert!(valid_build("20261015-2632"));
        for bad in [
            "2026101-2632",
            "20261015",
            "20261015-",
            "2026-1015-1",
            "20261015-26a",
            "",
        ] {
            assert!(!valid_build(bad), "{bad}");
        }
    }

    // --- the guest files ---

    #[test]
    fn the_docker_key_is_one_public_key_block() {
        let lines: Vec<&str> = DOCKER_KEY.trim_end().lines().collect();
        assert_eq!(lines.first(), Some(&"-----BEGIN PGP PUBLIC KEY BLOCK-----"));
        assert_eq!(lines.last(), Some(&"-----END PGP PUBLIC KEY BLOCK-----"));
        assert_eq!(DOCKER_KEY.matches("-----BEGIN PGP").count(), 1);
        assert_eq!(DOCKER_KEY.matches("-----END PGP").count(), 1);
        assert!(!DOCKER_KEY.contains("PRIVATE"));
        assert_eq!(DOCKER_KEY_FINGERPRINT.len(), 40);
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (plain, enc) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), enc);
        }
    }

    #[test]
    fn user_data_has_no_way_in_and_writes_every_file() {
        let ud = user_data();
        assert!(ud.starts_with("#cloud-config\n"));
        assert!(ud.contains("\nusers: []\n"));
        assert!(ud.contains("\ndisable_root: true\n"));
        assert!(ud.contains("\nssh_pwauth: false\n"));
        for forbidden in [
            "ssh_authorized_keys",
            "ssh_keys",
            "passwd",
            "chpasswd",
            "sudo:",
            "lock_passwd",
        ] {
            assert!(!ud.contains(forbidden), "{forbidden}");
        }
        assert_eq!(ud.matches("write_files:").count(), 1);
        for f in GUEST_FILES {
            assert!(
                ud.contains(&format!("  - path: {}\n", f.guest_path)),
                "{}",
                f.name
            );
            assert!(ud.contains(&base64(f.bytes.as_bytes())), "{}", f.name);
        }
        assert!(ud.contains("/usr/local/libexec/faber-provision"));
        // The provisioning script installs what it finds under its share.
        let provision = GUEST_FILES[6].bytes;
        for f in GUEST_FILES
            .iter()
            .filter(|f| f.guest_path.starts_with(SHARE))
        {
            let name = f.guest_path.rsplit('/').next().unwrap();
            assert!(provision.contains(&format!("$share/{name}")), "{name}");
        }
    }

    #[test]
    fn the_seed_names_the_build_instance_and_the_resolvers() {
        assert!(META_DATA.contains("instance-id: faber-linux-image-build"));
        assert!(META_DATA.contains("local-hostname: faber-linux"));
        assert!(NETWORK_CONFIG.contains("version: 2"));
        assert!(NETWORK_CONFIG.contains("use-dns: false"));
        assert!(NETWORK_CONFIG.contains("dhcp-identifier: mac"));
        assert!(NETWORK_CONFIG.contains("[1.1.1.1, 1.0.0.1]"));
    }

    #[test]
    fn the_relay_is_socket_activated_and_not_socket_proxyd() {
        let socket = GUEST_FILES[1].bytes;
        let service = GUEST_FILES[2].bytes;
        assert!(socket.contains("ListenStream=vsock::2375"));
        assert!(socket.contains("Accept=no"));
        assert!(service.contains("ExecStart=/usr/local/libexec/faber-docker-relay"));
        for hardening in [
            "PrivateNetwork=yes",
            "PrivateTmp=yes",
            "NoNewPrivileges=yes",
        ] {
            assert!(service.contains(hardening), "{hardening}");
        }
        assert!(!service.contains("systemd-socket-proxyd"));
        let relay = GUEST_FILES[0].bytes;
        assert!(relay.starts_with("#!/usr/bin/python3\n"));
        assert!(relay.contains("SHUT_WR") && relay.contains("LISTEN_PID"));
    }

    #[test]
    fn the_wall_probe_is_socket_activated_per_connection_on_its_own_port() {
        let probe = GUEST_FILES[7].bytes;
        let socket = GUEST_FILES[8].bytes;
        let service = GUEST_FILES[9].bytes;
        assert_eq!(
            GUEST_FILES[7].guest_path,
            "/usr/local/libexec/faber-wall-probe"
        );
        assert!(socket.contains("ListenStream=vsock::6101"));
        assert!(socket.contains("Accept=yes"));
        assert!(service.contains("ExecStart=/usr/local/libexec/faber-wall-probe"));
        for hardening in [
            "DynamicUser=yes",
            "PrivateTmp=yes",
            "NoNewPrivileges=yes",
            "ProtectSystem=strict",
            "RestrictAddressFamilies=AF_INET AF_VSOCK",
        ] {
            assert!(service.contains(hardening), "{hardening}");
        }
        // It must reach the host's address on the VM network, so it keeps the network.
        assert!(!service.contains("PrivateNetwork=yes"));
        assert!(probe.starts_with("#!/usr/bin/python3\n"));
        assert!(probe.contains("LISTEN_PID") && probe.contains("MAX_LINE = 64"));
        assert!(probe.contains("MAX_COUNT = 16") && probe.contains("PAYLOAD = bytes(8)"));
        assert!(probe.contains("b\"sent %d\\n\""));
        let provision = GUEST_FILES[6].bytes;
        assert!(provision.contains("faber-wall-probe.socket"));
    }

    #[test]
    fn docker_uses_only_cloudflare_resolvers_in_containers_too() {
        let daemon = GUEST_FILES[10];
        assert_eq!(daemon.bytes.trim_end(), r#"{"dns":["1.1.1.1","1.0.0.1"]}"#);
        let provision = GUEST_FILES[6].bytes;
        let installed = provision
            .find("/etc/docker/daemon.json")
            .expect("provision installs daemon.json");
        let engine = provision
            .find("docker-ce ")
            .expect("provision installs docker-ce");
        assert!(
            installed < engine,
            "daemon.json is in place before Docker first starts"
        );
    }

    /// Hosts and addresses the guest files may name: Docker's apt repository
    /// and Cloudflare's resolvers. Nothing else, so no private name or LAN
    /// address ever ships in the image.
    #[test]
    fn the_guest_files_name_only_the_allowed_hosts_and_addresses() {
        let mut texts: Vec<(&str, &str)> = vec![
            ("meta-data", META_DATA),
            ("network-config", NETWORK_CONFIG),
            ("user-data", USER_DATA_TEMPLATE),
        ];
        texts.extend(
            GUEST_FILES
                .iter()
                .filter(|f| f.name != "docker-archive-keyring.asc")
                .map(|f| (f.name, f.bytes)),
        );
        for (name, text) in texts {
            for token in text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '.' || c == '-'))
            {
                let token = token.trim_matches('.');
                if token.parse::<std::net::Ipv4Addr>().is_ok() {
                    // The build network's gateway, which the wall gate probes.
                    let gateway = BUILD_GATEWAY.to_string();
                    assert!(
                        ["1.1.1.1", "1.0.0.1", gateway.as_str()].contains(&token),
                        "{name}: {token}"
                    );
                }
                let lower = token.to_ascii_lowercase();
                if [".com", ".org", ".net", ".io", ".dev", ".local", ".lan"]
                    .iter()
                    .any(|tld| lower.ends_with(tld))
                {
                    // `containerd.io` is a package name, not a host.
                    assert!(
                        ["download.docker.com", "containerd.io"].contains(&lower.as_str()),
                        "{name}: {token}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_provision_digest_covers_every_file_and_its_boundaries() {
        assert!(is_lower_hex(&PROVISION_DIGEST, 64));
        let a = digest_of(&[("x".into(), b"ab"), ("y".into(), b"c")]);
        let b = digest_of(&[("x".into(), b"a"), ("y".into(), b"bc")]);
        let c = digest_of(&[("x".into(), b"ab"), ("y".into(), b"d")]);
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(a, digest_of(&[("x".into(), b"ab"), ("y".into(), b"c")]));
    }

    // --- judging ---

    fn marker() -> Marker {
        Marker {
            schema: SCHEMA,
            base_build: BASE_BUILD.into(),
            base_sha512: BASE_SHA512.into(),
            provision_digest: PROVISION_DIGEST.clone(),
            built_at_unix: 1_790_000_000,
            docker_version: "29.0.1".into(),
        }
    }

    #[test]
    fn the_verdicts() {
        assert_eq!(judge(&Installed::Absent), Verdict::NotBuilt);
        assert_eq!(judge(&Installed::Absent).standing(), ImageStanding::Unknown);
        assert_eq!(judge(&Installed::Found(marker())), Verdict::Current);
        assert_eq!(
            judge(&Installed::Found(marker())).standing(),
            ImageStanding::Current
        );

        let mut old = marker();
        old.base_build = "20260901-2500".into();
        old.base_sha512 = "a".repeat(128);
        let v = judge(&Installed::Found(old));
        assert_eq!(v.standing(), ImageStanding::OutOfDate);
        assert!(
            matches!(&v, Verdict::OutOfDate(why) if why.contains("20260901-2500")),
            "{v:?}"
        );

        let mut other = marker();
        other.provision_digest = "b".repeat(64);
        assert_eq!(
            judge(&Installed::Found(other)).standing(),
            ImageStanding::OutOfDate
        );

        let mut bad = marker();
        bad.docker_version = "1.0; rm".into();
        assert_eq!(
            judge(&Installed::Found(bad)).standing(),
            ImageStanding::OutOfDate
        );

        let unusable = Installed::Unusable("/x is owned by uid 452, not root".into());
        assert_eq!(judge(&unusable).standing(), ImageStanding::OutOfDate);
    }

    #[test]
    fn reading_an_absent_or_untrusted_store() {
        let tmp = TempDir::new();
        assert_eq!(read_installed(&tmp.0), Installed::Absent);
        let linux = tmp.0.join(LINUX_DIR);
        std::fs::create_dir_all(&linux).unwrap();
        std::fs::write(linux.join(DISK_FILE), b"disk").unwrap();
        std::fs::write(
            linux.join(MARKER_FILE),
            serde_json::to_vec(&marker()).unwrap(),
        )
        .unwrap();
        for f in [DISK_FILE, MARKER_FILE] {
            set_mode(&linux.join(f), 0o444).unwrap();
        }
        // A test's temp dir is not root's all the way up (unless the tests run
        // as root, where it is the same check passing).
        match read_installed(&tmp.0) {
            Installed::Unusable(why) => assert!(
                why.contains("not root") || why.contains("writable"),
                "{why}"
            ),
            Installed::Found(m) => {
                assert_eq!(crate::platform::record::current_euid(), 0);
                assert_eq!(m, marker());
            }
            Installed::Absent => panic!("the marker is there"),
        }
    }

    #[test]
    fn a_missing_tail_is_appended_to_the_resolved_prefix() {
        let tmp = TempDir::new();
        let real = std::fs::canonicalize(&tmp.0).unwrap();
        assert_eq!(
            resolve_existing_prefix(&tmp.0.join("a/b")).unwrap(),
            real.join("a").join("b")
        );
        assert_eq!(resolve_existing_prefix(&tmp.0).unwrap(), real);
    }

    #[test]
    fn plans() {
        let ok = LinuxImageProbe {
            launcher: Ok(()),
            installed: Installed::Absent,
        };
        assert_eq!(plan_for(Os::Linux, &ok), None);
        assert_eq!(
            plan_for(Os::MacOs, &ok),
            Some(LinuxImagePlan::Build {
                reason: "not built".into(),
                fresh: true
            })
        );
        let blocked = LinuxImageProbe {
            launcher: Err("no launcher".into()),
            installed: Installed::Absent,
        };
        assert!(matches!(
            plan_for(Os::MacOs, &blocked),
            Some(LinuxImagePlan::Blocked { why }) if why.contains("no launcher")
        ));
        let current = LinuxImageProbe {
            launcher: Err("no launcher".into()),
            installed: Installed::Found(marker()),
        };
        assert_eq!(plan_for(Os::MacOs, &current), Some(LinuxImagePlan::Current));
        let mut old = marker();
        old.base_sha512 = "c".repeat(128);
        let stale = LinuxImageProbe {
            launcher: Ok(()),
            installed: Installed::Found(old),
        };
        assert!(matches!(
            plan_for(Os::MacOs, &stale),
            Some(LinuxImagePlan::Build { fresh: false, .. })
        ));
    }

    // --- the console ---

    /// What a real build VM's console held on the builder (2026-10-21):
    /// getty's prompt shares hvc0 and ends with no newline, so a result
    /// written without one first was glued to it and read as no result. The
    /// script now writes a newline first; the judge stays strict.
    #[test]
    fn a_result_after_getty_s_prompt_needs_its_own_line() {
        let glued = b"Debian GNU/Linux 13 faber-linux hvc0\n\
                      faber-linux login: FABER-LINUX-IMAGE ok docker=29.9.2\n";
        assert!(matches!(
            judge_console(Some(0), glued),
            Err(BuildError::NoResult { .. })
        ));
        let fixed = b"Debian GNU/Linux 13 faber-linux hvc0\n\
                      faber-linux login: \nFABER-LINUX-IMAGE ok docker=29.9.2\n";
        assert_eq!(judge_console(Some(0), fixed).unwrap(), "29.9.2");
        // Every console write in the guest script starts a fresh line.
        let script = include_str!("../../launcher/linux/provision.sh");
        assert!(script.contains(r#"printf '\n%s\n' "$1" >/dev/hvc0"#));
        assert!(!script.contains(r#"printf '%s\n' "$1" >/dev/hvc0"#));
    }

    #[test]
    fn the_console_is_judged_by_exact_lines() {
        let ok =
            b"[  1.0] booting\r\nFABER-LINUX-IMAGE ok docker=29.0.1\r\n[ 9.0] reboot: Power down\n";
        assert_eq!(judge_console(Some(0), ok).unwrap(), "29.0.1");
        // A launcher failure overrides an ok line.
        assert!(matches!(
            judge_console(Some(1), ok),
            Err(BuildError::NoResult { code: Some(1) })
        ));
        assert!(matches!(
            judge_console(None, ok),
            Err(BuildError::NoResult { code: None })
        ));
        let failed = b"FABER-LINUX-IMAGE failed step=4\n";
        assert!(matches!(
            judge_console(Some(0), failed),
            Err(BuildError::Failed {
                step: Some(4),
                code: Some(0)
            })
        ));
        let msg = judge_console(Some(0), failed).unwrap_err().to_string();
        assert!(msg.contains("failed step=4"), "{msg}");
        // Both: a failure wins.
        let both = b"FABER-LINUX-IMAGE ok docker=29.0.1\nFABER-LINUX-IMAGE failed step=9\n";
        assert!(matches!(
            judge_console(Some(0), both),
            Err(BuildError::Failed { .. })
        ));
        // Not at the start of a line, or a bad token: no result.
        for bad in [
            &b"x FABER-LINUX-IMAGE ok docker=29.0.1\n"[..],
            b"FABER-LINUX-IMAGE ok docker=$docker_version\n",
            b"FABER-LINUX-IMAGE ok docker=\n",
            b"FABER-LINUX-IMAGE ok docker=29.0.1 extra\n",
        ] {
            assert!(matches!(
                judge_console(Some(0), bad),
                Err(BuildError::NoResult { .. })
            ));
        }
        let long = format!("FABER-LINUX-IMAGE ok docker={}\n", "9".repeat(65));
        assert!(judge_console(Some(0), long.as_bytes()).is_err());
        // A malformed step is reported as unknown, not echoed.
        let odd = b"FABER-LINUX-IMAGE failed step=\x1b[2J\n";
        let e = judge_console(Some(0), odd).unwrap_err().to_string();
        assert!(e.contains("step=?") && !e.contains('\x1b'), "{e}");
    }

    // --- the sums ---

    #[test]
    fn the_sums_must_list_the_file_once_with_the_pinned_digest() {
        let h = "a".repeat(128);
        let other = "b".repeat(128);
        let f = "debian.tar.xz";
        let good = format!("{other}  debian.json\n{h}  {f}\n");
        assert!(check_sums(good.as_bytes(), f, &h).is_ok());
        let upper_star = format!("{}  x\n{} *{f}\n", other, h.to_uppercase());
        assert!(check_sums(upper_star.as_bytes(), f, &h).is_ok());
        assert!(matches!(
            check_sums(format!("{other}  {f}\n").as_bytes(), f, &h),
            Err(BuildError::Sums(_))
        ));
        assert!(check_sums(format!("{h}  {f}.sig\n").as_bytes(), f, &h).is_err());
        assert!(check_sums(format!("{h}  {f}\n{h}  {f}\n").as_bytes(), f, &h).is_err());
        assert!(check_sums(&[0xff, 0xfe], f, &h).is_err());
        let e = check_sums(format!("{other}  {f}\n").as_bytes(), f, &h)
            .unwrap_err()
            .to_string();
        assert!(!e.contains(&other), "{e}");
    }

    // --- the build, against a fake host ---

    const TARBALL: &[u8] = b"pretend this is debian's tarball";
    const SUMS_URL: &str = "https://example.invalid/SHA512SUMS";
    const TAR_URL: &str = "https://example.invalid/base.tar.xz";

    struct Fake {
        root: PathBuf,
        served: HashMap<&'static str, Vec<u8>>,
        console: Vec<u8>,
        exit: Option<i32>,
        /// How many `wall_covers` calls pass before the wall is refused.
        wall_ok: usize,
        /// While the launcher runs: how many polls answer `Pending` before the
        /// wall binds the build network (#243). Before boot it is `Pending`.
        pending_polls: usize,
        /// While the launcher runs: after how many `Bound` polls it goes back
        /// to `Pending`.
        repend_after: Option<usize>,
        /// How long the guest takes to provision and power off.
        run_for: Duration,
        /// The operator stops setup this long after the launcher starts.
        interrupt_after: Option<Duration>,
        calls: Mutex<Vec<&'static str>>,
    }

    struct FakeChunks(Vec<Vec<u8>>);

    impl Chunks for FakeChunks {
        async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, BuildError> {
            Ok((!self.0.is_empty()).then(|| self.0.remove(0)))
        }
    }

    impl Fake {
        fn new(root: &Path) -> Self {
            let mut served = HashMap::new();
            served.insert(TAR_URL, TARBALL.to_vec());
            served.insert(
                SUMS_URL,
                format!("{}  base.tar.xz\n", sha512_hex(TARBALL)).into_bytes(),
            );
            Self {
                root: root.to_path_buf(),
                served,
                console: b"FABER-LINUX-IMAGE ok docker=29.0.1\n".to_vec(),
                exit: Some(0),
                wall_ok: usize::MAX,
                pending_polls: 1,
                repend_after: None,
                run_for: Duration::from_secs(5),
                interrupt_after: None,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn called(&self, what: &'static str) {
            self.calls.lock().unwrap().push(what);
        }

        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }

        /// The calls, with each run of `wall` polls shown once.
        fn squashed(&self) -> Vec<&'static str> {
            let mut out: Vec<&'static str> = Vec::new();
            for c in self.calls() {
                if !(c == "wall" && out.last() == Some(&"wall")) {
                    out.push(c);
                }
            }
            out
        }
    }

    impl ImageBuildHost for Fake {
        type Chunks = FakeChunks;

        async fn wall_covers(&self, net: &VmNetwork) -> Result<Binding, BuildError> {
            self.called("wall");
            assert_eq!(net, &build_network());
            let calls = self.calls();
            let asked = calls.iter().filter(|c| **c == "wall").count();
            if asked > self.wall_ok {
                return Err(BuildError::Wall(
                    "the stored wall has no network 16 for the build VM".into(),
                ));
            }
            // Polls since the launcher started, this one included.
            let Some(launched) = calls.iter().position(|c| *c == "launch") else {
                return Ok(Binding::Pending);
            };
            let polls = calls[launched..].iter().filter(|c| **c == "wall").count();
            let bound = polls > self.pending_polls
                && self
                    .repend_after
                    .is_none_or(|n| polls <= self.pending_polls + n);
            Ok(if bound {
                Binding::Bound
            } else {
                Binding::Pending
            })
        }

        async fn download(&self, url: &'static str) -> Result<FakeChunks, BuildError> {
            self.called("download");
            let bytes = self.served.get(url).cloned().unwrap_or_default();
            Ok(FakeChunks(bytes.chunks(5).map(<[u8]>::to_vec).collect()))
        }

        async fn extract_disk(&self, tarball: &Path, dest_dir: &Path) -> Result<(), BuildError> {
            self.called("extract");
            let mut bytes = std::fs::read(tarball)?;
            bytes.extend_from_slice(b" -> disk");
            std::fs::write(dest_dir.join(DISK_FILE), bytes)?;
            Ok(())
        }

        async fn make_seed_iso(&self, src_dir: &Path, out: &Path) -> Result<(), BuildError> {
            self.called("seed");
            let mut all = Vec::new();
            for name in ["meta-data", "network-config", "user-data"] {
                all.extend(std::fs::read(src_dir.join(name))?);
            }
            std::fs::write(out, all)?;
            Ok(())
        }

        async fn prepare_bundle(
            &self,
            staging: &Path,
            disk: &Path,
            seed: &Path,
        ) -> Result<PathBuf, BuildError> {
            self.called("bundle");
            // The build hands over its own staging, root's 0700, which holds
            // the disk and the seed.
            assert_eq!(mode(staging), 0o700);
            assert_eq!(disk.parent(), Some(staging));
            assert_eq!(seed.parent(), Some(staging));
            // Finished in staging, then moved into place, as the real host does.
            let staged = staging.join(BUNDLE_NAME);
            std::fs::create_dir(&staged)?;
            std::fs::copy(disk, staged.join(DISK_FILE))?;
            std::fs::copy(seed, staged.join(SEED_FILE))?;
            let b = self.root.join(BUNDLE_NAME);
            assert!(!b.exists(), "the bundle never replaces anything");
            std::fs::rename(&staged, &b)?;
            Ok(b)
        }

        async fn run_launcher(
            &self,
            bundle: &Path,
            timeout: Duration,
            stop: Arc<AtomicBool>,
        ) -> Result<Option<i32>, BuildError> {
            use std::os::unix::fs::FileExt;
            self.called("launch");
            assert_eq!(timeout, Duration::from_secs(7 * 60));
            let started = tokio::time::Instant::now();
            while started.elapsed() < self.run_for {
                if stop.load(Ordering::SeqCst) {
                    // Killed: the guest wrote nothing it could report.
                    self.called("killed");
                    std::fs::write(bundle.join(CONSOLE_FILE), b"")?;
                    return Ok(None);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            std::fs::write(bundle.join(CONSOLE_FILE), &self.console)?;
            // The guest "provisions" its disk, inside it: a VM disk does not
            // grow.
            let f = OpenOptions::new()
                .write(true)
                .open(bundle.join(DISK_FILE))?;
            f.write_all_at(b" provisioned", (TARBALL.len() + b" -> disk".len()) as u64)?;
            Ok(self.exit)
        }

        async fn read_console(&self, bundle: &Path, cap: u64) -> Result<Vec<u8>, BuildError> {
            self.called("console");
            let all = std::fs::read(bundle.join(CONSOLE_FILE))?;
            let start = all.len().saturating_sub(cap as usize);
            Ok(all[start..].to_vec())
        }

        async fn land(&self, bundle: &Path, dest: &Path, max_bytes: u64) -> Result<(), BuildError> {
            self.called("land");
            let len = std::fs::metadata(bundle.join(DISK_FILE))?.len();
            if len > max_bytes {
                return Err(BuildError::Land(format!("{len} bytes; refused")));
            }
            std::fs::copy(bundle.join(DISK_FILE), dest)?;
            Ok(())
        }

        async fn remove_bundle(&self, bundle: &Path) {
            self.called("remove");
            let _ = std::fs::remove_dir_all(bundle);
        }

        async fn interrupt(&self) {
            match self.interrupt_after {
                Some(after) => tokio::time::sleep(after).await,
                None => std::future::pending().await,
            }
        }
    }

    fn spec() -> BuildSpec {
        static SHA: LazyLock<String> = LazyLock::new(|| sha512_hex(TARBALL));
        BuildSpec {
            pin: BasePin {
                build: "20261015-2632",
                url: TAR_URL,
                sums_url: SUMS_URL,
                sha512: SHA.as_str(),
            },
            provision_digest: "d".repeat(64),
            disk_bytes: 4096,
            max_tarball: 1024,
            max_sums: 1024,
            max_console: 1 << 20,
            launch_timeout: Duration::from_secs(7 * 60),
            bind_deadline: Duration::from_secs(3),
            wall_poll: Duration::from_millis(500),
            failed_console: None,
        }
    }

    fn mode(p: &Path) -> u32 {
        std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777
    }

    fn staging_is_empty(store: &Path) -> bool {
        std::fs::read_dir(store.join(".staging")).unwrap().count() == 0
    }

    #[tokio::test(start_paused = true)]
    async fn a_build_lands_the_disk_then_the_marker() {
        let tmp = TempDir::new();
        let store = tmp.0.join("images");
        let host = Fake::new(&tmp.0);
        let m = build(&host, &store, &spec(), 1234).await.unwrap();
        assert_eq!(m.docker_version, "29.0.1");
        assert_eq!(m.base_sha512, sha512_hex(TARBALL));
        assert_eq!(m.built_at_unix, 1234);
        assert_eq!(
            host.squashed(),
            [
                "wall", "download", "download", "extract", "seed", "bundle", "wall", "launch",
                "wall", "console", "land", "remove"
            ]
        );
        let linux = store.join(LINUX_DIR);
        let disk = std::fs::read(linux.join(DISK_FILE)).unwrap();
        assert!(disk.starts_with(TARBALL), "the guest's disk was landed");
        assert!(
            disk.windows(12).any(|w| w == b" provisioned"),
            "as the guest left it"
        );
        assert_eq!(disk.len() as u64, 4096);
        assert_eq!(mode(&linux.join(DISK_FILE)), 0o444);
        assert_eq!(mode(&linux.join(MARKER_FILE)), 0o444);
        assert_eq!(mode(&store), 0o755);
        assert_eq!(mode(&linux), 0o755);
        assert_eq!(mode(&store.join(".staging")), 0o700);
        let read: Marker =
            serde_json::from_slice(&std::fs::read(linux.join(MARKER_FILE)).unwrap()).unwrap();
        assert_eq!(read, m);
        assert!(staging_is_empty(&store));
        assert!(!tmp.0.join(BUNDLE_NAME).exists());
    }

    /// The previous image in place, to show a failure leaves it alone.
    fn previous(store: &Path) -> (Vec<u8>, Vec<u8>) {
        let linux = store.join(LINUX_DIR);
        std::fs::create_dir_all(&linux).unwrap();
        std::fs::write(linux.join(DISK_FILE), b"old disk").unwrap();
        std::fs::write(linux.join(MARKER_FILE), b"old marker").unwrap();
        (b"old disk".to_vec(), b"old marker".to_vec())
    }

    fn unchanged(store: &Path, before: &(Vec<u8>, Vec<u8>)) {
        let linux = store.join(LINUX_DIR);
        assert_eq!(std::fs::read(linux.join(DISK_FILE)).unwrap(), before.0);
        assert_eq!(std::fs::read(linux.join(MARKER_FILE)).unwrap(), before.1);
        assert!(staging_is_empty(store));
    }

    #[tokio::test(start_paused = true)]
    async fn a_tampered_base_is_refused_before_anything_runs() {
        let tmp = TempDir::new();
        let store = tmp.0.join("images");
        let before = previous(&store);
        let mut host = Fake::new(&tmp.0);
        host.served
            .insert(TAR_URL, b"pretend this is debian's tarbalL".to_vec());
        let e = build(&host, &store, &spec(), 1).await.unwrap_err();
        assert!(matches!(e, BuildError::HashMismatch), "{e}");
        assert_eq!(host.calls(), ["wall", "download"]);
        unchanged(&store, &before);
    }

    #[tokio::test(start_paused = true)]
    async fn sums_that_disagree_with_the_pin_are_refused() {
        let tmp = TempDir::new();
        let store = tmp.0.join("images");
        let mut host = Fake::new(&tmp.0);
        host.served.insert(
            SUMS_URL,
            format!("{}  base.tar.xz\n", "e".repeat(128)).into_bytes(),
        );
        let e = build(&host, &store, &spec(), 1).await.unwrap_err();
        assert!(matches!(e, BuildError::Sums(_)), "{e}");
        assert!(!host.calls().contains(&"extract"));
        assert!(staging_is_empty(&store));
    }

    #[tokio::test(start_paused = true)]
    async fn an_oversize_download_is_refused() {
        let tmp = TempDir::new();
        let store = tmp.0.join("images");
        let mut host = Fake::new(&tmp.0);
        host.served.insert(TAR_URL, vec![0u8; 2048]);
        let e = build(&host, &store, &spec(), 1).await.unwrap_err();
        assert!(matches!(e, BuildError::TooLarge { .. }), "{e}");
        assert!(staging_is_empty(&store));
    }

    #[tokio::test(start_paused = true)]
    async fn a_guest_failure_keeps_the_previous_image_and_names_the_step() {
        let tmp = TempDir::new();
        let store = tmp.0.join("images");
        let before = previous(&store);
        let mut host = Fake::new(&tmp.0);
        host.console = b"FABER-LINUX-IMAGE failed step=2\n".to_vec();
        let e = build(&host, &store, &spec(), 1).await.unwrap_err();
        assert!(e.to_string().contains("failed step=2"), "{e}");
        assert!(!host.calls().contains(&"land"));
        assert_eq!(host.calls().last(), Some(&"remove"));
        unchanged(&store, &before);
        assert!(!tmp.0.join(BUNDLE_NAME).exists());
    }

    /// A failed build keeps the console it read, root's alone, for the
    /// operator to read; a successful one keeps nothing.
    #[tokio::test(start_paused = true)]
    async fn a_failed_build_keeps_its_console_for_the_operator() {
        let tmp = TempDir::new();
        let store = tmp.0.join("images");
        let kept = tmp.0.join("kept.console.log");
        let mut s = spec();
        s.failed_console = Some(kept.clone());

        let mut host = Fake::new(&tmp.0);
        host.console = b"cloud-init: something went wrong\n".to_vec();
        let e = build(&host, &store, &s, 1).await.unwrap_err();
        assert!(matches!(e, BuildError::NoResult { .. }), "{e}");
        assert_eq!(
            std::fs::read(&kept).unwrap(),
            b"cloud-init: something went wrong\n"
        );
        assert_eq!(mode(&kept), 0o600);
        // The error itself still carries no guest bytes.
        assert!(!e.to_string().contains("cloud-init"), "{e}");

        std::fs::remove_file(&kept).unwrap();
        let ok = Fake::new(&tmp.0);
        build(&ok, &store, &s, 2).await.unwrap();
        assert!(!kept.exists(), "a successful build kept a console");
    }

    #[tokio::test(start_paused = true)]
    async fn a_launcher_that_did_not_exit_0_fails_the_build() {
        let tmp = TempDir::new();
        let store = tmp.0.join("images");
        let before = previous(&store);
        let mut host = Fake::new(&tmp.0);
        host.exit = Some(3);
        let e = build(&host, &store, &spec(), 1).await.unwrap_err();
        assert!(matches!(e, BuildError::NoResult { code: Some(3) }), "{e}");
        unchanged(&store, &before);
    }

    // --- the wall (#236) ---

    #[tokio::test(start_paused = true)]
    async fn no_wall_means_nothing_is_fetched_or_booted() {
        let tmp = TempDir::new();
        let store = tmp.0.join("images");
        let before = previous(&store);
        let mut host = Fake::new(&tmp.0);
        host.wall_ok = 0;
        let e = build(&host, &store, &spec(), 1).await.unwrap_err();
        assert!(matches!(e, BuildError::Wall(_)), "{e}");
        assert!(e.to_string().contains("nothing was booted"), "{e}");
        assert_eq!(host.calls(), ["wall"]);
        unchanged(&store, &before);
        assert!(!tmp.0.join(BUNDLE_NAME).exists());
    }

    #[tokio::test(start_paused = true)]
    async fn a_wall_gone_by_boot_time_boots_nothing() {
        let tmp = TempDir::new();
        let store = tmp.0.join("images");
        let before = previous(&store);
        let mut host = Fake::new(&tmp.0);
        host.wall_ok = 1;
        let e = build(&host, &store, &spec(), 1).await.unwrap_err();
        assert!(matches!(e, BuildError::Wall(_)), "{e}");
        let calls = host.calls();
        assert!(!calls.contains(&"launch"), "{calls:?}");
        assert!(!calls.contains(&"land"), "{calls:?}");
        assert_eq!(calls.last(), Some(&"remove"));
        unchanged(&store, &before);
        assert!(!tmp.0.join(BUNDLE_NAME).exists());
    }

    #[tokio::test(start_paused = true)]
    async fn a_guest_that_saw_no_wall_keeps_the_previous_image() {
        let tmp = TempDir::new();
        let store = tmp.0.join("images");
        let before = previous(&store);
        let mut host = Fake::new(&tmp.0);
        host.console = b"[ 3.0] cloud-init\r\nFABER-LINUX-IMAGE wall-not-up\r\n".to_vec();
        let e = build(&host, &store, &spec(), 1).await.unwrap_err();
        assert!(matches!(e, BuildError::WallNotUp { code: Some(0) }), "{e}");
        assert!(!host.calls().contains(&"land"));
        unchanged(&store, &before);
    }

    // --- the wall around the running build VM (#243) ---

    /// What every lost-wall build leaves: the VM killed, nothing landed, the
    /// bundle removed, the previous image as it was.
    fn killed_and_unchanged(host: &Fake, tmp: &TempDir, store: &Path, before: &(Vec<u8>, Vec<u8>)) {
        let calls = host.calls();
        assert!(calls.contains(&"killed"), "{calls:?}");
        assert!(!calls.contains(&"land"), "{calls:?}");
        assert_eq!(calls.last(), Some(&"remove"));
        unchanged(store, before);
        assert!(!tmp.0.join(BUNDLE_NAME).exists());
    }

    #[tokio::test(start_paused = true)]
    async fn a_wall_that_never_binds_the_build_network_kills_the_vm() {
        let tmp = TempDir::new();
        let store = tmp.0.join("images");
        let before = previous(&store);
        let mut host = Fake::new(&tmp.0);
        host.pending_polls = usize::MAX;
        let e = build(&host, &store, &spec(), 1).await.unwrap_err();
        assert!(matches!(e, BuildError::WallLost(_)), "{e}");
        assert!(e.to_string().contains("still not bound 3s after"), "{e}");
        killed_and_unchanged(&host, &tmp, &store, &before);
    }

    #[tokio::test(start_paused = true)]
    async fn a_wall_that_turns_red_mid_build_kills_the_vm() {
        let tmp = TempDir::new();
        let store = tmp.0.join("images");
        let before = previous(&store);
        let mut host = Fake::new(&tmp.0);
        // Two checks before boot, then bound, then red.
        host.wall_ok = 5;
        let e = build(&host, &store, &spec(), 1).await.unwrap_err();
        assert!(matches!(e, BuildError::WallLost(_)), "{e}");
        assert!(e.to_string().contains("no network 16"), "{e}");
        killed_and_unchanged(&host, &tmp, &store, &before);
    }

    #[tokio::test(start_paused = true)]
    async fn a_network_that_goes_back_to_pending_kills_the_vm() {
        let tmp = TempDir::new();
        let store = tmp.0.join("images");
        let before = previous(&store);
        let mut host = Fake::new(&tmp.0);
        host.repend_after = Some(2);
        let e = build(&host, &store, &spec(), 1).await.unwrap_err();
        assert!(matches!(e, BuildError::WallLost(_)), "{e}");
        assert!(e.to_string().contains("back to pending"), "{e}");
        killed_and_unchanged(&host, &tmp, &store, &before);
    }

    /// A build VM that finished before setup ever saw the wall bound is not
    /// trusted, whatever its console says.
    #[tokio::test(start_paused = true)]
    async fn a_build_that_ends_before_the_wall_was_seen_bound_fails() {
        let tmp = TempDir::new();
        let store = tmp.0.join("images");
        let before = previous(&store);
        let mut host = Fake::new(&tmp.0);
        host.pending_polls = usize::MAX;
        host.run_for = Duration::from_secs(1);
        let e = build(&host, &store, &spec(), 1).await.unwrap_err();
        assert!(matches!(e, BuildError::WallLost(_)), "{e}");
        assert!(e.to_string().contains("stopped before"), "{e}");
        assert!(!host.calls().contains(&"land"));
        unchanged(&store, &before);
    }

    /// SIGINT, SIGTERM or SIGHUP while the build VM runs: it is stopped, nothing lands, and
    /// the bundle and staging are removed.
    #[tokio::test(start_paused = true)]
    async fn a_signal_while_the_build_vm_runs_stops_it_and_lands_nothing() {
        let tmp = TempDir::new();
        let store = tmp.0.join("images");
        let before = previous(&store);
        let mut host = Fake::new(&tmp.0);
        host.run_for = Duration::from_secs(600);
        host.interrupt_after = Some(Duration::from_secs(2));
        let e = build(&host, &store, &spec(), 1).await.unwrap_err();
        assert!(matches!(e, BuildError::Interrupted), "{e}");
        assert!(host.calls().contains(&"killed"));
        assert!(!host.calls().contains(&"land"));
        assert!(host.calls().contains(&"remove"));
        unchanged(&store, &before);
    }

    /// Bound within the deadline after a few pending polls: the build lands.
    #[tokio::test(start_paused = true)]
    async fn a_wall_that_binds_within_the_deadline_lets_the_build_land() {
        let tmp = TempDir::new();
        let store = tmp.0.join("images");
        previous(&store);
        let mut host = Fake::new(&tmp.0);
        host.pending_polls = 4;
        build(&host, &store, &spec(), 1).await.unwrap();
        assert!(!host.calls().contains(&"killed"));
        assert!(host.calls().contains(&"land"));
    }

    // --- landing (#230) ---

    /// A guest (or `_faber_vm`) that left a disk longer than the image is
    /// made with: refused before anything is copied.
    #[tokio::test(start_paused = true)]
    async fn a_bundle_disk_longer_than_the_image_is_refused() {
        let tmp = TempDir::new();
        let store = tmp.0.join("images");
        let before = previous(&store);
        let mut host = Fake::new(&tmp.0);
        host.run_for = Duration::from_millis(1500);
        // Grown behind the build's back while the VM runs.
        let bundle_disk = tmp.0.join(BUNDLE_NAME).join(DISK_FILE);
        let grow = async {
            loop {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if bundle_disk.exists() {
                    std::fs::OpenOptions::new()
                        .write(true)
                        .open(&bundle_disk)
                        .unwrap()
                        .set_len(4097)
                        .unwrap();
                    break;
                }
            }
        };
        let spec = spec();
        let (r, ()) = tokio::join!(build(&host, &store, &spec, 1), grow);
        let e = r.unwrap_err();
        assert!(matches!(e, BuildError::Land(_)), "{e}");
        assert!(e.to_string().contains("4097"), "{e}");
        unchanged(&store, &before);
    }

    /// The renames of a landing, any one of which `fail` makes fail.
    fn failing_at(fail: usize) -> impl FnMut(&Path, &Path) -> std::io::Result<()> {
        let mut n = 0;
        move |from, to| {
            n += 1;
            if n == fail {
                return Err(std::io::Error::other("injected rename failure"));
            }
            std::fs::rename(from, to)
        }
    }

    fn staged_pair(dir: &Path) -> (PathBuf, PathBuf) {
        let (d, m) = (dir.join("disk.raw.landed"), dir.join("image.json.staged"));
        std::fs::write(&d, b"new disk").unwrap();
        std::fs::write(&m, b"new marker").unwrap();
        (d, m)
    }

    /// #230: whichever rename fails (the old marker aside, the old disk aside,
    /// the new disk in, the new marker in), the previous disk and its marker
    /// are back as they were, and no aside is left behind.
    #[test]
    fn a_failed_landing_keeps_the_previous_disk_and_marker() {
        for fail in 1..=4 {
            let tmp = TempDir::new();
            let store = tmp.0.join("images");
            let before = previous(&store);
            let linux = store.join(LINUX_DIR);
            let (d, m) = staged_pair(&tmp.0);
            let e = swap_in(&linux, &d, &m, &mut failing_at(fail)).unwrap_err();
            assert!(e.to_string().contains("injected"), "step {fail}: {e}");
            assert_eq!(std::fs::read(linux.join(DISK_FILE)).unwrap(), before.0);
            assert_eq!(std::fs::read(linux.join(MARKER_FILE)).unwrap(), before.1);
            let mut names: Vec<_> = std::fs::read_dir(&linux)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            assert_eq!(names, [DISK_FILE, MARKER_FILE], "step {fail}");
        }
        // And with nothing before: a failed marker leaves no disk behind.
        let tmp = TempDir::new();
        let linux = tmp.0.join("linux");
        std::fs::create_dir(&linux).unwrap();
        let (d, m) = staged_pair(&tmp.0);
        // The two aside renames find nothing (and still count); the fourth,
        // the new marker, fails, and the new disk is removed.
        assert!(swap_in(&linux, &d, &m, &mut failing_at(4)).is_err());
        assert_eq!(std::fs::read_dir(&linux).unwrap().count(), 0);
    }

    /// The old disk cannot be put back after the new marker failed: the old
    /// marker stays aside, never beside the new disk.
    #[test]
    fn a_marker_never_comes_back_beside_the_wrong_disk() {
        let tmp = TempDir::new();
        let store = tmp.0.join("images");
        previous(&store);
        let linux = store.join(LINUX_DIR);
        let (d, m) = staged_pair(&tmp.0);
        // 1, 2: asides; 3: new disk in; 4: new marker fails; 5: disk back fails.
        let mut n = 0;
        let e = swap_in(&linux, &d, &m, &mut |from, to| {
            n += 1;
            if n >= 4 {
                return Err(std::io::Error::other("injected rename failure"));
            }
            std::fs::rename(from, to)
        });
        assert!(e.is_err());
        assert!(
            !linux.join(MARKER_FILE).exists(),
            "no marker beside the new disk"
        );
        assert_eq!(std::fs::read(linux.join(DISK_FILE)).unwrap(), b"new disk");
        // The next landing recovers the old pair before anything else, then
        // lands the new one.
        std::fs::remove_file(linux.join(DISK_FILE)).unwrap();
        let (d, m) = staged_pair(&tmp.0);
        // Fails at once: the old image must still be there afterwards.
        assert!(swap_in(&linux, &d, &m, &mut failing_at(1)).is_err());
        assert_eq!(std::fs::read(linux.join(DISK_FILE)).unwrap(), b"old disk");
        assert_eq!(
            std::fs::read(linux.join(MARKER_FILE)).unwrap(),
            b"old marker"
        );
    }

    /// A run killed after moving the old pair aside: the next landing puts it
    /// back rather than deleting the only copy.
    #[test]
    fn asides_left_by_a_killed_run_are_recovered_not_deleted() {
        for new_disk_in in [false, true] {
            let tmp = TempDir::new();
            let linux = tmp.0.join("linux");
            std::fs::create_dir(&linux).unwrap();
            std::fs::write(linux.join("disk.raw.prev"), b"old disk").unwrap();
            std::fs::write(linux.join("image.json.prev"), b"old marker").unwrap();
            if new_disk_in {
                std::fs::write(linux.join(DISK_FILE), b"unmarked disk").unwrap();
            }
            let (d, m) = staged_pair(&tmp.0);
            assert!(swap_in(&linux, &d, &m, &mut failing_at(3)).is_err());
            assert_eq!(std::fs::read(linux.join(DISK_FILE)).unwrap(), b"old disk");
            assert_eq!(
                std::fs::read(linux.join(MARKER_FILE)).unwrap(),
                b"old marker"
            );
            assert_eq!(std::fs::read_dir(&linux).unwrap().count(), 2);
        }
    }

    /// A marker with no disk (the disk hand-deleted), and a run killed after the
    /// new disk went in: the old marker must not come back beside it.
    #[test]
    fn a_marker_aside_with_no_disk_aside_is_discarded() {
        let tmp = TempDir::new();
        let linux = tmp.0.join("linux");
        std::fs::create_dir(&linux).unwrap();
        // The state a kill leaves: the new disk in place, the old marker
        // aside, nothing else.
        std::fs::write(linux.join(DISK_FILE), b"new disk").unwrap();
        std::fs::write(linux.join("image.json.prev"), b"old marker").unwrap();
        let (d, m) = staged_pair(&tmp.0);
        // The next run recovers first: it must not pair the old marker with
        // the new disk, and it lands the new pair.
        swap_in(&linux, &d, &m, &mut failing_at(3)).unwrap_err();
        assert!(!linux.join(MARKER_FILE).exists(), "no stale marker");
        assert!(!linux.join("image.json.prev").exists());
    }

    /// A kill between the two asides leaves a marker with no disk and the disk
    /// aside: the disk comes back.
    #[test]
    fn a_disk_aside_beside_its_marker_is_recovered() {
        let tmp = TempDir::new();
        let linux = tmp.0.join("linux");
        std::fs::create_dir(&linux).unwrap();
        std::fs::write(linux.join("disk.raw.prev"), b"old disk").unwrap();
        std::fs::write(linux.join(MARKER_FILE), b"old marker").unwrap();
        let (d, m) = staged_pair(&tmp.0);
        assert!(swap_in(&linux, &d, &m, &mut failing_at(1)).is_err());
        assert_eq!(std::fs::read(linux.join(DISK_FILE)).unwrap(), b"old disk");
        assert_eq!(
            std::fs::read(linux.join(MARKER_FILE)).unwrap(),
            b"old marker"
        );
    }

    /// The happy path, and a stale aside pair from an interrupted run.
    #[test]
    fn a_landing_replaces_both_and_clears_stale_asides() {
        let tmp = TempDir::new();
        let store = tmp.0.join("images");
        previous(&store);
        let linux = store.join(LINUX_DIR);
        std::fs::write(linux.join("disk.raw.prev"), b"stale").unwrap();
        std::fs::write(linux.join("image.json.prev"), b"stale").unwrap();
        let (d, m) = staged_pair(&tmp.0);
        swap_in(&linux, &d, &m, &mut |f, t| std::fs::rename(f, t)).unwrap();
        assert_eq!(std::fs::read(linux.join(DISK_FILE)).unwrap(), b"new disk");
        assert_eq!(
            std::fs::read(linux.join(MARKER_FILE)).unwrap(),
            b"new marker"
        );
        assert_eq!(std::fs::read_dir(&linux).unwrap().count(), 2);
    }

    #[test]
    fn the_console_reports_a_wall_that_was_not_up() {
        let down = b"FABER-LINUX-IMAGE wall-not-up\n";
        let e = judge_console(Some(0), down).unwrap_err();
        assert!(matches!(e, BuildError::WallNotUp { code: Some(0) }), "{e}");
        let msg = e.to_string();
        assert!(
            msg.contains("no wall") && msg.contains("fetched nothing"),
            "{msg}"
        );
        // It outranks an ok line and a failed step, whatever the exit.
        let mixed = b"FABER-LINUX-IMAGE ok docker=29.0.1\nFABER-LINUX-IMAGE failed step=0\nFABER-LINUX-IMAGE wall-not-up\n";
        assert!(matches!(
            judge_console(Some(1), mixed),
            Err(BuildError::WallNotUp { code: Some(1) })
        ));
        // Only the exact line.
        for near in [
            &b"x FABER-LINUX-IMAGE wall-not-up\n"[..],
            b"FABER-LINUX-IMAGE wall-not-up now\n",
        ] {
            assert!(matches!(
                judge_console(Some(0), near),
                Err(BuildError::NoResult { .. })
            ));
        }
    }

    const PROVISION: &str = include_str!("../../launcher/linux/provision.sh");

    #[test]
    fn the_guest_waits_for_the_wall_before_its_first_fetch() {
        // cloud-init itself fetches nothing: no packages, no apt updates.
        for key in [
            "packages:",
            "package_update",
            "package_upgrade",
            "apt:",
            "bootcmd",
        ] {
            assert!(!USER_DATA_TEMPLATE.contains(key), "user-data has {key}");
        }
        assert!(USER_DATA_TEMPLATE.contains("runcmd:\n  - [/usr/local/libexec/faber-provision]"));
        // In provision.sh the gate runs before anything reaches the network.
        let gate = PROVISION
            .find("if ! wall_gate ")
            .expect("provision.sh calls the gate");
        let first_fetch = PROVISION
            .find("apt-get update")
            .expect("provision.sh fetches Docker");
        assert!(gate < first_fetch, "the gate must come first");
        for tool in ["apt-get ", "apt ", "curl ", "wget "] {
            assert!(!PROVISION[..gate].contains(tool), "{tool}before the gate");
        }
        // It probes this network's gateway, for the documented time, and its
        // failure is the line the console judge knows.
        assert!(PROVISION.contains(&format!("wall_gate {BUILD_GATEWAY}")));
        assert!(PROVISION.contains(&format!("deadline = time.monotonic() + {WALL_GATE_SECS}")));
        let marker = WALL_DOWN_LINE.strip_prefix("FABER-LINUX-IMAGE ").unwrap();
        assert!(PROVISION.contains(&format!("say \"$tag {marker}\"")));
        // Assembled at run time: the file never reads as the marker itself.
        assert!(!PROVISION.contains(WALL_DOWN_LINE));
        // And it is in what the seed carries, so the digest pins it.
        let f = GUEST_FILES
            .iter()
            .find(|f| f.name == "provision.sh")
            .unwrap();
        assert_eq!(f.bytes, PROVISION);
    }

    #[test]
    fn the_build_network_is_the_launchers() {
        let n = build_network();
        assert_eq!(n.subnet.to_string(), BUILD_SUBNET);
        // vmnet's gateway is the subnet's second address, the launcher's
        // default reservation its third, as for every job slot.
        assert_eq!(u32::from(n.gateway), u32::from(n.subnet.network()) + 1);
        assert_eq!(u32::from(BUILD_GUEST), u32::from(n.subnet.network()) + 2);
        assert_eq!(n.slots.len(), 1);
        assert_eq!(n.slots[0].id, BUILD_SLOT);
        assert_eq!(n.slots[0].addr, BUILD_GUEST);
        assert!(n.slots[0].profiles.is_empty(), "no profile, so no grant");
    }

    // --- judging the wall ---

    use crate::wall::grants::Grants;
    use crate::wall::model::NetworkPlan;
    use crate::wall::render::render;
    use crate::wall::report::{State, SCHEMA as VERDICT_SCHEMA};

    const NOW: u64 = 1_800_000_000;
    const FILE: u32 = 0o100_644;
    const DIR: u32 = 0o040_755;

    /// Job networks 0–15 at `192.168.240+n.0/24`, each with a profile, and
    /// the build network.
    fn plan_with_build() -> NetworkPlan {
        let mut networks: Vec<VmNetwork> = (0u8..=15)
            .map(|n| VmNetwork {
                id: n,
                gateway: Ipv4Addr::new(192, 168, 240 + n, 1),
                subnet: format!("192.168.{}.0/24", 240 + n).parse().unwrap(),
                slots: vec![VmSlot {
                    id: n,
                    addr: Ipv4Addr::new(192, 168, 240 + n, 2),
                    profiles: vec!["docker".into()],
                }],
            })
            .collect();
        networks.push(build_network());
        NetworkPlan { networks }
    }

    /// Grants for every profile there is, the LAN and the tailnet among them,
    /// and a resolver of the operator's.
    fn wide_grants() -> Grants {
        Grants::parse(
            r#"
[dns]
resolvers = ["9.9.9.9"]

[[grant]]
profile = "docker"
to = "0.0.0.0/0"
port = 443
proto = "tcp"

[[grant]]
profile = "docker"
to = "100.64.0.0/10"
port = 22
proto = "tcp"
"#,
        )
        .unwrap()
    }

    fn stored_with_build() -> StoredWall {
        render(&plan_with_build(), &wide_grants()).unwrap()
    }

    fn build_lines(s: &StoredWall) -> Vec<String> {
        s.networks
            .iter()
            .find(|n| n.id == BUILD_NETWORK_ID)
            .unwrap()
            .lines
            .clone()
    }

    #[test]
    fn the_build_network_validates_beside_every_job_network() {
        assert_eq!(plan_with_build().validate(), Ok(()));
    }

    #[test]
    fn the_build_network_renders_the_wall_and_no_grant() {
        let s = stored_with_build();
        let lines = build_lines(&s);
        let net = "faber-wall/n16/";
        assert!(lines.iter().all(|l| l.contains(net)), "{lines:#?}");
        // Its subnet is fenced for every slot, its guest's address named.
        assert!(lines
            .iter()
            .any(|l| l.contains("from 192.168.239.2 to <faber_host>")));
        assert!(lines
            .iter()
            .any(|l| l.contains("from 192.168.239.2 to 192.168.239.0/24")));
        let job = &s.networks.iter().find(|n| n.id == 3).unwrap().lines;
        assert!(job
            .iter()
            .any(|l| l.contains("to 192.168.239.0/24") && l.contains("/fence")));
        // DNS only to the operator's resolver, public unicast passed.
        assert!(lines
            .iter()
            .any(|l| l.contains("to 9.9.9.9 port = 53") && l.contains("/dns\"")));
        assert!(lines.iter().any(|l| l.ends_with("/n16/s15/public\"")));
        // No grant, though every job slot has two.
        assert!(!lines.iter().any(|l| l.contains("/grant\"")), "{lines:#?}");
        assert!(job.iter().any(|l| l.contains("/grant\"")));
        // The no-grant shape is exactly what it renders.
        let resolvers = ["9.9.9.9".parse().unwrap()].into_iter().collect();
        assert_eq!(
            lines,
            ungranted_lines(&build_network(), &resolvers, &plan_with_build())
        );
    }

    fn chain_for(path: &str) -> Vec<PathFacts> {
        let mut out = vec![PathFacts {
            path: path.into(),
            uid: 0,
            gid: 0,
            mode: FILE,
        }];
        let mut at = Path::new(path).parent();
        while let Some(dir) = at {
            out.push(PathFacts {
                path: dir.into(),
                uid: 0,
                gid: 0,
                mode: DIR,
            });
            at = dir.parent();
        }
        out
    }

    fn root_facts() -> MetaFacts {
        MetaFacts {
            is_symlink: false,
            is_regular: true,
            uid: 0,
            mode: FILE,
            parent_is_dir: true,
            parent_uid: 0,
            parent_mode: DIR,
        }
    }

    fn verdict_for(s: &StoredWall, state: State, at: u64) -> Vec<u8> {
        serde_json::to_vec(&VerdictFile {
            schema: VERDICT_SCHEMA,
            state,
            reasons: match state {
                State::Green => vec![],
                State::Red => vec!["pf is disabled".into()],
            },
            rules_version: s.rules_version,
            digest: s.digest.clone(),
            pending_networks: vec![BUILD_NETWORK_ID],
            checked_at_unix: at,
            load_generation: 1,
            counters: None,
        })
        .unwrap()
    }

    struct Case {
        stored: Vec<u8>,
        stored_chain: Vec<PathFacts>,
        verdict: Vec<u8>,
        facts: MetaFacts,
        verdict_chain: Vec<PathFacts>,
    }

    impl Case {
        fn green(s: &StoredWall) -> Self {
            Self {
                stored: s.to_json(),
                stored_chain: chain_for("/private/etc/faber/wall/stored.json"),
                verdict: verdict_for(s, State::Green, NOW - 3),
                facts: root_facts(),
                verdict_chain: chain_for("/private/var/db/faber/wall/verdict.json"),
            }
        }

        fn binding(&self) -> Result<Binding, String> {
            wall_binding(
                &build_network(),
                &WallEvidence {
                    stored: &self.stored,
                    stored_chain: &self.stored_chain,
                    verdict: &self.verdict,
                    verdict_facts: &self.facts,
                    verdict_chain: &self.verdict_chain,
                    now_unix: NOW,
                },
            )
        }

        fn judge(&self) -> Result<(), String> {
            wall_verdict(
                &build_network(),
                &WallEvidence {
                    stored: &self.stored,
                    stored_chain: &self.stored_chain,
                    verdict: &self.verdict,
                    verdict_facts: &self.facts,
                    verdict_chain: &self.verdict_chain,
                    now_unix: NOW,
                },
            )
        }
    }

    #[test]
    fn a_green_wall_holding_the_build_network_covers_it() {
        assert_eq!(Case::green(&stored_with_build()).judge(), Ok(()));
        // With the default resolvers too.
        let s = render(&plan_with_build(), &Grants::default()).unwrap();
        assert_eq!(Case::green(&s).judge(), Ok(()));
    }

    /// #243: the build network is bound once the checker's green verdict,
    /// for the stored set on disk, no longer lists it as pending; every
    /// refusal of the cover check is a refusal here too.
    #[test]
    fn binding_is_read_from_the_green_verdicts_pending_networks() {
        let s = stored_with_build();
        let mut c = Case::green(&s);
        assert_eq!(c.binding(), Ok(Binding::Pending));
        let mut v: VerdictFile = serde_json::from_slice(&c.verdict).unwrap();
        v.pending_networks = vec![3, 7];
        c.verdict = serde_json::to_vec(&v).unwrap();
        assert_eq!(c.binding(), Ok(Binding::Bound));
        // Red, stale, or for another stored set: refused, never bound.
        let mut red = Case::green(&s);
        let mut v: VerdictFile = serde_json::from_slice(&red.verdict).unwrap();
        v.pending_networks.clear();
        v.state = State::Red;
        red.verdict = serde_json::to_vec(&v).unwrap();
        assert!(red.binding().is_err());
        let mut stale = Case::green(&s);
        let mut v: VerdictFile = serde_json::from_slice(&stale.verdict).unwrap();
        v.pending_networks.clear();
        v.checked_at_unix = NOW - VERDICT_MAX_AGE_SECS - 1;
        stale.verdict = serde_json::to_vec(&v).unwrap();
        assert!(stale.binding().is_err());
        let mut other = Case::green(&s);
        let mut v: VerdictFile = serde_json::from_slice(&other.verdict).unwrap();
        v.pending_networks.clear();
        v.digest = "0".repeat(64);
        other.verdict = serde_json::to_vec(&v).unwrap();
        let e = other.binding().unwrap_err();
        assert!(e.contains("another stored rule set"), "{e}");
    }

    #[test]
    fn a_plan_without_the_build_network_refuses() {
        let mut p = plan_with_build();
        p.networks.retain(|n| n.id != BUILD_NETWORK_ID);
        let s = render(&p, &wide_grants()).unwrap();
        let e = Case::green(&s).judge().unwrap_err();
        assert!(e.contains("no network 16"), "{e}");
    }

    #[test]
    fn a_build_network_with_a_profile_or_other_addresses_refuses() {
        let mut p = plan_with_build();
        let b = p.networks.last_mut().unwrap();
        b.slots[0].profiles = vec!["docker".into()];
        let s = render(&p, &wide_grants()).unwrap();
        let e = Case::green(&s).judge().unwrap_err();
        assert!(e.contains("not the build network"), "{e}");

        for edit in [
            (|n: &mut VmNetwork| n.slots[0].addr = Ipv4Addr::new(192, 168, 239, 9))
                as fn(&mut VmNetwork),
            |n| n.slots[0].id = 1,
            |n| n.gateway = Ipv4Addr::new(192, 168, 239, 254),
            |n| {
                n.slots.push(VmSlot {
                    id: 1,
                    addr: Ipv4Addr::new(192, 168, 239, 3),
                    profiles: vec![],
                })
            },
        ] {
            let mut p = plan_with_build();
            edit(p.networks.last_mut().unwrap());
            let s = render(&p, &wide_grants()).unwrap();
            assert!(Case::green(&s).judge().is_err());
        }
    }

    /// `s` with the build network's lines edited and the set resealed, as root
    /// could have stored it.
    fn resealed(edit: impl FnOnce(&mut Vec<String>)) -> StoredWall {
        let mut s = stored_with_build();
        let n = s
            .networks
            .iter_mut()
            .find(|n| n.id == BUILD_NETWORK_ID)
            .unwrap();
        edit(&mut n.lines);
        s.digest = StoredWall::digest_of(&s.networks);
        StoredWall::from_json(&s.to_json()).unwrap()
    }

    /// One edit to the build network's stored lines.
    type LinesEdit = Box<dyn FnOnce(&mut Vec<String>)>;

    #[test]
    fn build_rules_other_than_the_no_grant_shape_refuse() {
        let grant = "pass in quick on {if} inet proto tcp from 192.168.239.2 to 10.0.0.0/8 port = 22 flags S/SA keep state label \"faber-wall/n16/s15/grant\"".to_string();
        let cases: [LinesEdit; 3] = [
            Box::new(move |l| {
                let at = l.iter().position(|x| x.contains("/nonpublic\"")).unwrap();
                l.insert(at, grant);
            }),
            Box::new(|l| {
                l.retain(|x| !x.contains("/host\""));
            }),
            Box::new(|l| {
                l.pop();
            }),
        ];
        for edit in cases {
            let s = resealed(edit);
            let e = Case::green(&s).judge().unwrap_err();
            assert!(e.contains("not those of a slot with no grant"), "{e}");
        }
    }

    #[test]
    fn an_untrusted_or_unreadable_stored_set_refuses() {
        let s = stored_with_build();
        let mut c = Case::green(&s);
        c.stored_chain[1].uid = 451;
        let e = c.judge().unwrap_err();
        assert!(e.contains("not root's alone"), "{e}");

        let mut c = Case::green(&s);
        c.stored = b"{}".to_vec();
        assert!(c.judge().is_err());

        // A digest that does not match its rules.
        let mut tampered = s.clone();
        tampered.networks.last_mut().unwrap().lines.pop();
        let mut c = Case::green(&s);
        c.stored = tampered.to_json();
        assert!(c.judge().unwrap_err().contains("digest"));
    }

    #[test]
    fn anything_but_a_fresh_green_verdict_for_this_set_refuses() {
        let s = stored_with_build();

        let mut c = Case::green(&s);
        c.verdict = verdict_for(&s, State::Red, NOW);
        assert!(c.judge().unwrap_err().contains("pf is disabled"));

        let mut c = Case::green(&s);
        c.verdict = verdict_for(&s, State::Green, NOW - VERDICT_MAX_AGE_SECS - 1);
        assert!(c.judge().unwrap_err().contains("stopped"));

        let mut c = Case::green(&s);
        c.facts.uid = 451;
        assert!(c.judge().is_err());

        let mut c = Case::green(&s);
        c.verdict_chain[2].mode = 0o040_777;
        assert!(c.judge().unwrap_err().contains("not root's alone"));

        let mut other = s.clone();
        other.digest = "0".repeat(64);
        let mut c = Case::green(&s);
        c.verdict = verdict_for(&other, State::Green, NOW);
        assert!(c.judge().unwrap_err().contains("another stored rule set"));

        let mut c = Case::green(&s);
        c.verdict = b"not json".to_vec();
        assert!(c.judge().is_err());
    }
}
