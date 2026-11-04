//! `faber golden prepare`: the operator's half of building the golden macOS image
//! (ADR-0049 §2, delivery step 2D).
//!
//! ```text
//! faber golden prepare <bundle> [--ipsw FILE --sha256 HEX] [--keep-ipsw] [--disk-gib N]
//!                      [--xcode PATH] [--runtime PLATFORM[:VERSION]]... [--no-host-runtimes]
//!                      [--brew FORMULA]... [--from NAME] [--yes]
//! ```
//!
//! Run as the operator, never with sudo, from a desktop session. It drives
//! `faber-golden-prep` (`launcher/golden/prepare.swift`), the one tool that installs macOS
//! from Apple's restore image and opens a window, so this command holds the policy and the
//! tool holds the framework calls. In order:
//!
//! 1. **Where.** The bundle's parent must be on the same volume as the image store, and the
//!    volume must have the room ADR-0049 §2 gives (about 85 GB at peak, plus ADR-0024's
//!    10 GiB floor). Each large step checks again against its own need.
//! 2. **The recipe**, shown before anything starts and confirmed unless `--yes`: the Xcode
//!    (the flag, the host's `Xcode.app`, or Apple's download page and a wait for the file),
//!    checked against Apple's signature now so a bad one is found before the hours are
//!    spent; the simulator runtimes (the host's, when it is the same Xcode); the extra
//!    Homebrew formulas. `--from NAME` prefills it from an image's stored manifest.
//! 3. **Install**: the restore image is fetched and verified, and macOS is installed into
//!    the bundle with no screen.
//! 4. **Boot**: a window with no network device and a read-only setup disk carrying
//!    `arm.sh`. The operator does Setup Assistant offline and runs one command; the guest
//!    powers itself off.
//! 5. The recipe is written to `<bundle>/recipe.json` and the exact `sudo faber golden
//!    build …` is printed.
//!
//! # Resuming
//!
//! A sidecar beside the bundle (`.<name>.prepare`) records the last phase that finished. A
//! bundle that finished the install skips to the boot; one that finished the boot skips to
//! printing the command. A bundle with no sidecar is refused, never adopted: whether it was
//! already armed cannot be known, and booting an armed bundle would run the provisioner in
//! the offline window.
//!
//! # What this cannot prove
//!
//! The host's `Xcode.app` signature check is `codesign` against Apple's requirement on the
//! operator's own copy; it catches a corrupt or modified Xcode, and says nothing about a
//! host an attacker already controls. The build checks the Xcode again on root's copy.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::golden_build::{
    brew_problem, parse_runtime, Recipe, RuntimeSpec, XcodeKind, BOOTSTRAP_SH, GUEST_AGENT_PLIST,
    PROVISION_SH,
};
use super::vm_images::{golden_name_problem, MAX_OPTIONAL_BYTES};

// ---------------------------------------------------------------------------
// What is embedded, and what the numbers are
// ---------------------------------------------------------------------------

/// The offline window's script, compiled in so its version always matches this binary.
pub const ARM_SH: &str = include_str!("../../launcher/golden/arm.sh");
/// The wrapper `arm.sh` installs.
pub const PROVISION_WRAPPER_SH: &str = include_str!("../../launcher/golden/provision-wrapper.sh");
/// The run-once LaunchDaemon `arm.sh` installs.
pub const PROVISION_PLIST: &str =
    include_str!("../../launcher/golden/io.github.rejifald.faber.golden-provision.plist");

/// Everything on the window VM's setup disk: `(name, contents)`. `arm.sh` installs the
/// wrapper and the daemon; the build's own copy of the rest is on its disk, but `arm.sh`
/// checks for these too, so the window's disk carries the same set.
pub fn setup_files() -> Vec<(&'static str, &'static str)> {
    vec![
        ("arm.sh", ARM_SH),
        ("provision.sh", PROVISION_SH),
        ("provision-wrapper.sh", PROVISION_WRAPPER_SH),
        (
            "io.github.rejifald.faber.golden-provision.plist",
            PROVISION_PLIST,
        ),
        ("bootstrap.sh", BOOTSTRAP_SH),
        (
            "io.github.rejifald.faber.guest-agent.plist",
            GUEST_AGENT_PLIST,
        ),
    ]
}

/// The disk the whole build peaks at on one volume (ADR-0049 §2): the restore image
/// (~25 GB), an Xcode disk (~15-20 GB) and the VM disk (~40 GB).
pub const PEAK_BYTES: u64 = 85 << 30;
/// ADR-0024's floor: what stays free for CI.
pub const FLOOR_BYTES: u64 = 10 << 30;
/// What the macOS install writes into the bundle (26 GiB for 27.0.1), with headroom.
pub const INSTALL_BYTES: u64 = 30 << 30;
/// What a boot adds: the setup disk and the window VM's own writes.
pub const BOOT_BYTES: u64 = 2 << 30;
/// The guest disk's default size. It is sparse.
pub const DEFAULT_DISK_GIB: u32 = 100;
/// The window VM's size, as `build.sh` had it.
pub const CPUS: u32 = 4;
pub const MEMORY_GIB: u32 = 8;
/// The recipe file `prepare` leaves in the bundle. It is
/// [`super::golden_build::PREPARED_EXTRAS`]'s one entry.
pub const RECIPE_FILE: &str = "recipe.json";
/// The most of `recipe.json` read back.
const MAX_RECIPE_BYTES: u64 = 64 << 10;
/// The most of `xcrun simctl runtime list -j` read.
pub const MAX_SIMCTL_BYTES: usize = 1 << 20;

// ---------------------------------------------------------------------------
// The request
// ---------------------------------------------------------------------------

/// `faber golden prepare`'s arguments.
#[derive(Debug, Clone, PartialEq, Eq, clap::Args)]
pub struct PrepareArgs {
    /// The new bundle's directory, such as `~/faber-golden/macos-27`. It must not
    /// exist yet, or be one this command is resuming. Its parent must be on the same
    /// volume as /var/db/faber/images.
    pub bundle: PathBuf,
    /// Install from this restore image instead of downloading the newest one this host
    /// supports.
    #[arg(long, value_name = "FILE")]
    pub ipsw: Option<PathBuf>,
    /// Refuse the restore image unless its SHA-256 is this (64 lowercase hex digits).
    #[arg(long, value_name = "HEX")]
    pub sha256: Option<String>,
    /// Keep a downloaded restore image (default: delete it after the install).
    #[arg(long)]
    pub keep_ipsw: bool,
    /// The guest disk's size. It is sparse: it takes only what the guest writes.
    #[arg(long, value_name = "N", default_value_t = DEFAULT_DISK_GIB)]
    pub disk_gib: u32,
    /// The Xcode to install: an `Xcode.app` or an `Xcode_*.xip`. Default: this host's
    /// /Applications/Xcode.app, after asking; else Apple's download page.
    #[arg(long, value_name = "PATH")]
    pub xcode: Option<PathBuf>,
    /// A simulator runtime for the image, such as `iOS` or `iOS:18.5`. Repeatable.
    /// Replaces the runtimes inferred from this host.
    #[arg(long = "runtime", value_name = "PLATFORM[:VERSION]")]
    pub runtimes: Vec<String>,
    /// Install only the matching iOS runtime, not this host's.
    #[arg(long, conflicts_with = "runtimes")]
    pub no_host_runtimes: bool,
    /// A Homebrew formula to add to jq, gh, zstd and git-lfs. Repeatable.
    #[arg(long = "brew", value_name = "FORMULA")]
    pub brew: Vec<String>,
    /// Start from the recipe stored with this image, `/var/db/faber/images/NAME`. Flags
    /// override parts of it.
    #[arg(long, value_name = "NAME")]
    pub from: Option<String>,
    /// Do not ask for confirmation.
    #[arg(long, short = 'y')]
    pub yes: bool,
}

/// Validated arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareRequest {
    pub bundle: PathBuf,
    pub parent: PathBuf,
    pub bundle_name: String,
    pub ipsw: Option<PathBuf>,
    pub sha256: Option<String>,
    pub keep_ipsw: bool,
    pub disk_gib: u32,
    pub xcode: Option<(PathBuf, XcodeKind)>,
    pub runtimes: Vec<String>,
    pub no_host_runtimes: bool,
    pub brew: Vec<String>,
    pub from: Option<String>,
    pub yes: bool,
}

/// `path` made absolute against `cwd`, with `.` and `..` resolved lexically.
pub fn absolute(path: &Path, cwd: &Path) -> PathBuf {
    use std::path::Component;
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    let mut out = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Why `path` cannot be printed into a command line and read back: not UTF-8, or holding
/// a control character.
fn path_text_problem(what: &str, path: &Path) -> Option<String> {
    match path.to_str() {
        None => Some(format!("{what} is not valid UTF-8")),
        Some(s) if s.chars().any(|c| c.is_control()) => {
            Some(format!("{what} holds a control character"))
        }
        Some(_) => None,
    }
}

/// A restore image's SHA-256, as `--sha256` and the tool's output spell it.
pub fn sha256_problem(s: &str) -> Option<String> {
    let ok = s.len() == 64 && s.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f'));
    (!ok).then(|| "--sha256 takes 64 lowercase hex digits".to_string())
}

impl PrepareRequest {
    /// Whether any flag that shapes the recipe was given.
    pub fn has_recipe_flags(&self) -> bool {
        self.xcode.is_some()
            || !self.runtimes.is_empty()
            || self.no_host_runtimes
            || !self.brew.is_empty()
            || self.from.is_some()
    }

    /// Check the flags. Nothing is read from disk.
    pub fn new(args: &PrepareArgs, cwd: &Path) -> Result<Self, String> {
        let bundle = absolute(&args.bundle, cwd);
        if let Some(why) = path_text_problem("the bundle path", &bundle) {
            return Err(why);
        }
        let (Some(parent), Some(name)) = (bundle.parent(), bundle.file_name()) else {
            return Err("give a bundle directory, not /".into());
        };
        let name = name.to_str().unwrap_or_default().to_string();
        let name_ok = !name.is_empty()
            && name.len() <= 64
            && !name.starts_with('.')
            && name
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'));
        if !name_ok {
            return Err(
                "the bundle's name may hold only [A-Za-z0-9._-], up to 64 characters, and must \
                 not start with a dot"
                    .into(),
            );
        }
        if !(64..=2048).contains(&args.disk_gib) {
            return Err("--disk-gib must be 64 to 2048".into());
        }
        if let Some(h) = &args.sha256 {
            if let Some(why) = sha256_problem(h) {
                return Err(why);
            }
        }
        let ipsw = args.ipsw.as_ref().map(|p| absolute(p, cwd));
        if let Some(p) = &ipsw {
            if let Some(why) = path_text_problem("--ipsw", p) {
                return Err(why);
            }
        }
        let xcode = match &args.xcode {
            None => None,
            Some(p) => {
                let p = absolute(p, cwd);
                if let Some(why) = path_text_problem("--xcode", &p) {
                    return Err(why);
                }
                let kind = XcodeKind::of_path(&p).ok_or_else(|| {
                    "--xcode is an Xcode.app or an Xcode_*.xip: its name must end in .app or .xip"
                        .to_string()
                })?;
                Some((p, kind))
            }
        };
        // The same rules the build holds them to, so a typo costs seconds, not a boot.
        for r in &args.runtimes {
            parse_runtime(r)?;
        }
        for f in &args.brew {
            if let Some(why) = brew_problem(f) {
                return Err(why);
            }
        }
        if let Some(n) = &args.from {
            if let Some(why) = golden_name_problem(n) {
                return Err(why.replace("--golden-name", "--from"));
            }
        }
        Ok(Self {
            parent: parent.to_path_buf(),
            bundle,
            bundle_name: name,
            ipsw,
            sha256: args.sha256.clone(),
            keep_ipsw: args.keep_ipsw,
            disk_gib: args.disk_gib,
            xcode,
            runtimes: args.runtimes.clone(),
            no_host_runtimes: args.no_host_runtimes,
            brew: args.brew.clone(),
            from: args.from.clone(),
            yes: args.yes,
        })
    }
}

// ---------------------------------------------------------------------------
// Xcode identity
// ---------------------------------------------------------------------------

/// What identifies an Xcode: its marketing version, and its build when it can be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XcodeIdentity {
    pub version: String,
    pub build: Option<String>,
}

/// `1` to `3` dot-separated numbers of up to four digits.
fn version_text_ok(v: &str) -> bool {
    let parts: Vec<&str> = v.split('.').collect();
    (1..=3).contains(&parts.len())
        && parts
            .iter()
            .all(|p| (1..=4).contains(&p.len()) && p.bytes().all(|c| c.is_ascii_digit()))
}

impl XcodeIdentity {
    /// From an `Xcode.app`'s `Contents/version.plist`, converted with `plutil -convert
    /// json`.
    pub fn from_version_plist_json(json: &str) -> Result<Self, String> {
        let v: serde_json::Value =
            serde_json::from_str(json).map_err(|_| "version.plist is not readable".to_string())?;
        let version = v
            .get("CFBundleShortVersionString")
            .and_then(|s| s.as_str())
            .filter(|s| version_text_ok(s))
            .ok_or("version.plist has no usable CFBundleShortVersionString")?;
        let build = v
            .get("ProductBuildVersion")
            .and_then(|s| s.as_str())
            .filter(|s| (3..=16).contains(&s.len()) && s.bytes().all(|c| c.is_ascii_alphanumeric()))
            .map(str::to_string);
        Ok(Self {
            version: version.to_string(),
            build,
        })
    }

    /// From an `Xcode_<version>….xip`'s file name (Apple's own naming), or `None`.
    pub fn from_xip_name(file_name: &str) -> Option<Self> {
        let rest = file_name.strip_prefix("Xcode")?;
        let rest = rest.trim_start_matches(['_', ' ', '-']);
        let ver: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let ver = ver.trim_end_matches('.');
        version_text_ok(ver).then(|| Self {
            version: ver.to_string(),
            build: None,
        })
    }

    pub fn major(&self) -> u32 {
        self.version
            .split('.')
            .next()
            .and_then(|m| m.parse().ok())
            .unwrap_or(0)
    }

    /// The simulator SDKs' major version, when it can be told. Since Xcode 26 every
    /// platform's SDK carries the Xcode's own major number. Before that the numbers
    /// differ per platform, and nothing is inferred.
    pub fn sdk_major(&self) -> Option<u32> {
        (self.major() >= 26).then(|| self.major())
    }

    /// The same Xcode: equal versions, and equal builds when both are known.
    pub fn same_as(&self, other: &XcodeIdentity) -> bool {
        self.version == other.version
            && match (&self.build, &other.build) {
                (Some(a), Some(b)) => a == b,
                _ => true,
            }
    }

    /// `27.1 (27A9275)`, or just the version.
    pub fn shown(&self) -> String {
        match &self.build {
            Some(b) => format!("{} ({b})", self.version),
            None => self.version.clone(),
        }
    }
}

/// The image's default name for an Xcode: `xcode-27.1`.
pub fn default_image_name(xcode: &XcodeIdentity) -> String {
    format!("xcode-{}", xcode.version)
}

/// Apple's download page for the Xcode that matches macOS `major`, or the page's Xcode
/// search when it is not known. The operator signs in there; faber never sees the Apple
/// ID.
pub fn xcode_download_url(macos_major: Option<u32>) -> String {
    match macos_major {
        Some(m) => format!("https://developer.apple.com/download/all/?q=Xcode%20{m}"),
        None => "https://developer.apple.com/download/all/?q=Xcode".to_string(),
    }
}

/// Whether a file name in ~/Downloads is a finished Xcode download: `Xcode*.xip`, and not
/// a browser's partial (`.download`, `.crdownload`, `.part` end in something else).
pub fn is_xcode_xip_name(name: &str) -> bool {
    name.starts_with("Xcode") && name.ends_with(".xip") && name.len() > "Xcode.xip".len() - 1
}

// ---------------------------------------------------------------------------
// Signatures
// ---------------------------------------------------------------------------

/// Whether `pkgutil --check-signature` on an `.xip` reports Apple's own signature. The
/// guest's `xip -x` checks it again; this tells the operator now.
pub fn xip_signature_problem(success: bool, out: &str) -> Option<String> {
    let signed = out
        .lines()
        .any(|l| l.trim() == "Status: signed Apple Software");
    let apple_root = out.lines().any(|l| l.contains("Apple Root CA"));
    if success && signed && apple_root {
        return None;
    }
    Some(
        "pkgutil does not report this file as signed Apple Software (a corrupt, partial or \
         modified download). Download Xcode again from Apple"
            .to_string(),
    )
}

// ---------------------------------------------------------------------------
// Runtimes
// ---------------------------------------------------------------------------

/// A simulator runtime installed on this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostRuntime {
    pub platform: &'static str,
    pub version: String,
    pub build: String,
    pub size_bytes: Option<u64>,
}

#[derive(Deserialize)]
struct SimRuntime {
    #[serde(default)]
    state: Option<String>,
    #[serde(rename = "platformIdentifier", default)]
    platform_identifier: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    build: Option<String>,
    #[serde(rename = "sizeBytes", default)]
    size_bytes: Option<u64>,
}

fn platform_of(identifier: &str) -> Option<&'static str> {
    match identifier {
        "com.apple.platform.iphonesimulator" => Some("iOS"),
        "com.apple.platform.watchsimulator" => Some("watchOS"),
        "com.apple.platform.appletvsimulator" => Some("tvOS"),
        "com.apple.platform.xrsimulator" => Some("visionOS"),
        _ => None,
    }
}

fn version_key(v: &str) -> Vec<u32> {
    v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
}

/// The ready runtimes in `xcrun simctl runtime list -j`: platform, version, build and
/// size. Runtimes that are not `Ready`, of a platform this build does not install, or
/// whose version this build would refuse, are left out. Sorted by platform, then version.
pub fn parse_simctl_runtimes(json: &str) -> Result<Vec<HostRuntime>, String> {
    if json.len() > MAX_SIMCTL_BYTES {
        return Err("the runtime list is unreasonably large".into());
    }
    let all: std::collections::BTreeMap<String, SimRuntime> =
        serde_json::from_str(json).map_err(|_| {
            "xcrun simctl runtime list printed JSON this version does not read".to_string()
        })?;
    let mut out: Vec<HostRuntime> = Vec::new();
    for rt in all.into_values() {
        let (Some(state), Some(id), Some(version)) = (rt.state, rt.platform_identifier, rt.version)
        else {
            continue;
        };
        let Some(platform) = platform_of(&id) else {
            continue;
        };
        if state != "Ready" || parse_runtime(&format!("{platform}:{version}")).is_err() {
            continue;
        }
        let host = HostRuntime {
            platform,
            version,
            build: rt.build.unwrap_or_default(),
            size_bytes: rt.size_bytes,
        };
        if !out
            .iter()
            .any(|r| r.platform == host.platform && r.version == host.version)
        {
            out.push(host);
        }
    }
    let order = |p: &str| {
        super::golden_build::PLATFORMS
            .iter()
            .position(|x| *x == p)
            .unwrap_or(usize::MAX)
    };
    out.sort_by(|a, b| {
        order(a.platform)
            .cmp(&order(b.platform))
            .then_with(|| version_key(&a.version).cmp(&version_key(&b.version)))
    });
    Ok(out)
}

/// One runtime the image will carry, with what is known about its size and why it is there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeChoice {
    pub spec: RuntimeSpec,
    pub size_bytes: Option<u64>,
    pub why: String,
}

/// The runtimes the recipe will name, and the facts the operator should read.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RuntimePlan {
    pub choices: Vec<RuntimeChoice>,
    pub notes: Vec<String>,
}

impl RuntimePlan {
    /// The sizes that are known, summed, and whether any runtime has none.
    pub fn total(&self) -> (u64, bool) {
        let known: u64 = self.choices.iter().filter_map(|c| c.size_bytes).sum();
        (known, self.choices.iter().any(|c| c.size_bytes.is_none()))
    }

    pub fn tokens(&self) -> Vec<String> {
        self.choices.iter().map(|c| c.spec.token()).collect()
    }
}

/// Where the runtime list comes from.
#[derive(Debug, Clone, Copy)]
pub enum RuntimeSource<'a> {
    /// `--runtime` flags: replace any inference.
    Explicit(&'a [String]),
    /// The `--from` image's recipe.
    From(&'a [String], &'a str),
    /// Inferred from the host.
    Inferred { no_host_runtimes: bool },
}

fn matching_ios() -> RuntimeSpec {
    RuntimeSpec {
        platform: "iOS".into(),
        version: None,
    }
}

fn host_size(host: &[HostRuntime], spec: &RuntimeSpec) -> Option<u64> {
    let v = spec.version.as_deref()?;
    host.iter()
        .find(|r| r.platform == spec.platform && r.version == v)
        .and_then(|r| r.size_bytes)
}

/// The runtimes the image gets (ADR-0049 §4).
///
/// Inferred, the default is this host's installed runtimes that the chosen Xcode's SDK can
/// run (their major is at most the SDK's), plus the matching iOS runtime when none of them
/// is one. When the Xcode is not this host's, or its SDK cannot be told, or the host has
/// none, it falls back to the matching iOS runtime and says why. `--runtime` replaces the
/// inference; `--no-host-runtimes` keeps only the matching iOS runtime.
pub fn plan_runtimes(
    source: RuntimeSource<'_>,
    xcode: &XcodeIdentity,
    host_xcode: Option<&XcodeIdentity>,
    host_runtimes: &[HostRuntime],
) -> Result<RuntimePlan, String> {
    let mut plan = RuntimePlan::default();
    let push = |plan: &mut RuntimePlan, spec: RuntimeSpec, why: String| {
        if plan.choices.iter().any(|c| c.spec == spec) {
            return;
        }
        let size = host_size(host_runtimes, &spec);
        plan.choices.push(RuntimeChoice {
            spec,
            size_bytes: size,
            why,
        });
    };
    match source {
        RuntimeSource::Explicit(list) => {
            for r in list {
                push(&mut plan, parse_runtime(r)?, "from --runtime".into());
            }
        }
        RuntimeSource::From(list, image) => {
            for r in list {
                push(
                    &mut plan,
                    parse_runtime(r)?,
                    format!("from image {image}'s recipe"),
                );
            }
        }
        RuntimeSource::Inferred { no_host_runtimes } => {
            let matching = "the matching runtime".to_string();
            if no_host_runtimes {
                plan.notes
                    .push("--no-host-runtimes: only the matching iOS runtime".into());
                push(&mut plan, matching_ios(), matching);
            } else if !host_xcode.is_some_and(|h| h.same_as(xcode)) {
                plan.notes.push(match host_xcode {
                    Some(h) => format!(
                        "this host has Xcode {}, not {}: installing the matching iOS runtime \
                         instead of this host's runtimes",
                        h.shown(),
                        xcode.shown()
                    ),
                    None => {
                        "this host has no Xcode: installing the matching iOS runtime".to_string()
                    }
                });
                push(&mut plan, matching_ios(), matching);
            } else if let Some(sdk) = xcode.sdk_major() {
                let mut skipped = Vec::new();
                for r in host_runtimes {
                    if version_key(&r.version).first().copied().unwrap_or(0) <= sdk {
                        let spec = RuntimeSpec {
                            platform: r.platform.to_string(),
                            version: Some(r.version.clone()),
                        };
                        push(&mut plan, spec, "installed on this host".into());
                    } else {
                        skipped.push(format!("{} {}", r.platform, r.version));
                    }
                }
                if !skipped.is_empty() {
                    plan.notes.push(format!(
                        "left out, newer than Xcode {}'s SDK ({sdk}): {}",
                        xcode.version,
                        skipped.join(", ")
                    ));
                }
                let has_matching_ios = plan.choices.iter().any(|c| {
                    c.spec.platform == "iOS"
                        && c.spec
                            .version
                            .as_deref()
                            .is_some_and(|v| version_key(v).first() == Some(&sdk))
                });
                if !has_matching_ios {
                    plan.notes.push(format!(
                        "this host has no iOS {sdk} runtime: adding the matching one"
                    ));
                    plan.choices.insert(
                        0,
                        RuntimeChoice {
                            spec: matching_ios(),
                            size_bytes: None,
                            why: matching,
                        },
                    );
                }
            } else {
                plan.notes.push(format!(
                    "Xcode {} predates the single version number of Xcode 26: installing the \
                     matching iOS runtime, not this host's",
                    xcode.version
                ));
                push(&mut plan, matching_ios(), matching);
            }
        }
    }
    Ok(plan)
}

// ---------------------------------------------------------------------------
// --from
// ---------------------------------------------------------------------------

/// What `--from NAME` takes out of an image's `manifest.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prefill {
    pub runtimes: Vec<String>,
    pub brew: Vec<String>,
}

/// The manifest as `faber golden build` writes it, read strictly: schema 1, no field this
/// version does not know in the recipe, every value held to the rules the build holds it to.
/// What the guest reported (`installed`) is a record and is never read.
#[derive(Deserialize)]
#[allow(dead_code)] // the guest-reported fields exist to be accepted, never read
#[serde(deny_unknown_fields)]
struct ManifestIn {
    schema: u32,
    recipe: ManifestRecipe,
    #[serde(default)]
    installed: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    built_at: Option<serde::de::IgnoredAny>,
    #[serde(default)]
    faber_version: Option<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestRecipe {
    xcode_source: String,
    xcode_file: String,
    runtimes: Vec<String>,
    brew_extra: Vec<String>,
    #[serde(default)]
    homebrew_tag: Option<String>,
    #[serde(default)]
    homebrew_sha256: Option<String>,
}

/// Parse an image's manifest. `bytes` is all of the file; over [`MAX_OPTIONAL_BYTES`] is
/// refused.
pub fn parse_from_manifest(bytes: &[u8]) -> Result<Prefill, String> {
    if bytes.len() as u64 > MAX_OPTIONAL_BYTES {
        return Err(format!(
            "manifest.json is over {MAX_OPTIONAL_BYTES} bytes: not a manifest faber wrote"
        ));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| "manifest.json is not UTF-8".to_string())?;
    let m: ManifestIn = serde_json::from_str(text).map_err(|e| {
        format!(
            "manifest.json does not match schema 1: {}",
            crate::term::sanitize_bounded(&e.to_string(), 200)
        )
    })?;
    if m.schema != 1 {
        return Err(format!(
            "manifest.json has schema {}; this faber reads schema 1",
            m.schema
        ));
    }
    let r = m.recipe;
    let _ = (&r.homebrew_tag, &r.homebrew_sha256);
    let kind = match r.xcode_source.as_str() {
        "xip" => XcodeKind::Xip,
        "app" => XcodeKind::App,
        _ => return Err("manifest.json: xcode_source is neither xip nor app".into()),
    };
    let file_ok = !r.xcode_file.is_empty()
        && r.xcode_file.len() <= 100
        && r.xcode_file
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'));
    if !file_ok {
        return Err("manifest.json: xcode_file is not a plain file name".into());
    }
    // The build's own validation, so nothing hostile reaches a flag or a command line.
    let checked = Recipe::new(kind, &r.runtimes, &r.brew_extra)
        .map_err(|e| format!("manifest.json: {}", crate::term::sanitize_bounded(&e, 200)))?;
    Ok(Prefill {
        runtimes: r.runtimes,
        brew: checked.brew_extra,
    })
}

// ---------------------------------------------------------------------------
// The recipe, resolved
// ---------------------------------------------------------------------------

/// Everything the printed build command and `recipe.json` carry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRecipe {
    pub name: String,
    pub xcode: PathBuf,
    pub kind: XcodeKind,
    pub runtimes: Vec<RuntimeSpec>,
    pub brew: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecipeFile {
    schema: u32,
    name: String,
    xcode: String,
    xcode_source: String,
    runtimes: Vec<String>,
    brew_extra: Vec<String>,
}

impl ResolvedRecipe {
    /// Validate the parts through the build's own rules.
    pub fn new(
        name: &str,
        xcode: &Path,
        kind: XcodeKind,
        runtimes: &[String],
        brew: &[String],
    ) -> Result<Self, String> {
        if let Some(why) = golden_name_problem(name) {
            return Err(why);
        }
        if !xcode.is_absolute() {
            return Err("the Xcode path is not absolute".into());
        }
        if let Some(why) = path_text_problem("the Xcode path", xcode) {
            return Err(why);
        }
        let r = Recipe::new(kind, runtimes, brew)?;
        Ok(Self {
            name: name.to_string(),
            xcode: xcode.to_path_buf(),
            kind,
            runtimes: r.runtimes,
            brew: r.brew_extra,
        })
    }

    /// `recipe.json`'s text.
    pub fn to_json(&self) -> String {
        let f = RecipeFile {
            schema: 1,
            name: self.name.clone(),
            xcode: self.xcode.to_string_lossy().into_owned(),
            xcode_source: self.kind.as_str().to_string(),
            runtimes: self.runtimes.iter().map(RuntimeSpec::token).collect(),
            brew_extra: self.brew.clone(),
        };
        let mut s = serde_json::to_string_pretty(&f).unwrap_or_default();
        s.push('\n');
        s
    }

    /// Read `recipe.json` back, strictly.
    pub fn from_json(text: &str) -> Result<Self, String> {
        if text.len() as u64 > MAX_RECIPE_BYTES {
            return Err("recipe.json is unreasonably large".into());
        }
        let f: RecipeFile = serde_json::from_str(text)
            .map_err(|_| "recipe.json is not a recipe this faber wrote".to_string())?;
        if f.schema != 1 {
            return Err(format!("recipe.json has schema {}", f.schema));
        }
        let kind = match f.xcode_source.as_str() {
            "xip" => XcodeKind::Xip,
            "app" => XcodeKind::App,
            _ => return Err("recipe.json: xcode_source is neither xip nor app".into()),
        };
        let path = PathBuf::from(&f.xcode);
        if XcodeKind::of_path(&path) != Some(kind) {
            return Err("recipe.json: the Xcode path does not match its source".into());
        }
        Self::new(&f.name, &path, kind, &f.runtimes, &f.brew_extra)
    }

    /// The exact next command, quoted for a POSIX shell.
    pub fn build_command(&self, bundle: &Path) -> String {
        let mut words: Vec<String> = vec![
            "sudo".into(),
            "faber".into(),
            "golden".into(),
            "build".into(),
            shell_quote(&bundle.to_string_lossy()),
            "--name".into(),
            shell_quote(&self.name),
            "--xcode".into(),
            shell_quote(&self.xcode.to_string_lossy()),
        ];
        for r in &self.runtimes {
            words.push("--runtime".into());
            words.push(shell_quote(&r.token()));
        }
        for f in &self.brew {
            words.push("--brew".into());
            words.push(shell_quote(f));
        }
        words.join(" ")
    }
}

/// One word for a POSIX shell: unquoted when it holds only safe characters, else in single
/// quotes with each `'` closed, escaped and reopened.
pub fn shell_quote(s: &str) -> String {
    let safe = !s.is_empty()
        && s.bytes().all(|c| {
            c.is_ascii_alphanumeric()
                || matches!(
                    c,
                    b'_' | b'@' | b'%' | b'+' | b'=' | b':' | b',' | b'.' | b'/' | b'-'
                )
        });
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// `7.8 GiB`.
pub fn gib(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / (1u64 << 30) as f64)
}

/// The recipe, as shown before anything starts.
pub fn render_recipe(
    r: &ResolvedRecipe,
    xcode: &XcodeIdentity,
    plan: &RuntimePlan,
    verified: &str,
) -> Vec<String> {
    let mut l = vec![
        format!("The golden image {}:", r.name),
        format!(
            "  Xcode      {}, {} ({verified})",
            xcode.shown(),
            r.xcode.display()
        ),
        "  Runtimes".to_string(),
    ];
    for c in &plan.choices {
        let size = c.size_bytes.map_or("size not known".to_string(), gib);
        l.push(format!("    {:<14} {:<15} {}", c.spec.token(), size, c.why));
    }
    let (known, unknown) = plan.total();
    l.push(format!(
        "    total          {}{}",
        if unknown {
            format!("at least {}", gib(known))
        } else {
            gib(known)
        },
        if unknown {
            ", and the sizes not known"
        } else {
            ""
        }
    ));
    for n in &plan.notes {
        l.push(format!("    note: {n}"));
    }
    l.push(format!(
        "  Homebrew   jq gh zstd git-lfs{}",
        if r.brew.is_empty() {
            String::new()
        } else {
            format!(" + {}", r.brew.join(" "))
        }
    ));
    l.push(format!(
        "  Disk       about {} at peak, and {} stays free for CI (ADR-0024)",
        gib(PEAK_BYTES),
        gib(FLOOR_BYTES)
    ));
    l
}

// ---------------------------------------------------------------------------
// Disk
// ---------------------------------------------------------------------------

/// A large step, for the space it needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskStep {
    /// Before anything: the whole build's peak.
    Start,
    /// Fetching the restore image, with the install that follows it.
    Download,
    /// The macOS install, the restore image already on disk.
    Install,
    /// The offline window's boot.
    Boot,
}

/// Free bytes the step needs, ADR-0024's floor included.
pub fn disk_needed(step: DiskStep, ipsw_bytes: u64) -> u64 {
    FLOOR_BYTES
        + match step {
            DiskStep::Start => PEAK_BYTES,
            DiskStep::Download => ipsw_bytes + INSTALL_BYTES,
            DiskStep::Install => INSTALL_BYTES,
            DiskStep::Boot => BOOT_BYTES,
        }
}

fn step_name(step: DiskStep) -> &'static str {
    match step {
        DiskStep::Start => "the build",
        DiskStep::Download => "the download and the install",
        DiskStep::Install => "the install",
        DiskStep::Boot => "the boot",
    }
}

/// Why `free` is not enough for `step`, or `None`.
pub fn free_problem(step: DiskStep, free: u64, ipsw_bytes: u64, volume: &Path) -> Option<String> {
    let need = disk_needed(step, ipsw_bytes);
    (free < need).then(|| {
        format!(
            "the volume holding {} has {} free; {} needs about {}, which includes the {} \
             that stays free for CI (ADR-0024). Free some space and run this again",
            volume.display(),
            gib(free),
            step_name(step),
            gib(need),
            gib(FLOOR_BYTES)
        )
    })
}

/// Why the bundle cannot live where it was asked: the import clones it into the image
/// store, and a clone is free only on one volume.
pub fn volume_problem(
    bundle_dev: u64,
    store_dev: u64,
    parent: &Path,
    store: &Path,
) -> Option<String> {
    (bundle_dev != store_dev).then(|| {
        format!(
            "{} is not on the same volume as the image store {}: the import's clone is free \
             only on one volume. Choose a bundle directory on the store's volume",
            parent.display(),
            store.display()
        )
    })
}

// ---------------------------------------------------------------------------
// The restore image
// ---------------------------------------------------------------------------

/// A restore image as `faber-golden-prep` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restore {
    pub url: Option<String>,
    pub build: String,
    pub os_version: String,
}

impl Restore {
    pub fn macos_major(&self) -> Option<u32> {
        self.os_version.split('.').next()?.parse().ok()
    }
}

/// Why `url` is not one to fetch a restore image from: https, a `.ipsw`, on Apple's CDN.
pub fn restore_url_problem(url: &str) -> Option<String> {
    let bad = |m: &str| {
        Some(format!(
            "the catalog's restore image {m}: {}",
            crate::term::sanitize_bounded(url, 200)
        ))
    };
    let Some(rest) = url.strip_prefix("https://") else {
        return bad("is not an https URL");
    };
    let host = rest.split('/').next().unwrap_or_default();
    if !url.ends_with(".ipsw") {
        return bad("is not an .ipsw");
    }
    if host.is_empty()
        || !host
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'-'))
    {
        return bad("has an unexpected host");
    }
    if !(host.ends_with(".apple.com") || host.ends_with(".cdn-apple.com")) {
        return bad("is not on Apple's servers");
    }
    None
}

/// A build string: letters and digits.
pub fn build_text_ok(b: &str) -> bool {
    (1..=16).contains(&b.len()) && b.bytes().all(|c| c.is_ascii_alphanumeric())
}

/// The value of `key=` in the tool's `key=value` lines.
pub fn kv<'a>(out: &'a str, key: &str) -> Option<&'a str> {
    out.lines()
        .find_map(|l| l.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}

/// `faber-golden-prep latest` / `verify` output, checked.
pub fn parse_restore(out: &str, with_url: bool) -> Result<Restore, String> {
    let build = kv(out, "build").ok_or("the tool printed no build")?;
    let os = kv(out, "os_version").ok_or("the tool printed no os_version")?;
    if !build_text_ok(build) {
        return Err(format!(
            "the restore image's build is unusable: {}",
            crate::term::sanitize_bounded(build, 40)
        ));
    }
    if !version_text_ok(os) {
        return Err(format!(
            "the restore image's version is unusable: {}",
            crate::term::sanitize_bounded(os, 40)
        ));
    }
    let url = if with_url {
        let u = kv(out, "url").ok_or("the tool printed no url")?;
        if let Some(why) = restore_url_problem(u) {
            return Err(why);
        }
        Some(u.to_string())
    } else {
        None
    };
    Ok(Restore {
        url,
        build: build.to_string(),
        os_version: os.to_string(),
    })
}

/// The last `Content-Length` in a header dump, if any.
pub fn content_length(headers: &str) -> Option<u64> {
    headers
        .lines()
        .filter_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse().ok())?
        })
        .next_back()
}

// ---------------------------------------------------------------------------
// Resuming
// ---------------------------------------------------------------------------

/// The last phase that finished, as the sidecar records it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Installed,
    Booted,
}

impl Phase {
    pub fn to_json(self) -> String {
        let p = match self {
            Phase::Installed => "installed",
            Phase::Booted => "booted",
        };
        format!("{{\"schema\":1,\"phase\":\"{p}\"}}\n")
    }

    pub fn from_json(text: &str) -> Result<Self, String> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct P {
            schema: u32,
            phase: String,
        }
        let p: P =
            serde_json::from_str(text).map_err(|_| "the state file is unreadable".to_string())?;
        match (p.schema, p.phase.as_str()) {
            (1, "installed") => Ok(Phase::Installed),
            (1, "booted") => Ok(Phase::Booted),
            _ => Err("the state file names a phase this faber does not know".into()),
        }
    }
}

/// What the bundle's directory and its sidecar hold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BundleFacts {
    pub exists: bool,
    pub entries: Vec<String>,
    pub phase: Option<Phase>,
    /// Set by the host from [`bundle_trust_problem`]: the bundle is not safe to boot.
    pub trust_problem: Option<String>,
}

/// One filesystem entry as `lstat` reports it (a symlink is not followed).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryFacts {
    pub name: String,
    pub uid: u32,
    /// The full `st_mode`, file type bits included.
    pub mode: u32,
}

const S_IFMT: u32 = 0o170_000;

impl EntryFacts {
    fn kind(&self) -> u32 {
        self.mode & S_IFMT
    }
}

/// Why the bundle, what is in it and its sidecar cannot be trusted to be this operator's,
/// or `None`. `prepare` boots what it finds, so another account that can write the parent
/// directory (a shared folder) must not be able to plant a bundle there: the parent may not
/// be writable by group or others (a sticky bit does not help, since the bundle's own owner
/// is what is judged next), and the bundle, each file in it and the sidecar must be the
/// operator's own, and none a symlink.
pub fn bundle_trust_problem(
    euid: u32,
    parent: &EntryFacts,
    bundle: Option<&EntryFacts>,
    inside: &[EntryFacts],
    sidecar: Option<&EntryFacts>,
) -> Option<String> {
    if parent.mode & 0o022 != 0 {
        return Some(format!(
            "{} is writable by its group or by everyone (mode {:o}), so another account could \
             plant a bundle in it. Choose a directory only you can write",
            parent.name,
            parent.mode & 0o7777
        ));
    }
    if parent.uid != 0 && parent.uid != euid {
        return Some(format!(
            "{} is owned by uid {}, neither you nor root",
            parent.name, parent.uid
        ));
    }
    let own = |e: &EntryFacts, want: u32, what: &str| -> Option<String> {
        if e.kind() == 0o120_000 {
            return Some(format!("{} is a symlink", e.name.escape_debug()));
        }
        if e.kind() != want {
            return Some(format!("{} is not {what}", e.name.escape_debug()));
        }
        (e.uid != euid).then(|| {
            format!(
                "{} is not owned by you (uid {})",
                e.name.escape_debug(),
                e.uid
            )
        })
    };
    if let Some(b) = bundle {
        if let Some(p) = own(b, 0o040_000, "a directory") {
            return Some(p);
        }
        if b.mode & 0o022 != 0 {
            return Some(format!(
                "{} is writable by its group or by everyone (mode {:o})",
                b.name.escape_debug(),
                b.mode & 0o7777
            ));
        }
    }
    for e in inside {
        if let Some(p) = own(e, 0o100_000, "a regular file") {
            return Some(p);
        }
    }
    sidecar.and_then(|s| own(s, 0o100_000, "a regular file"))
}

/// Where a run starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resume {
    /// Nothing is there: download, install, boot.
    Fresh,
    /// The install finished: boot.
    Installed,
    /// The boot finished: print the command.
    Booted,
}

/// Where to start, from what is on disk, or why not to.
pub fn resume_state(bundle: &Path, f: &BundleFacts) -> Result<Resume, String> {
    use super::vm_images::GOLDEN_FILES;
    if let Some(why) = &f.trust_problem {
        return Err(format!(
            "{}: {why}. Nothing here will be booted",
            bundle.display()
        ));
    }
    if !f.exists || f.entries.is_empty() && f.phase.is_none() {
        return Ok(Resume::Fresh);
    }
    let Some(phase) = f.phase else {
        return Err(format!(
            "{} exists, but this command did not finish installing macOS into it (or it is \
             not a bundle this command made). Booting a bundle of unknown state could run an \
             armed provisioner. Remove it and run this again",
            bundle.display()
        ));
    };
    let missing: Vec<&str> = GOLDEN_FILES
        .iter()
        .copied()
        .filter(|g| !f.entries.iter().any(|e| e == g))
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "{} is damaged: it is missing {}. Remove it and run this again",
            bundle.display(),
            missing.join(", ")
        ));
    }
    Ok(match phase {
        Phase::Installed => Resume::Installed,
        Phase::Booted => Resume::Booted,
    })
}

// ---------------------------------------------------------------------------
// Finding faber-golden-prep
// ---------------------------------------------------------------------------

/// The tool's installed name.
pub const PREP_NAME: &str = "faber-golden-prep";

/// Where to look for `faber-golden-prep`, most expected first: beside the root-owned CLI
/// copy (`/usr/local/bin`), then beside the running binary.
pub fn prep_candidates(cli_bin: &Path, running: Option<&Path>) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in [cli_bin.parent(), running.and_then(Path::parent)]
        .into_iter()
        .flatten()
    {
        let p = dir.join(PREP_NAME);
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Why the tool at the end of `chain` (the file, then each directory up to `/`) is not
/// fit to run as the operator with the virtualization entitlement: every entry must be
/// root's and writable by nobody else, and the file executable by the operator. Whoever
/// could write one of them could choose what the operator runs.
pub fn prep_trust_problem(chain: &[super::exe::PathFacts]) -> Option<String> {
    let Some(file) = chain.first() else {
        return Some("it could not be examined".into());
    };
    for f in chain {
        let shown = f.path.display();
        if f.uid != 0 {
            return Some(format!("{shown} is owned by uid {}, not root", f.uid));
        }
        if f.mode & 0o022 != 0 {
            return Some(format!(
                "{shown} is writable by its group or by everyone (mode {:o})",
                f.mode & 0o7777
            ));
        }
    }
    (file.mode & 0o170_000 != 0o100_000 || file.mode & 0o001 == 0).then(|| {
        format!(
            "{} is not a regular file the operator can execute (mode {:o})",
            file.path.display(),
            file.mode & 0o7777
        )
    })
}

/// Whether the tool's `--version` is this binary's stamp: `faber-golden-prep <version>`.
pub fn prep_version_problem(stdout: &str, version: &str) -> Option<String> {
    let want = format!("{PREP_NAME} {version}");
    (stdout.trim() != want).then(|| {
        format!(
            "it reports {:?}, not {want:?}: install the tool from the same release as this \
             binary",
            super::tools::sanitize_child_output(stdout.trim(), 200)
        )
    })
}

/// The first candidate that exists and is trusted, or why none is, with what to do.
/// `facts` examines a path (the file, then its directories) and fails when it is absent.
pub fn locate_prep<F>(candidates: &[PathBuf], facts: F) -> Result<PathBuf, String>
where
    F: Fn(&Path) -> std::io::Result<Vec<super::exe::PathFacts>>,
{
    let mut seen: Vec<String> = Vec::new();
    for c in candidates {
        match facts(c) {
            Ok(chain) => match prep_trust_problem(&chain) {
                // The path that was examined, symlinks resolved: exec this one, so a link
                // swapped after the check cannot redirect the run.
                None => return Ok(chain[0].path.clone()),
                Some(why) => seen.push(format!("{}: {why}", c.display())),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                seen.push(format!("{}: not there", c.display()));
            }
            Err(e) => seen.push(format!("{}: {e}", c.display())),
        }
    }
    Err(format!(
        "cannot find a usable {PREP_NAME}. Looked at:\n  {}\nIt ships in each release as \
         {PREP_NAME}-<target>. Install it root-owned beside the CLI:\n  sudo install -m 0755 -o \
         root -g wheel {PREP_NAME}-aarch64-apple-darwin /usr/local/bin/{PREP_NAME}",
        seen.join("\n  ")
    ))
}

// ---------------------------------------------------------------------------
// The orchestration
// ---------------------------------------------------------------------------

/// How a boot ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootEnd {
    /// The guest shut itself down.
    ShutDown,
    /// The guest was forced off.
    Forced,
}

/// Every effect of a prepare. The real one ([`mac::MacHost`]) is macOS only; tests
/// substitute a fake.
pub trait PrepareHost {
    fn is_root(&self) -> bool;
    /// Whether stdin is a terminal.
    fn interactive(&self) -> bool;
    /// Whether this process is in a desktop session, where a window can open.
    fn has_desktop_session(&self) -> bool;
    fn say(&mut self, line: &str);
    fn confirm(&mut self, question: &str) -> Result<bool, String>;

    fn bundle_facts(&self, bundle: &Path) -> Result<BundleFacts, String>;
    /// The device of `path`, or of its nearest existing ancestor.
    fn device_of(&self, path: &Path) -> Result<u64, String>;
    /// The image store's device (its nearest existing ancestor's, before setup).
    fn store_device(&self) -> Result<u64, String>;
    fn store_path(&self) -> PathBuf;
    /// Free bytes on `path`'s volume (its nearest existing ancestor's).
    fn free_bytes(&self, path: &Path) -> Result<u64, String>;
    /// Make `parent`, mode 0700, if absent. Its own parent must exist.
    fn make_parent(&mut self, parent: &Path) -> Result<(), String>;
    fn write_phase(&mut self, bundle: &Path, phase: Phase) -> Result<(), String>;
    fn write_recipe(&mut self, bundle: &Path, json: &str) -> Result<(), String>;
    fn read_recipe(&self, bundle: &Path) -> Result<Option<String>, String>;
    fn remove_bundle(&mut self, bundle: &Path);

    /// This host's /Applications/Xcode.app, if there is one.
    fn host_xcode(&self) -> Option<(PathBuf, XcodeIdentity)>;
    fn xcode_identity(&self, path: &Path, kind: XcodeKind) -> Result<XcodeIdentity, String>;
    /// Hold the Xcode to Apple's signature; the message names what was checked.
    fn verify_xcode(&mut self, path: &Path, kind: XcodeKind) -> Result<String, String>;
    fn host_runtimes(&self) -> Result<Vec<HostRuntime>, String>;
    fn open_url(&mut self, url: &str) -> Result<(), String>;
    /// Wait for a new, finished `Xcode_*.xip` in ~/Downloads.
    fn wait_for_xip(&mut self) -> Result<PathBuf, String>;
    fn read_from(&self, name: &str) -> Result<Prefill, String>;

    fn catalog_latest(&mut self) -> Result<Restore, String>;
    fn verify_ipsw(&mut self, ipsw: &Path) -> Result<Restore, String>;
    fn remote_size(&mut self, url: &str) -> Result<u64, String>;
    fn file_len(&self, path: &Path) -> Result<u64, String>;
    fn download(&mut self, url: &str, dest: &Path) -> Result<(), String>;
    fn sha256(&mut self, path: &Path) -> Result<String, String>;
    fn remove_file(&mut self, path: &Path);
    fn install(&mut self, bundle: &Path, ipsw: &Path, disk_gib: u32) -> Result<(), String>;
    fn boot(&mut self, bundle: &Path, files: &[(&str, &str)]) -> Result<BootEnd, String>;
}

/// Why a prepare stopped.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PrepareError {
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Host(String),
    #[error(
        "no Xcode was given and this host has none. Download Xcode from Apple (sign in there; \
         faber never sees the Apple ID):\n  {0}\nthen run this again with --xcode <the downloaded \
         Xcode_*.xip>"
    )]
    NeedsXcode(String),
    #[error("not confirmed; nothing was started")]
    Declined,
}

fn host_err(e: String) -> PrepareError {
    PrepareError::Host(e)
}

/// What a finished prepare reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prepared {
    pub recipe: ResolvedRecipe,
    pub command: String,
}

/// The lines the operator follows in the window.
pub fn checklist(bundle_name: &str) -> Vec<String> {
    vec![
        format!("A window with the guest {bundle_name} opens now, with no network device."),
        "In it, do this:".into(),
        "  1. Setup Assistant, offline: at the network screen choose \"My computer does not \
         connect to the internet\"."
            .into(),
        "  2. Create the admin account with any name except \"runner\" and a throwaway \
         password. Sign in with no Apple Account. Leave FileVault off."
            .into(),
        "  3. Open Terminal in the guest and run: sudo sh /Volumes/FABERSETUP/arm.sh".into(),
        "The guest then powers itself off and this command goes on. Closing the window \
         asks the guest to shut down."
            .into(),
    ]
}

/// Run `faber golden prepare` against `host`.
pub fn prepare<H: PrepareHost>(
    host: &mut H,
    req: &PrepareRequest,
) -> Result<Prepared, PrepareError> {
    if host.is_root() {
        return Err(PrepareError::Refused(
            "run this as yourself, not as root or with sudo: it opens a window on your desktop \
             and needs no privilege"
                .into(),
        ));
    }
    let facts = host.bundle_facts(&req.bundle).map_err(host_err)?;
    let resume = resume_state(&req.bundle, &facts).map_err(PrepareError::Refused)?;

    // Where: the same volume as the store, on every run, resumed ones included.
    let bundle_dev = host.device_of(&req.parent).map_err(host_err)?;
    let store_dev = host.store_device().map_err(host_err)?;
    if let Some(why) = volume_problem(bundle_dev, store_dev, &req.parent, &host.store_path()) {
        return Err(PrepareError::Refused(why));
    }

    if resume == Resume::Booted {
        host.say(&format!(
            "{} has finished its boot; nothing is left but the build.",
            req.bundle.display()
        ));
        let recipe = match host.read_recipe(&req.bundle).map_err(host_err)? {
            Some(text) => {
                let recorded = ResolvedRecipe::from_json(&text).map_err(|e| {
                    PrepareError::Refused(format!(
                        "{}/{RECIPE_FILE}: {e}. Remove that file and run this again to write it anew",
                        req.bundle.display()
                    ))
                })?;
                if req.has_recipe_flags() {
                    host.say(
                        "note: the recipe flags you gave are ignored. This bundle finished its \
                         boot, so the recipe recorded in it stands; remove recipe.json to write \
                         another.",
                    );
                }
                recorded
            }
            None => {
                let mut cache = None;
                let recipe = resolve_recipe(host, req, resume, &mut cache)?.recipe;
                host.write_recipe(&req.bundle, &recipe.to_json())
                    .map_err(host_err)?;
                recipe
            }
        };
        return Ok(finish(host, req, recipe));
    }

    let free = host.free_bytes(&req.parent).map_err(host_err)?;
    let first = if resume == Resume::Fresh {
        DiskStep::Start
    } else {
        DiskStep::Boot
    };
    if let Some(why) = free_problem(first, free, 0, &req.parent) {
        return Err(PrepareError::Refused(why));
    }
    // A window needs a desktop; learn it before anything is downloaded.
    if !host.has_desktop_session() {
        return Err(PrepareError::Refused(
            "this is not a desktop session, and the offline window needs one. Run it at the \
             Mac or over Screen Sharing; an SSH-only session is not supported. Nothing was \
             downloaded"
                .into(),
        ));
    }

    let mut cache: Option<Restore> = None;
    let Resolved {
        recipe,
        xcode,
        plan,
        verified,
    } = resolve_recipe(host, req, resume, &mut cache)?;
    for line in render_recipe(&recipe, &xcode, &plan, &verified) {
        host.say(&line);
    }
    if !req.yes {
        if !host.interactive() {
            return Err(PrepareError::Refused(
                "no terminal to confirm the recipe on: re-run with --yes to accept it".into(),
            ));
        }
        if !host.confirm("Build this recipe?").map_err(host_err)? {
            return Err(PrepareError::Declined);
        }
    }

    if resume == Resume::Fresh {
        install_phase(host, req, &mut cache)?;
    } else {
        host.say("macOS is already installed in the bundle; going on to the boot.");
        if let Some(text) = host.read_recipe(&req.bundle).map_err(host_err)? {
            match ResolvedRecipe::from_json(&text) {
                Ok(old) if old == recipe => {}
                Ok(old) => {
                    host.say("note: this recipe differs from the one recorded when the install finished:");
                    host.say(&format!("  recorded: {}", old.build_command(&req.bundle)));
                    host.say(&format!(
                        "  now:      {}",
                        recipe.build_command(&req.bundle)
                    ));
                    host.say("The install does not depend on the recipe, so it goes on with the new one.");
                }
                Err(_) => {
                    host.say("note: the recipe recorded in the bundle is unreadable; replacing it.")
                }
            }
        }
    }
    // Recorded as soon as the install is, so a later run can tell what changed.
    host.write_recipe(&req.bundle, &recipe.to_json())
        .map_err(host_err)?;
    boot_phase(host, req)?;
    host.write_recipe(&req.bundle, &recipe.to_json())
        .map_err(host_err)?;
    host.write_phase(&req.bundle, Phase::Booted)
        .map_err(host_err)?;
    Ok(finish(host, req, recipe))
}

fn finish<H: PrepareHost>(host: &mut H, req: &PrepareRequest, recipe: ResolvedRecipe) -> Prepared {
    let command = recipe.build_command(&req.bundle);
    host.say("");
    host.say("The bundle is ready. Now build the image, from your own account:");
    host.say(&format!("  {command}"));
    Prepared { recipe, command }
}

fn runtime_plan_for<H: PrepareHost>(
    host: &mut H,
    req: &PrepareRequest,
    xcode: &XcodeIdentity,
) -> Result<RuntimePlan, PrepareError> {
    let prefill = match &req.from {
        Some(n) => Some((n.clone(), host.read_from(n).map_err(PrepareError::Refused)?)),
        None => None,
    };
    let host_xcode = host.host_xcode().map(|(_, i)| i);
    let host_runtimes = if req.runtimes.is_empty() && !req.no_host_runtimes && prefill.is_none() {
        match host.host_runtimes() {
            Ok(r) => r,
            Err(e) => {
                host.say(&format!("note: could not read this host's runtimes ({e})"));
                Vec::new()
            }
        }
    } else {
        Vec::new()
    };
    let source = if !req.runtimes.is_empty() {
        RuntimeSource::Explicit(&req.runtimes)
    } else if let Some((name, p)) = prefill.as_ref().filter(|(_, p)| !p.runtimes.is_empty()) {
        RuntimeSource::From(&p.runtimes, name)
    } else {
        RuntimeSource::Inferred {
            no_host_runtimes: req.no_host_runtimes,
        }
    };
    plan_runtimes(source, xcode, host_xcode.as_ref(), &host_runtimes).map_err(PrepareError::Refused)
}

fn restore_for<H: PrepareHost>(
    host: &mut H,
    req: &PrepareRequest,
    cache: &mut Option<Restore>,
) -> Result<Restore, PrepareError> {
    if let Some(r) = cache {
        return Ok(r.clone());
    }
    let r = match &req.ipsw {
        Some(p) => host.verify_ipsw(p),
        None => host.catalog_latest(),
    }
    .map_err(host_err)?;
    *cache = Some(r.clone());
    Ok(r)
}

/// The recipe, with what was learned settling it.
struct Resolved {
    recipe: ResolvedRecipe,
    xcode: XcodeIdentity,
    plan: RuntimePlan,
    verified: String,
}

/// Choose and check the Xcode, then settle the recipe around it.
fn resolve_recipe<H: PrepareHost>(
    host: &mut H,
    req: &PrepareRequest,
    resume: Resume,
    cache: &mut Option<Restore>,
) -> Result<Resolved, PrepareError> {
    // Which Xcode.
    let (path, kind) = match &req.xcode {
        Some((p, k)) => (p.clone(), *k),
        None => match host.host_xcode() {
            Some((p, id)) => {
                host.say(&format!(
                    "This host has Xcode {} at {}.",
                    id.shown(),
                    p.display()
                ));
                let use_it = req.yes
                    || (host.interactive()
                        && host
                            .confirm("Install this Xcode in the image?")
                            .map_err(host_err)?);
                if use_it {
                    (p, XcodeKind::App)
                } else {
                    fetch_xip(host, req, resume, cache)?
                }
            }
            None => fetch_xip(host, req, resume, cache)?,
        },
    };
    let xcode = host
        .xcode_identity(&path, kind)
        .map_err(PrepareError::Refused)?;
    host.say(&format!(
        "Checking {} against Apple's signature (a few minutes)...",
        path.display()
    ));
    let verified = host
        .verify_xcode(&path, kind)
        .map_err(PrepareError::Refused)?;

    let prefill = match &req.from {
        Some(n) => Some(host.read_from(n).map_err(PrepareError::Refused)?),
        None => None,
    };
    let plan = runtime_plan_for(host, req, &xcode)?;
    let brew: Vec<String> = if !req.brew.is_empty() {
        req.brew.clone()
    } else {
        prefill.map(|p| p.brew).unwrap_or_default()
    };
    let recipe = ResolvedRecipe::new(
        &default_image_name(&xcode),
        &path,
        kind,
        &plan.tokens(),
        &brew,
    )
    .map_err(PrepareError::Refused)?;
    Ok(Resolved {
        recipe,
        xcode,
        plan,
        verified,
    })
}

/// No Xcode on hand: open Apple's page and wait for the file, or stop with the URL.
fn fetch_xip<H: PrepareHost>(
    host: &mut H,
    req: &PrepareRequest,
    resume: Resume,
    cache: &mut Option<Restore>,
) -> Result<(PathBuf, XcodeKind), PrepareError> {
    let major = if resume == Resume::Fresh {
        restore_for(host, req, cache)
            .ok()
            .and_then(|r| r.macos_major())
    } else {
        None
    };
    let url = xcode_download_url(major);
    if !host.interactive() {
        return Err(PrepareError::NeedsXcode(url));
    }
    host.say(&format!("Opening Apple's download page: {url}"));
    host.open_url(&url).map_err(host_err)?;
    host.say("Download the Xcode .xip there. Waiting for a new Xcode_*.xip in ~/Downloads...");
    let xip = host.wait_for_xip().map_err(host_err)?;
    Ok((xip, XcodeKind::Xip))
}

fn install_phase<H: PrepareHost>(
    host: &mut H,
    req: &PrepareRequest,
    cache: &mut Option<Restore>,
) -> Result<(), PrepareError> {
    host.make_parent(&req.parent).map_err(host_err)?;
    let (ipsw, size, downloaded, catalog_build) = match &req.ipsw {
        Some(p) => {
            let size = host.file_len(p).map_err(host_err)?;
            (p.clone(), size, false, None)
        }
        None => {
            let r = restore_for(host, req, cache)?;
            let url = r
                .url
                .clone()
                .ok_or_else(|| host_err("no restore image URL".into()))?;
            host.say(&format!(
                "newest restore image this host supports: macOS {} ({})",
                r.os_version, r.build
            ));
            let size = host.remote_size(&url).map_err(host_err)?;
            let ipsw = req.parent.join(format!(".restore-{}.ipsw", r.build));
            let free = host.free_bytes(&req.parent).map_err(host_err)?;
            if let Some(why) = free_problem(DiskStep::Download, free, size, &req.parent) {
                return Err(PrepareError::Refused(why));
            }
            host.say(&format!("downloading {} to {}", gib(size), ipsw.display()));
            host.download(&url, &ipsw).map_err(|e| {
                PrepareError::Host(format!("{e}; run again to resume the download"))
            })?;
            (ipsw, size, true, Some(r.build))
        }
    };
    let digest = host.sha256(&ipsw).map_err(host_err)?;
    host.say(&format!("restore image sha256={digest}"));
    if let Some(want) = &req.sha256 {
        if &digest != want {
            if downloaded {
                host.remove_file(&ipsw);
            }
            return Err(PrepareError::Refused(format!(
                "{} has sha256 {digest}, not the pinned {want}",
                ipsw.display()
            )));
        }
    }
    let verified = host.verify_ipsw(&ipsw).map_err(PrepareError::Refused)?;
    if let Some(b) = catalog_build {
        if verified.build != b {
            return Err(PrepareError::Refused(format!(
                "{} is build {}, not the catalog's {b}",
                ipsw.display(),
                verified.build
            )));
        }
    }
    host.say(&format!(
        "verified: macOS {} ({})",
        verified.os_version, verified.build
    ));
    let free = host.free_bytes(&req.parent).map_err(host_err)?;
    if let Some(why) = free_problem(DiskStep::Install, free, size, &req.parent) {
        return Err(PrepareError::Refused(why));
    }
    host.say("installing macOS into the bundle (about 15 minutes)");
    if let Err(e) = host.install(&req.bundle, &ipsw, req.disk_gib) {
        host.remove_bundle(&req.bundle);
        return Err(PrepareError::Host(format!(
            "the install failed ({e}); removed {}. The restore image is kept at {}: run again \
             with --ipsw {}",
            req.bundle.display(),
            ipsw.display(),
            ipsw.display()
        )));
    }
    host.write_phase(&req.bundle, Phase::Installed)
        .map_err(host_err)?;
    if downloaded && !req.keep_ipsw {
        host.remove_file(&ipsw);
        host.say("deleted the restore image");
    }
    Ok(())
}

fn boot_phase<H: PrepareHost>(host: &mut H, req: &PrepareRequest) -> Result<(), PrepareError> {
    let free = host.free_bytes(&req.parent).map_err(host_err)?;
    if let Some(why) = free_problem(DiskStep::Boot, free, 0, &req.parent) {
        return Err(PrepareError::Refused(why));
    }
    for line in checklist(&req.bundle_name) {
        host.say(&line);
    }
    match host.boot(&req.bundle, &setup_files()).map_err(host_err)? {
        BootEnd::ShutDown => {
            host.say("the guest shut down");
            Ok(())
        }
        BootEnd::Forced => Err(PrepareError::Refused(
            "the guest was forced off, not shut down from inside. Run this again to boot it; \
             do the checklist again if Setup Assistant was not finished"
                .into(),
        )),
    }
}

// ---------------------------------------------------------------------------
// The command
// ---------------------------------------------------------------------------

/// Run `faber golden prepare`.
pub async fn run(args: PrepareArgs) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        tokio::task::spawn_blocking(move || mac::run(args)).await?
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = args;
        anyhow::bail!("the golden macOS image is built on macOS only")
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::io::{BufRead, IsTerminal, Read};
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use anyhow::{anyhow, bail};

    use super::super::exe;
    use super::super::golden_build::xcode_codesign_args;
    use super::super::tools::{output_within, sanitize_child_output, SAFE_PATH};
    use super::super::vm_images::golden_dir;
    use super::*;
    use crate::platform::layout::CLI_BIN;

    const SHORT: Duration = Duration::from_secs(60);
    const LONG: Duration = Duration::from_secs(60 * 60);

    pub fn run(args: PrepareArgs) -> anyhow::Result<()> {
        let cwd = std::env::current_dir()?;
        let req = PrepareRequest::new(&args, &cwd).map_err(|e| anyhow!("{e}"))?;
        if crate::platform::record::current_euid() == 0 {
            bail!(
                "run this as yourself, not as root or with sudo: it opens a window on your desktop"
            );
        }
        let running = std::env::current_exe().ok();
        let candidates = prep_candidates(Path::new(CLI_BIN), running.as_deref());
        let prep = locate_prep(&candidates, exe::chain_of).map_err(|e| anyhow!("{e}"))?;
        let mut host = MacHost::new(prep)?;
        match prepare(&mut host, &req) {
            Ok(_) => Ok(()),
            Err(e) => Err(anyhow!("{}", crate::term::sanitize(&e.to_string()))),
        }
    }

    pub struct MacHost {
        prep: PathBuf,
        store: PathBuf,
    }

    impl MacHost {
        fn new(prep: PathBuf) -> anyhow::Result<Self> {
            let out = Self::run_prep(&prep, &["--version"], SHORT).map_err(|e| anyhow!("{e}"))?;
            if let Some(why) = prep_version_problem(&out, env!("FABER_VERSION")) {
                bail!("{}: {why}", prep.display());
            }
            Ok(Self {
                prep,
                store: golden_dir(),
            })
        }

        fn run_prep(prep: &Path, args: &[&str], limit: Duration) -> Result<String, String> {
            let bin = prep;
            assert!(bin.is_absolute());
            let mut cmd = Command::new(bin); // absolute: checked above
            cmd.args(args).stdin(Stdio::null());
            let out =
                output_within(cmd, limit).map_err(|e| format!("{}: {e:#}", prep.display()))?;
            if !out.success {
                return Err(format!(
                    "{} {} failed: {}",
                    prep.display(),
                    args.first().copied().unwrap_or(""),
                    out.sanitized(1000)
                ));
            }
            Ok(out.stdout)
        }

        fn tool(
            program: &'static str,
            args: &[&std::ffi::OsStr],
            limit: Duration,
        ) -> Result<super::super::tools::Output, String> {
            let bin = program;
            assert!(bin.starts_with('/'));
            let mut cmd = Command::new(bin); // absolute: checked above
            cmd.args(args).env("PATH", SAFE_PATH).stdin(Stdio::null());
            output_within(cmd, limit).map_err(|e| format!("{program}: {e:#}"))
        }

        fn ancestor(path: &Path) -> PathBuf {
            let mut at = path;
            while !at.exists() {
                match at.parent() {
                    Some(p) => at = p,
                    None => break,
                }
            }
            at.to_path_buf()
        }

        fn phase_path(bundle: &Path) -> PathBuf {
            let name = bundle
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("bundle");
            bundle
                .parent()
                .unwrap_or(Path::new("/"))
                .join(format!(".{name}.prepare"))
        }

        fn write_atomic(path: &Path, text: &str) -> Result<(), String> {
            let tmp = path.with_extension("tmp");
            let mut f = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .custom_flags(0x100) // O_NOFOLLOW on macOS
                .open(&tmp)
                .map_err(|e| format!("{}: {e}", tmp.display()))?;
            std::io::Write::write_all(&mut f, text.as_bytes()).map_err(|e| e.to_string())?;
            f.sync_all().map_err(|e| e.to_string())?;
            std::fs::rename(&tmp, path).map_err(|e| format!("{}: {e}", path.display()))
        }

        /// A bounded, regular, not-linked read of a file in a directory.
        fn read_small(path: &Path, cap: u64) -> Result<Option<String>, String> {
            let meta = match std::fs::symlink_metadata(path) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(format!("{}: {e}", path.display())),
            };
            if !meta.file_type().is_file() {
                return Err(format!("{} is not a regular file", path.display()));
            }
            if meta.len() > cap {
                return Err(format!("{} is over {cap} bytes", path.display()));
            }
            let mut text = String::new();
            std::fs::File::open(path)
                .and_then(|f| f.take(cap).read_to_string(&mut text))
                .map_err(|e| format!("{}: {e}", path.display()))?;
            Ok(Some(text))
        }
    }

    impl PrepareHost for MacHost {
        fn is_root(&self) -> bool {
            crate::platform::record::current_euid() == 0
        }

        fn interactive(&self) -> bool {
            std::io::stdin().is_terminal()
        }

        fn has_desktop_session(&self) -> bool {
            Self::tool("/bin/launchctl", &["managername".as_ref()], SHORT)
                .is_ok_and(|o| o.success && o.stdout.trim() == "Aqua")
        }

        fn say(&mut self, line: &str) {
            println!("{}", crate::term::sanitize(line));
        }

        fn confirm(&mut self, question: &str) -> Result<bool, String> {
            super::super::confirm(question).map_err(|e| e.to_string())
        }

        fn bundle_facts(&self, bundle: &Path) -> Result<BundleFacts, String> {
            let mut facts = BundleFacts::default();
            let facts_of = |name: String, m: &std::fs::Metadata| EntryFacts {
                name,
                uid: m.uid(),
                mode: m.mode(),
            };
            let parent_path = bundle.parent().unwrap_or(Path::new("/"));
            // The parent as it resolves: a path through /var or a user's own link is
            // fine; what matters is who can write the directory the bundle lives in.
            let parent = std::fs::metadata(parent_path)
                .ok()
                .map(|m| facts_of(parent_path.display().to_string(), &m));
            let mut bundle_entry = None;
            let mut inside = Vec::new();
            match std::fs::symlink_metadata(bundle) {
                Ok(m) if m.file_type().is_symlink() => {
                    return Err(format!(
                        "{} is a symlink; give the directory itself",
                        bundle.display()
                    ))
                }
                Ok(m) if !m.file_type().is_dir() => {
                    return Err(format!("{} is not a directory", bundle.display()))
                }
                Ok(m) => {
                    facts.exists = true;
                    bundle_entry = Some(facts_of(bundle.display().to_string(), &m));
                    for e in std::fs::read_dir(bundle).map_err(|e| e.to_string())? {
                        let e = e.map_err(|e| e.to_string())?;
                        let name = e.file_name().to_string_lossy().into_owned();
                        if let Ok(m) = std::fs::symlink_metadata(e.path()) {
                            inside.push(facts_of(name.clone(), &m));
                        }
                        facts.entries.push(name);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{}: {e}", bundle.display())),
            }
            let sidecar_path = Self::phase_path(bundle);
            let sidecar = std::fs::symlink_metadata(&sidecar_path)
                .ok()
                .map(|m| facts_of(sidecar_path.display().to_string(), &m));
            if let Some(p) = &parent {
                facts.trust_problem = bundle_trust_problem(
                    crate::platform::record::current_euid(),
                    p,
                    bundle_entry.as_ref(),
                    &inside,
                    sidecar.as_ref(),
                );
            }
            if facts.trust_problem.is_none() {
                if let Some(text) = Self::read_small(&sidecar_path, 1024)? {
                    facts.phase = Some(Phase::from_json(&text)?);
                }
            }
            Ok(facts)
        }

        fn device_of(&self, path: &Path) -> Result<u64, String> {
            let at = Self::ancestor(path);
            std::fs::metadata(&at)
                .map(|m| m.dev())
                .map_err(|e| format!("{}: {e}", at.display()))
        }

        fn store_device(&self) -> Result<u64, String> {
            self.device_of(&self.store)
        }

        fn store_path(&self) -> PathBuf {
            self.store.clone()
        }

        fn free_bytes(&self, path: &Path) -> Result<u64, String> {
            let at = Self::ancestor(path);
            let out = Self::tool("/bin/df", &["-k".as_ref(), at.as_os_str()], SHORT)?;
            super::super::golden_build::df_available_bytes(&out.stdout)
                .ok_or_else(|| "df printed no free space".to_string())
        }

        fn make_parent(&mut self, parent: &Path) -> Result<(), String> {
            if parent.exists() {
                return Ok(());
            }
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(parent)
                .map_err(|e| {
                    format!(
                        "cannot make {} (its own parent must exist): {e}",
                        parent.display()
                    )
                })
        }

        fn write_phase(&mut self, bundle: &Path, phase: Phase) -> Result<(), String> {
            Self::write_atomic(&Self::phase_path(bundle), &phase.to_json())
        }

        fn write_recipe(&mut self, bundle: &Path, json: &str) -> Result<(), String> {
            Self::write_atomic(&bundle.join(RECIPE_FILE), json)
        }

        fn read_recipe(&self, bundle: &Path) -> Result<Option<String>, String> {
            Self::read_small(&bundle.join(RECIPE_FILE), MAX_RECIPE_BYTES)
        }

        fn remove_bundle(&mut self, bundle: &Path) {
            let _ = std::fs::remove_dir_all(bundle);
            let _ = std::fs::remove_file(Self::phase_path(bundle));
        }

        fn host_xcode(&self) -> Option<(PathBuf, XcodeIdentity)> {
            let app = PathBuf::from("/Applications/Xcode.app");
            let id = self.xcode_identity(&app, XcodeKind::App).ok()?;
            Some((app, id))
        }

        fn xcode_identity(&self, path: &Path, kind: XcodeKind) -> Result<XcodeIdentity, String> {
            match kind {
                XcodeKind::Xip => path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(XcodeIdentity::from_xip_name)
                    .ok_or_else(|| {
                        format!(
                            "cannot tell the Xcode version from the name {}: Apple names it \
                             Xcode_<version>.xip",
                            path.display()
                        )
                    }),
                XcodeKind::App => {
                    let plist = path.join("Contents/version.plist");
                    let out = Self::tool(
                        "/usr/bin/plutil",
                        &[
                            "-convert".as_ref(),
                            "json".as_ref(),
                            "-o".as_ref(),
                            "-".as_ref(),
                            plist.as_os_str(),
                        ],
                        SHORT,
                    )?;
                    if !out.success {
                        return Err(format!(
                            "{} is not an Xcode.app: {}",
                            path.display(),
                            out.sanitized(200)
                        ));
                    }
                    XcodeIdentity::from_version_plist_json(&out.stdout)
                }
            }
        }

        fn verify_xcode(&mut self, path: &Path, kind: XcodeKind) -> Result<String, String> {
            let meta =
                std::fs::symlink_metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
            if meta.file_type().is_symlink() {
                return Err(format!(
                    "{} is a symlink; give the Xcode itself",
                    path.display()
                ));
            }
            match kind {
                XcodeKind::App => {
                    let args = xcode_codesign_args(path);
                    let refs: Vec<&std::ffi::OsStr> = args.iter().map(|a| a.as_os_str()).collect();
                    let out = Self::tool("/usr/bin/codesign", &refs, LONG)?;
                    if !out.success {
                        return Err(format!(
                            "{} does not meet Apple's signature requirement: {}",
                            path.display(),
                            out.sanitized(400)
                        ));
                    }
                    Ok("codesign: Apple's requirement met".into())
                }
                XcodeKind::Xip => {
                    let out = Self::tool(
                        "/usr/sbin/pkgutil",
                        &["--check-signature".as_ref(), path.as_os_str()],
                        LONG,
                    )?;
                    match xip_signature_problem(out.success, &out.stdout) {
                        None => Ok("pkgutil: signed Apple Software".into()),
                        Some(why) => Err(format!("{}: {why}", path.display())),
                    }
                }
            }
        }

        fn host_runtimes(&self) -> Result<Vec<HostRuntime>, String> {
            let out = Self::tool(
                "/usr/bin/xcrun",
                &[
                    "simctl".as_ref(),
                    "runtime".as_ref(),
                    "list".as_ref(),
                    "-j".as_ref(),
                ],
                Duration::from_secs(120),
            )?;
            if !out.success {
                return Err(sanitize_child_output(&out.text(), 200));
            }
            parse_simctl_runtimes(&out.stdout)
        }

        fn open_url(&mut self, url: &str) -> Result<(), String> {
            let out = Self::tool("/usr/bin/open", &[url.as_ref()], SHORT)?;
            out.success
                .then_some(())
                .ok_or_else(|| format!("open failed: {}", out.sanitized(200)))
        }

        fn wait_for_xip(&mut self) -> Result<PathBuf, String> {
            let downloads = std::env::var_os("HOME")
                .map(|h| PathBuf::from(h).join("Downloads"))
                .ok_or("HOME is not set")?;
            let list = |dir: &Path| -> Vec<(PathBuf, u64)> {
                std::fs::read_dir(dir)
                    .into_iter()
                    .flatten()
                    .flatten()
                    .filter(|e| e.file_name().to_str().is_some_and(is_xcode_xip_name))
                    .filter_map(|e| {
                        let m = e.metadata().ok()?;
                        (m.is_file()).then(|| (e.path(), m.len()))
                    })
                    .collect()
            };
            let before: Vec<PathBuf> = list(&downloads).into_iter().map(|(p, _)| p).collect();
            let deadline = Instant::now() + Duration::from_secs(8 * 60 * 60);
            let mut last: Option<(PathBuf, u64)> = None;
            while Instant::now() < deadline {
                std::thread::sleep(Duration::from_secs(5));
                let fresh = list(&downloads)
                    .into_iter()
                    .find(|(p, _)| !before.contains(p));
                match (fresh, &last) {
                    (Some((p, n)), Some((lp, ln))) if &p == lp && n == *ln && n > 0 => {
                        return Ok(p)
                    }
                    (Some(f), _) => last = Some(f),
                    (None, _) => last = None,
                }
            }
            Err("no new Xcode_*.xip appeared in ~/Downloads within eight hours".into())
        }

        fn read_from(&self, name: &str) -> Result<Prefill, String> {
            if let Some(why) = golden_name_problem(name) {
                return Err(why);
            }
            let path = self
                .store
                .join(name)
                .join(super::super::golden_build::MANIFEST_FILE);
            let meta = std::fs::symlink_metadata(&path).map_err(|e| {
                format!(
                    "{} has no readable manifest ({e}): an image built by hand cannot be \
                     rebuilt with --from",
                    path.display()
                )
            })?;
            if meta.uid() != 0 || meta.mode() & 0o022 != 0 {
                return Err(format!(
                    "{} is not root's, or is writable by others",
                    path.display()
                ));
            }
            let text = Self::read_small(&path, MAX_OPTIONAL_BYTES)?
                .ok_or_else(|| format!("{} is gone", path.display()))?;
            parse_from_manifest(text.as_bytes())
        }

        fn catalog_latest(&mut self) -> Result<Restore, String> {
            let out = Self::run_prep(&self.prep, &["latest"], Duration::from_secs(120))?;
            parse_restore(&out, true)
        }

        fn verify_ipsw(&mut self, ipsw: &Path) -> Result<Restore, String> {
            let s = ipsw.to_str().ok_or("the restore image path is not UTF-8")?;
            let out = Self::run_prep(&self.prep, &["verify", s], Duration::from_secs(300))?;
            parse_restore(&out, false)
        }

        fn remote_size(&mut self, url: &str) -> Result<u64, String> {
            let out = Self::tool(
                "/usr/bin/curl",
                &[
                    "-fsSIL".as_ref(),
                    "--proto".as_ref(),
                    "=https".as_ref(),
                    url.as_ref(),
                ],
                Duration::from_secs(120),
            )?;
            if !out.success {
                return Err(format!(
                    "cannot ask Apple for the image's size: {}",
                    out.sanitized(200)
                ));
            }
            content_length(&out.stdout)
                .filter(|n| *n > 0)
                .ok_or_else(|| "Apple's server gave no Content-Length".to_string())
        }

        fn file_len(&self, path: &Path) -> Result<u64, String> {
            let m = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
            if !m.is_file() {
                return Err(format!("{} is not a file", path.display()));
            }
            Ok(m.len())
        }

        fn download(&mut self, url: &str, dest: &Path) -> Result<(), String> {
            let mut cmd = Command::new("/usr/bin/curl");
            cmd.args(["-fL", "--proto", "=https", "--retry", "3", "-C", "-", "-o"])
                .arg(dest)
                .arg(url)
                .env("PATH", SAFE_PATH);
            let status = cmd.status().map_err(|e| e.to_string())?;
            status
                .success()
                .then_some(())
                .ok_or_else(|| "the download failed".to_string())
        }

        fn sha256(&mut self, path: &Path) -> Result<String, String> {
            let out = Self::tool(
                "/usr/bin/shasum",
                &["-a".as_ref(), "256".as_ref(), path.as_os_str()],
                LONG,
            )?;
            let digest = out
                .stdout
                .split_whitespace()
                .next()
                .unwrap_or_default()
                .to_string();
            if !out.success || sha256_problem(&digest).is_some() {
                return Err("shasum printed no digest".into());
            }
            Ok(digest)
        }

        fn remove_file(&mut self, path: &Path) {
            let _ = std::fs::remove_file(path);
        }

        fn install(&mut self, bundle: &Path, ipsw: &Path, disk_gib: u32) -> Result<(), String> {
            let bin = &self.prep;
            assert!(bin.is_absolute());
            let mut cmd = Command::new(bin); // absolute: checked above
            cmd.arg("create")
                .arg(bundle)
                .arg(ipsw)
                .args(["--disk-gib", &disk_gib.to_string()])
                .args(["--cpus", &CPUS.to_string()])
                .args(["--memory-gib", &MEMORY_GIB.to_string()])
                .stdin(Stdio::null())
                .stdout(Stdio::piped());
            let mut child = cmd.spawn().map_err(|e| e.to_string())?;
            if let Some(out) = child.stdout.take() {
                for line in std::io::BufReader::new(out).lines().map_while(Result::ok) {
                    if line.starts_with("install_percent=") || line.starts_with("install=") {
                        println!("{}", crate::term::sanitize(&line));
                    }
                }
            }
            let status = child.wait().map_err(|e| e.to_string())?;
            status
                .success()
                .then_some(())
                .ok_or_else(|| format!("{status}"))
        }

        fn boot(&mut self, bundle: &Path, files: &[(&str, &str)]) -> Result<BootEnd, String> {
            let parent = bundle.parent().ok_or("the bundle has no parent")?;
            let name = bundle
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or("bad bundle name")?;
            let dir = parent.join(format!(".{name}-setup-src"));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&dir)
                .map_err(|e| e.to_string())?;
            let disk = parent.join(format!(".{name}-setup.img"));
            let cleanup = |dir: &Path, disk: &Path| {
                let _ = std::fs::remove_dir_all(dir);
                let _ = std::fs::remove_file(disk);
            };
            for (n, text) in files {
                if let Err(e) = std::fs::write(dir.join(n), text) {
                    cleanup(&dir, &disk);
                    return Err(e.to_string());
                }
            }
            let built =
                tokio::runtime::Handle::current().block_on(crate::vm::tools_disk::build_volume(
                    &dir,
                    &disk,
                    super::super::golden_build::SETUP_VOLUME,
                ));
            if let Err(e) = built {
                cleanup(&dir, &disk);
                return Err(format!("hdiutil: {e:#}"));
            }
            let bin = &self.prep;
            assert!(bin.is_absolute());
            let mut cmd = Command::new(bin); // absolute: checked above
            cmd.arg("boot")
                .arg(bundle)
                .arg("--staging")
                .arg(&disk)
                .args(["--cpus", &CPUS.to_string()])
                .args(["--memory-gib", &MEMORY_GIB.to_string()])
                .stdin(Stdio::null());
            // Ctrl-C reaches the tool too, which asks the guest to shut down; this process
            // keeps waiting rather than leave the window behind.
            let before = super::super::walled_vm::swap_disposition(
                super::super::walled_vm::SIGINT,
                super::super::walled_vm::SIG_IGN,
            );
            let status = cmd.spawn().and_then(|mut c| c.wait());
            super::super::walled_vm::swap_disposition(super::super::walled_vm::SIGINT, before);
            cleanup(&dir, &disk);
            match status.map_err(|e| e.to_string())?.code() {
                Some(0) => Ok(BootEnd::ShutDown),
                Some(3) => Ok(BootEnd::Forced),
                other => Err(format!("the boot failed (exit {other:?})")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::exe::PathFacts;

    const HOST_RUNTIMES_JSON: &str = r#"{
  "00000000-0000-0000-0000-000000000001" : {
    "build" : "23A0000",
    "deletable" : true,
    "kind" : "Patchable Cryptex Disk Image",
    "platformIdentifier" : "com.apple.platform.iphonesimulator",
    "runtimeIdentifier" : "com.apple.CoreSimulator.SimRuntime.iOS-26-3",
    "signatureState" : "Verified",
    "sizeBytes" : 8000000000,
    "state" : "Ready",
    "supportedArchitectures" : ["arm64"],
    "version" : "26.3.1"
  },
  "00000000-0000-0000-0000-000000000002" : {
    "build" : "24A5000",
    "platformIdentifier" : "com.apple.platform.iphonesimulator",
    "sizeBytes" : 9000000000,
    "state" : "Ready",
    "version" : "27.0"
  },
  "00000000-0000-0000-0000-000000000003" : {
    "build" : "22T1",
    "platformIdentifier" : "com.apple.platform.watchsimulator",
    "sizeBytes" : 3000000000,
    "state" : "Ready",
    "version" : "26.0"
  },
  "00000000-0000-0000-0000-000000000004" : {
    "platformIdentifier" : "com.apple.platform.iphonesimulator",
    "state" : "Unusable",
    "version" : "17.0"
  },
  "00000000-0000-0000-0000-000000000005" : {
    "platformIdentifier" : "com.apple.platform.macosx",
    "state" : "Ready",
    "version" : "26.0"
  }
}"#;

    fn id(v: &str, b: Option<&str>) -> XcodeIdentity {
        XcodeIdentity {
            version: v.into(),
            build: b.map(str::to_string),
        }
    }

    fn host_rts() -> Vec<HostRuntime> {
        parse_simctl_runtimes(HOST_RUNTIMES_JSON).unwrap()
    }

    fn tokens(p: &RuntimePlan) -> Vec<String> {
        p.tokens()
    }

    // --- Xcode identity ---

    #[test]
    fn a_version_plist_gives_the_version_and_build() {
        let j = r#"{"CFBundleShortVersionString":"27.1","ProductBuildVersion":"27A9275","x":1}"#;
        let i = XcodeIdentity::from_version_plist_json(j).unwrap();
        assert_eq!(i, id("27.1", Some("27A9275")));
        assert_eq!(i.shown(), "27.1 (27A9275)");
        assert!(XcodeIdentity::from_version_plist_json("{}").is_err());
        assert!(XcodeIdentity::from_version_plist_json(
            r#"{"CFBundleShortVersionString":"27.1; rm -rf"}"#
        )
        .is_err());
        // A build that is not plain letters and digits is dropped, not trusted.
        let odd = XcodeIdentity::from_version_plist_json(
            r#"{"CFBundleShortVersionString":"27.1","ProductBuildVersion":"a b"}"#,
        )
        .unwrap();
        assert_eq!(odd.build, None);
    }

    #[test]
    fn a_xip_name_gives_the_version() {
        for (name, want) in [
            ("Xcode_27.0.xip", Some("27.0")),
            ("Xcode_27.0_Release_Candidate.xip", Some("27.0")),
            ("Xcode 27.1 beta.xip", Some("27.1")),
            ("Xcode_16.4.1.xip", Some("16.4.1")),
            ("Xcode.xip", None),
            ("Other_27.0.xip", None),
            ("Xcode_x.xip", None),
        ] {
            assert_eq!(
                XcodeIdentity::from_xip_name(name).map(|i| i.version),
                want.map(str::to_string),
                "{name}"
            );
        }
        assert!(is_xcode_xip_name("Xcode_27.0.xip"));
        assert!(!is_xcode_xip_name("Xcode_27.0.xip.download"));
        assert!(!is_xcode_xip_name("Xcode_27.0.xip.crdownload"));
        assert!(!is_xcode_xip_name("Notes.xip"));
    }

    #[test]
    fn the_sdk_major_is_told_from_xcode_26_on() {
        assert_eq!(id("27.1", None).sdk_major(), Some(27));
        assert_eq!(id("26.0", None).sdk_major(), Some(26));
        assert_eq!(id("16.4", None).sdk_major(), None);
        assert!(id("27.1", Some("A")).same_as(&id("27.1", None)));
        assert!(!id("27.1", Some("A")).same_as(&id("27.1", Some("B"))));
        assert!(!id("27.0", None).same_as(&id("27.1", None)));
        assert_eq!(default_image_name(&id("27.1", None)), "xcode-27.1");
        assert!(golden_name_problem(&default_image_name(&id("27.1", None))).is_none());
    }

    #[test]
    fn the_download_page_names_the_matching_xcode() {
        assert_eq!(
            xcode_download_url(Some(27)),
            "https://developer.apple.com/download/all/?q=Xcode%2027"
        );
        assert!(xcode_download_url(None).starts_with("https://developer.apple.com/"));
    }

    #[test]
    fn a_xip_must_be_apples_signed_software() {
        let good = "Package \"Xcode_27.0.xip\":\n   Status: signed Apple Software\n   Certificate Chain:\n    1. Software Signing\n    3. Apple Root CA\n";
        assert_eq!(xip_signature_problem(true, good), None);
        assert!(xip_signature_problem(false, good).is_some());
        assert!(xip_signature_problem(true, "Status: no signature\n").is_some());
        assert!(xip_signature_problem(
            true,
            "Status: signed by a certificate not trusted\n 1. Apple Root CA\n"
        )
        .is_some());
    }

    // --- runtimes ---

    #[test]
    fn host_runtimes_are_read_from_simctl_output() {
        let rts = host_rts();
        let shown: Vec<String> = rts
            .iter()
            .map(|r| format!("{} {}", r.platform, r.version))
            .collect();
        // Not-ready and non-simulator entries are left out; sorted platform, then version.
        assert_eq!(shown, ["iOS 26.3.1", "iOS 27.0", "watchOS 26.0"]);
        assert_eq!(rts[0].size_bytes, Some(8000000000));
        assert!(parse_simctl_runtimes("not json").is_err());
        assert!(parse_simctl_runtimes(&" ".repeat(MAX_SIMCTL_BYTES + 1)).is_err());
        assert_eq!(parse_simctl_runtimes("{}").unwrap(), vec![]);
        // A hostile version is not carried.
        let hostile = r#"{"a":{"state":"Ready","platformIdentifier":"com.apple.platform.iphonesimulator","version":"26;rm"}}"#;
        assert_eq!(parse_simctl_runtimes(hostile).unwrap(), vec![]);
    }

    #[test]
    fn the_same_xcode_takes_the_hosts_runtimes_its_sdk_can_run() {
        let x = id("27.1", Some("27A9275"));
        let plan = plan_runtimes(
            RuntimeSource::Inferred {
                no_host_runtimes: false,
            },
            &x,
            Some(&x),
            &host_rts(),
        )
        .unwrap();
        // iOS 27.0 matches the SDK, so the matching runtime is not added again.
        assert_eq!(tokens(&plan), ["iOS:26.3.1", "iOS:27.0", "watchOS:26.0"]);
        assert_eq!(plan.total(), (8000000000 + 9000000000 + 3000000000, false));
        assert!(plan.choices[0].why.contains("installed on this host"));
    }

    #[test]
    fn the_matching_ios_is_added_when_the_host_has_none() {
        let x = id("27.1", None);
        let only26: Vec<HostRuntime> = host_rts()
            .into_iter()
            .filter(|r| r.version != "27.0")
            .collect();
        let plan = plan_runtimes(
            RuntimeSource::Inferred {
                no_host_runtimes: false,
            },
            &x,
            Some(&x),
            &only26,
        )
        .unwrap();
        assert_eq!(tokens(&plan), ["iOS", "iOS:26.3.1", "watchOS:26.0"]);
        assert!(plan.total().1, "the matching runtime has no known size");
        assert!(plan.notes.iter().any(|n| n.contains("no iOS 27 runtime")));
        // A host with no runtimes at all.
        let none = plan_runtimes(
            RuntimeSource::Inferred {
                no_host_runtimes: false,
            },
            &x,
            Some(&x),
            &[],
        )
        .unwrap();
        assert_eq!(tokens(&none), ["iOS"]);
    }

    #[test]
    fn runtimes_newer_than_the_sdk_are_left_out_and_said() {
        let x = id("26.2", None);
        let plan = plan_runtimes(
            RuntimeSource::Inferred {
                no_host_runtimes: false,
            },
            &x,
            Some(&x),
            &host_rts(),
        )
        .unwrap();
        assert_eq!(tokens(&plan), ["iOS:26.3.1", "watchOS:26.0"]);
        assert!(plan.notes.iter().any(|n| n.contains("iOS 27.0")));
    }

    #[test]
    fn a_different_xcode_falls_back_to_the_matching_ios_and_says_so() {
        let chosen = id("27.0", None);
        let host = id("27.1", Some("27A9275"));
        let plan = plan_runtimes(
            RuntimeSource::Inferred {
                no_host_runtimes: false,
            },
            &chosen,
            Some(&host),
            &host_rts(),
        )
        .unwrap();
        assert_eq!(tokens(&plan), ["iOS"]);
        assert!(
            plan.notes[0].contains("27.1") && plan.notes[0].contains("matching iOS"),
            "{:?}",
            plan.notes
        );
        // No Xcode on the host at all.
        let none = plan_runtimes(
            RuntimeSource::Inferred {
                no_host_runtimes: false,
            },
            &chosen,
            None,
            &[],
        )
        .unwrap();
        assert_eq!(tokens(&none), ["iOS"]);
        assert!(none.notes[0].contains("no Xcode"));
        // An Xcode before 26 has no told SDK.
        let old = id("16.4", None);
        let p = plan_runtimes(
            RuntimeSource::Inferred {
                no_host_runtimes: false,
            },
            &old,
            Some(&old),
            &host_rts(),
        )
        .unwrap();
        assert_eq!(tokens(&p), ["iOS"]);
    }

    #[test]
    fn explicit_runtimes_replace_the_inference_and_no_host_runtimes_keeps_only_ios() {
        let x = id("27.1", Some("27A9275"));
        let flags = vec![
            "iOS:26.3.1".to_string(),
            "tvOS".to_string(),
            "iOS:26.3.1".to_string(),
        ];
        let plan =
            plan_runtimes(RuntimeSource::Explicit(&flags), &x, Some(&x), &host_rts()).unwrap();
        assert_eq!(tokens(&plan), ["iOS:26.3.1", "tvOS"]);
        // A known version carries the host's size.
        assert_eq!(plan.choices[0].size_bytes, Some(8000000000));
        assert_eq!(plan.choices[1].size_bytes, None);
        assert!(plan_runtimes(
            RuntimeSource::Explicit(&["iOS:../x".to_string()]),
            &x,
            Some(&x),
            &[]
        )
        .is_err());
        let off = plan_runtimes(
            RuntimeSource::Inferred {
                no_host_runtimes: true,
            },
            &x,
            Some(&x),
            &host_rts(),
        )
        .unwrap();
        assert_eq!(tokens(&off), ["iOS"]);
        let from = vec!["watchOS:26.0".to_string()];
        let f = plan_runtimes(RuntimeSource::From(&from, "xcode-26.0"), &x, None, &[]).unwrap();
        assert_eq!(tokens(&f), ["watchOS:26.0"]);
        assert!(f.choices[0].why.contains("xcode-26.0"));
    }

    #[test]
    fn the_recipe_shows_each_runtime_with_a_size_and_a_total() {
        let x = id("27.1", Some("27A9275"));
        let plan = plan_runtimes(
            RuntimeSource::Inferred {
                no_host_runtimes: false,
            },
            &x,
            Some(&x),
            &host_rts()
                .into_iter()
                .filter(|r| r.version != "27.0")
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let r = ResolvedRecipe::new(
            "xcode-27.1",
            Path::new("/Applications/Xcode.app"),
            XcodeKind::App,
            &plan.tokens(),
            &["ruby".to_string()],
        )
        .unwrap();
        let text = render_recipe(&r, &x, &plan, "verified").join("\n");
        assert!(text.contains("xcode-27.1"), "{text}");
        assert!(text.contains("27.1 (27A9275)"), "{text}");
        assert!(text.contains("7.5 GiB"), "{text}");
        assert!(text.contains("size not known"), "{text}");
        assert!(
            text.contains("total") && text.contains("at least"),
            "{text}"
        );
        assert!(text.contains("jq gh zstd git-lfs + ruby"), "{text}");
    }

    // --- --from ---

    fn manifest(recipe: &str) -> String {
        format!(
            r#"{{"schema":1,"recipe":{recipe},"installed":{{"anything":[1,2]}},"built_at":1,"faber_version":"0.4.0"}}"#
        )
    }

    const GOOD_RECIPE: &str = r#"{"xcode_source":"xip","xcode_file":"Xcode_27.0.xip","runtimes":["iOS:26.3.1","watchOS"],"brew_extra":["ruby"],"homebrew_tag":"7.0.10","homebrew_sha256":"00"}"#;

    #[test]
    fn a_stored_manifest_prefills_the_recipe() {
        let p = parse_from_manifest(manifest(GOOD_RECIPE).as_bytes()).unwrap();
        assert_eq!(p.runtimes, ["iOS:26.3.1", "watchOS"]);
        assert_eq!(p.brew, ["ruby"]);
    }

    #[test]
    fn a_manifest_that_is_not_ours_is_refused() {
        // Oversize.
        let big = vec![b' '; MAX_OPTIONAL_BYTES as usize + 1];
        assert!(parse_from_manifest(&big).unwrap_err().contains("over"));
        // Wrong schema.
        let s2 = manifest(GOOD_RECIPE).replace("\"schema\":1", "\"schema\":2");
        assert!(parse_from_manifest(s2.as_bytes())
            .unwrap_err()
            .contains("schema 2"));
        // Unknown fields, missing fields, not JSON, not UTF-8.
        let extra =
            manifest(GOOD_RECIPE).replace("\"built_at\":1", "\"built_at\":1,\"surprise\":true");
        assert!(parse_from_manifest(extra.as_bytes()).is_err());
        let extra_in_recipe =
            manifest(&GOOD_RECIPE.replace("\"brew_extra\"", "\"x\":1,\"brew_extra\""));
        assert!(parse_from_manifest(extra_in_recipe.as_bytes()).is_err());
        assert!(parse_from_manifest(br#"{"schema":1}"#).is_err());
        assert!(parse_from_manifest(b"[]").is_err());
        assert!(parse_from_manifest(&[0xff, 0xfe]).is_err());
    }

    #[test]
    fn hostile_manifest_values_never_reach_a_command() {
        for (what, bad) in [
            ("runtime", GOOD_RECIPE.replace("iOS:26.3.1", "iOS:../x")),
            ("runtime platform", GOOD_RECIPE.replace("watchOS", "macOS")),
            ("flag as formula", GOOD_RECIPE.replace("ruby", "--force")),
            ("shell in formula", GOOD_RECIPE.replace("ruby", "a;rm")),
            (
                "path in file",
                GOOD_RECIPE.replace("Xcode_27.0.xip", "../../etc/passwd"),
            ),
            ("source", GOOD_RECIPE.replace("\"xip\"", "\"dmg\"")),
            (
                "escape in file",
                GOOD_RECIPE.replace("Xcode_27.0.xip", "x\\u001b[2J.xip"),
            ),
        ] {
            let r = parse_from_manifest(manifest(&bad).as_bytes());
            let e = r.expect_err(what);
            assert!(!e.contains('\u{1b}'), "{what}: {e:?}");
        }
        // Too many runtimes or formulas.
        let many = (0..9)
            .map(|i| format!("\"iOS:1.{i}\""))
            .collect::<Vec<_>>()
            .join(",");
        let r = GOOD_RECIPE.replace("[\"iOS:26.3.1\",\"watchOS\"]", &format!("[{many}]"));
        assert!(parse_from_manifest(manifest(&r).as_bytes()).is_err());
    }

    // --- the printed command ---

    #[test]
    fn the_build_command_is_exact_and_quoted() {
        let r = ResolvedRecipe::new(
            "xcode-27.1",
            Path::new("/Applications/Xcode.app"),
            XcodeKind::App,
            &["iOS".to_string(), "iOS:26.3.1".to_string()],
            &["ruby".to_string(), "python@3.13".to_string()],
        )
        .unwrap();
        assert_eq!(
            r.build_command(Path::new("/srv/op/faber golden/macos-27")),
            "sudo faber golden build '/srv/op/faber golden/macos-27' --name xcode-27.1 \
             --xcode /Applications/Xcode.app --runtime iOS --runtime iOS:26.3.1 --brew ruby \
             --brew python@3.13"
        );
        assert_eq!(shell_quote("a b"), "'a b'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
        assert_eq!(shell_quote(""), "''");
        assert_eq!(shell_quote("$(x)"), "'$(x)'");
        assert_eq!(shell_quote("a;b"), "'a;b'");
        assert_eq!(shell_quote("~/x"), "'~/x'");
        // An Xcode path with a quote, and with a space.
        let r2 = ResolvedRecipe::new(
            "x",
            Path::new("/srv/a'b/Downloads/Xcode 27.xip"),
            XcodeKind::Xip,
            &[],
            &[],
        )
        .unwrap();
        let c = r2.build_command(Path::new("/b"));
        assert!(
            c.contains("--xcode '/srv/a'\\''b/Downloads/Xcode 27.xip'"),
            "{c}"
        );
        assert!(c.contains("--runtime iOS"), "{c}");
    }

    #[test]
    fn a_recipe_file_round_trips_and_refuses_what_it_did_not_write() {
        let r = ResolvedRecipe::new(
            "xcode-27.1",
            Path::new("/Applications/Xcode.app"),
            XcodeKind::App,
            &["iOS:26.3.1".to_string()],
            &["ruby".to_string()],
        )
        .unwrap();
        let back = ResolvedRecipe::from_json(&r.to_json()).unwrap();
        assert_eq!(back, r);
        let bad = r.to_json().replace("xcode-27.1", "../x");
        assert!(ResolvedRecipe::from_json(&bad).is_err());
        let kind = r.to_json().replace("\"app\"", "\"xip\"");
        assert!(ResolvedRecipe::from_json(&kind).is_err());
        let rel = r.to_json().replace("/Applications/Xcode.app", "Xcode.app");
        assert!(ResolvedRecipe::from_json(&rel).is_err());
        assert!(ResolvedRecipe::from_json("{}").is_err());
        let flag = r.to_json().replace("\"ruby\"", "\"--evil\"");
        assert!(ResolvedRecipe::from_json(&flag).is_err());
    }

    // --- disk ---

    #[test]
    fn the_bundle_must_share_the_stores_volume() {
        let p = Path::new("/srv/op/faber-golden");
        let s = Path::new("/var/db/faber/images");
        assert_eq!(volume_problem(7, 7, p, s), None);
        let why = volume_problem(7, 9, p, s).unwrap();
        assert!(
            why.contains("same volume") && why.contains("/var/db/faber/images"),
            "{why}"
        );
    }

    #[test]
    fn each_step_needs_its_own_room_above_the_floor() {
        let v = Path::new("/srv/op");
        assert_eq!(disk_needed(DiskStep::Start, 0), PEAK_BYTES + FLOOR_BYTES);
        assert_eq!(
            disk_needed(DiskStep::Download, 25 << 30),
            (25 << 30) + INSTALL_BYTES + FLOOR_BYTES
        );
        assert_eq!(
            disk_needed(DiskStep::Install, 25 << 30),
            INSTALL_BYTES + FLOOR_BYTES
        );
        assert_eq!(disk_needed(DiskStep::Boot, 0), BOOT_BYTES + FLOOR_BYTES);
        assert_eq!(free_problem(DiskStep::Start, 95 << 30, 0, v), None);
        let why = free_problem(DiskStep::Start, 94 << 30, 0, v).unwrap();
        assert!(
            why.contains("94.0 GiB") && why.contains("95.0 GiB") && why.contains("ADR-0024"),
            "{why}"
        );
        assert!(free_problem(DiskStep::Boot, 12 << 30, 0, v).is_none());
        assert!(free_problem(DiskStep::Boot, 11 << 30, 0, v).is_some());
    }

    // --- resume ---

    fn facts(exists: bool, entries: &[&str], phase: Option<Phase>) -> BundleFacts {
        BundleFacts {
            exists,
            entries: entries.iter().map(|s| s.to_string()).collect(),
            phase,
            trust_problem: None,
        }
    }

    const FOUR: [&str; 4] = ["disk", "vz-aux", "vz-hwmodel", "vz-identifier"];

    #[test]
    fn resume_starts_where_the_last_run_stopped() {
        let b = Path::new("/b");
        assert_eq!(resume_state(b, &facts(false, &[], None)), Ok(Resume::Fresh));
        assert_eq!(resume_state(b, &facts(true, &[], None)), Ok(Resume::Fresh));
        // A stale sidecar with no bundle is a fresh start.
        assert_eq!(
            resume_state(b, &facts(false, &[], Some(Phase::Booted))),
            Ok(Resume::Fresh)
        );
        assert_eq!(
            resume_state(b, &facts(true, &FOUR, Some(Phase::Installed))),
            Ok(Resume::Installed)
        );
        let mut done = FOUR.to_vec();
        done.push("recipe.json");
        assert_eq!(
            resume_state(b, &facts(true, &done, Some(Phase::Booted))),
            Ok(Resume::Booted)
        );
    }

    #[test]
    fn a_bundle_of_unknown_state_is_never_adopted() {
        let b = Path::new("/b");
        // Four files and no sidecar: it could already be armed.
        let e = resume_state(b, &facts(true, &FOUR, None)).unwrap_err();
        assert!(e.contains("Remove it"), "{e}");
        // A half install.
        let e = resume_state(b, &facts(true, &["disk"], None)).unwrap_err();
        assert!(e.contains("did not finish"), "{e}");
        // The sidecar says installed but a file is gone.
        let e = resume_state(b, &facts(true, &["disk"], Some(Phase::Installed))).unwrap_err();
        assert!(e.contains("missing") && e.contains("vz-aux"), "{e}");
        // The sidecar is strict.
        assert_eq!(
            Phase::from_json(&Phase::Booted.to_json()),
            Ok(Phase::Booted)
        );
        assert_eq!(
            Phase::from_json(&Phase::Installed.to_json()),
            Ok(Phase::Installed)
        );
        assert!(Phase::from_json(r#"{"schema":1,"phase":"done"}"#).is_err());
        assert!(Phase::from_json(r#"{"schema":2,"phase":"booted"}"#).is_err());
        assert!(Phase::from_json("booted").is_err());
    }

    // --- the tool ---

    fn chain(entries: &[(&str, u32, u32)]) -> Vec<PathFacts> {
        entries
            .iter()
            .map(|(p, uid, mode)| PathFacts {
                path: PathBuf::from(p),
                uid: *uid,
                gid: 0,
                mode: *mode,
            })
            .collect()
    }

    #[test]
    fn the_tool_must_be_roots_and_unwritable_all_the_way_up() {
        let ok = chain(&[
            ("/usr/local/bin/faber-golden-prep", 0, 0o100_755),
            ("/usr/local/bin", 0, 0o040_755),
            ("/usr/local", 0, 0o040_755),
            ("/", 0, 0o040_755),
        ]);
        assert_eq!(prep_trust_problem(&ok), None);
        let mut op_owned = ok.clone();
        op_owned[0].uid = 501;
        assert!(prep_trust_problem(&op_owned).unwrap().contains("not root"));
        let mut group_w = ok.clone();
        group_w[2].mode = 0o040_775;
        assert!(prep_trust_problem(&group_w).unwrap().contains("/usr/local"));
        let mut not_exec = ok.clone();
        not_exec[0].mode = 0o100_750;
        assert!(prep_trust_problem(&not_exec).unwrap().contains("execute"));
        let mut dir = ok.clone();
        dir[0].mode = 0o040_755;
        assert!(prep_trust_problem(&dir).is_some());
        assert!(prep_trust_problem(&[]).is_some());
    }

    #[test]
    fn the_tool_is_looked_for_beside_the_cli_then_beside_the_running_binary() {
        let c = prep_candidates(
            Path::new("/usr/local/bin/faber"),
            Some(Path::new("/opt/faber/faber")),
        );
        assert_eq!(
            c,
            [
                PathBuf::from("/usr/local/bin/faber-golden-prep"),
                PathBuf::from("/opt/faber/faber-golden-prep")
            ]
        );
        // Beside the CLI twice is listed once.
        let d = prep_candidates(
            Path::new("/usr/local/bin/faber"),
            Some(Path::new("/usr/local/bin/faber")),
        );
        assert_eq!(d.len(), 1);
    }

    #[test]
    fn locating_the_tool_skips_untrusted_ones_and_explains_when_none_is_left() {
        let ok = chain(&[
            ("/usr/local/bin/faber-golden-prep", 0, 0o100_755),
            ("/", 0, 0o040_755),
        ]);
        let found = locate_prep(
            &[
                PathBuf::from("/a/faber-golden-prep"),
                PathBuf::from("/usr/local/bin/faber-golden-prep"),
            ],
            |p| {
                if p.starts_with("/usr/local") {
                    Ok(ok.clone())
                } else {
                    Err(std::io::ErrorKind::NotFound.into())
                }
            },
        )
        .unwrap();
        assert_eq!(found, PathBuf::from("/usr/local/bin/faber-golden-prep"));
        let mut bad = ok.clone();
        bad[0].uid = 501;
        let e = locate_prep(
            &[PathBuf::from("/x/faber-golden-prep")],
            |_| Ok(bad.clone()),
        )
        .unwrap_err();
        assert!(e.contains("not root") && e.contains("sudo install"), "{e}");
        let e = locate_prep(&[PathBuf::from("/y/faber-golden-prep")], |_| {
            Err(std::io::ErrorKind::NotFound.into())
        })
        .unwrap_err();
        assert!(
            e.contains("not there") && e.contains("/usr/local/bin/faber-golden-prep"),
            "{e}"
        );
    }

    #[test]
    fn the_tool_must_be_this_releases() {
        assert_eq!(
            prep_version_problem("faber-golden-prep 0.4.0+gabc1234\n", "0.4.0+gabc1234"),
            None
        );
        assert!(
            prep_version_problem("faber-golden-prep 0.3.0+gabc1234", "0.4.0+gabc1234").is_some()
        );
        assert!(prep_version_problem("faber-vm 0.4.0+gabc1234", "0.4.0+gabc1234").is_some());
    }

    // --- restore image bits ---

    #[test]
    fn the_catalogs_restore_image_must_be_apples() {
        let ok = "https://updates.cdn-apple.com/2026/UniversalMac_27.0.1_26A434_Restore.ipsw";
        assert_eq!(restore_url_problem(ok), None);
        assert!(restore_url_problem("http://updates.cdn-apple.com/x.ipsw").is_some());
        assert!(restore_url_problem("https://evil.example.com/x.ipsw").is_some());
        assert!(restore_url_problem("https://updates.cdn-apple.com/x.zip").is_some());
        assert!(restore_url_problem("https://apple.com.evil.test/x.ipsw").is_some());
        assert!(restore_url_problem("https://a b.apple.com/x.ipsw").is_some());
        let out = format!("url={ok}\nbuild=26A434\nos_version=27.0.1\n");
        let r = parse_restore(&out, true).unwrap();
        assert_eq!((r.build.as_str(), r.macos_major()), ("26A434", Some(27)));
        assert!(parse_restore("build=x y\nos_version=27.0\n", false).is_err());
        assert!(parse_restore("build=26A434\nos_version=27;\n", false).is_err());
        assert!(parse_restore("build=26A434\n", false).is_err());
        assert_eq!(
            content_length(
                "HTTP/2 302\r\nlocation: x\r\n\r\nHTTP/2 200\r\nContent-Length: 26843545600\r\n"
            ),
            Some(26843545600)
        );
        assert_eq!(content_length("HTTP/2 200\r\n"), None);
    }

    #[test]
    fn every_file_arm_sh_wants_is_on_the_setup_disk() {
        let names: Vec<&str> = setup_files().iter().map(|(n, _)| *n).collect();
        let list = ARM_SH
            .lines()
            .skip_while(|l| !l.starts_with("for f in provision.sh"))
            .take(2)
            .collect::<Vec<_>>()
            .join(" ");
        let mut wanted: Vec<&str> = Vec::new();
        for w in list.split_whitespace().skip(3) {
            if w != "\\" {
                wanted.push(w.trim_end_matches(';'));
            }
            if w.ends_with(';') {
                break;
            }
        }
        assert!(wanted.len() >= 5, "{wanted:?}");
        for w in wanted {
            assert!(
                names.contains(&w),
                "arm.sh wants {w}, which the setup disk lacks"
            );
        }
        assert!(names.contains(&"arm.sh"));
    }

    // --- the request ---

    fn args(bundle: &str) -> PrepareArgs {
        PrepareArgs {
            bundle: PathBuf::from(bundle),
            ipsw: None,
            sha256: None,
            keep_ipsw: false,
            disk_gib: DEFAULT_DISK_GIB,
            xcode: None,
            runtimes: vec![],
            no_host_runtimes: false,
            brew: vec![],
            from: None,
            yes: false,
        }
    }

    #[test]
    fn the_request_checks_the_flags_before_anything_runs() {
        let cwd = Path::new("/srv/op");
        let r = PrepareRequest::new(&args("faber-golden/../macos-27/"), cwd).unwrap();
        assert_eq!(r.bundle, PathBuf::from("/srv/op/macos-27"));
        assert_eq!(r.parent, PathBuf::from("/srv/op"));
        assert_eq!(r.bundle_name, "macos-27");
        for bad in ["/", "/x/.hidden", "/x/a b", "/x/a\nb"] {
            assert!(PrepareRequest::new(&args(bad), cwd).is_err(), "{bad:?}");
        }
        let mut a = args("/b/x");
        a.disk_gib = 10;
        assert!(PrepareRequest::new(&a, cwd).is_err());
        a.disk_gib = 100;
        a.sha256 = Some("ABC".into());
        assert!(PrepareRequest::new(&a, cwd).is_err());
        a.sha256 = Some("a".repeat(64));
        assert!(PrepareRequest::new(&a, cwd).is_ok());
        a.xcode = Some(PathBuf::from("/x/Xcode.dmg"));
        assert!(PrepareRequest::new(&a, cwd)
            .unwrap_err()
            .contains(".app or .xip"));
        a.xcode = Some(PathBuf::from("Downloads/Xcode_27.0.xip"));
        let r = PrepareRequest::new(&a, cwd).unwrap();
        assert_eq!(
            r.xcode,
            Some((
                PathBuf::from("/srv/op/Downloads/Xcode_27.0.xip"),
                XcodeKind::Xip
            ))
        );
        a.xcode = None;
        a.runtimes = vec!["macOS".into()];
        assert!(PrepareRequest::new(&a, cwd).is_err());
        a.runtimes = vec![];
        a.brew = vec!["--force".into()];
        assert!(PrepareRequest::new(&a, cwd).is_err());
        a.brew = vec![];
        a.from = Some("../x".into());
        assert!(PrepareRequest::new(&a, cwd).is_err());
    }

    // --- the orchestration, against a fake host ---

    struct Fake {
        calls: Vec<String>,
        said: Vec<String>,
        root: bool,
        tty: bool,
        desktop: bool,
        answer: bool,
        facts: BundleFacts,
        bundle_dev: u64,
        store_dev: u64,
        free: u64,
        host_xcode: Option<(PathBuf, XcodeIdentity)>,
        runtimes: Vec<HostRuntime>,
        recipe_file: Option<String>,
        boot_end: BootEnd,
        install_fails: bool,
        prefill: Option<Prefill>,
        sha: String,
        free_after_download: Option<u64>,
        free_before_download: Option<u64>,
    }

    impl Fake {
        fn new() -> Self {
            let x = id("27.1", Some("27A9275"));
            Self {
                calls: vec![],
                said: vec![],
                root: false,
                tty: true,
                desktop: true,
                answer: true,
                facts: BundleFacts::default(),
                bundle_dev: 1,
                store_dev: 1,
                free: 500 << 30,
                host_xcode: Some((PathBuf::from("/Applications/Xcode.app"), x)),
                runtimes: host_rts(),
                recipe_file: None,
                boot_end: BootEnd::ShutDown,
                install_fails: false,
                prefill: None,
                sha: "a".repeat(64),
                free_after_download: None,
                free_before_download: None,
            }
        }
        fn called(&self, what: &str) -> bool {
            self.calls.iter().any(|c| c.starts_with(what))
        }
    }

    impl PrepareHost for Fake {
        fn is_root(&self) -> bool {
            self.root
        }
        fn interactive(&self) -> bool {
            self.tty
        }
        fn has_desktop_session(&self) -> bool {
            self.desktop
        }
        fn say(&mut self, line: &str) {
            self.said.push(line.to_string());
        }
        fn confirm(&mut self, q: &str) -> Result<bool, String> {
            self.calls.push(format!("confirm {q}"));
            Ok(self.answer)
        }
        fn bundle_facts(&self, _: &Path) -> Result<BundleFacts, String> {
            Ok(self.facts.clone())
        }
        fn device_of(&self, _: &Path) -> Result<u64, String> {
            Ok(self.bundle_dev)
        }
        fn store_device(&self) -> Result<u64, String> {
            Ok(self.store_dev)
        }
        fn store_path(&self) -> PathBuf {
            PathBuf::from("/var/db/faber/images")
        }
        fn free_bytes(&self, _: &Path) -> Result<u64, String> {
            if self.called("download") {
                return Ok(self.free_after_download.unwrap_or(self.free));
            }
            if self.called("make_parent") {
                return Ok(self.free_before_download.unwrap_or(self.free));
            }
            Ok(self.free)
        }
        fn make_parent(&mut self, _: &Path) -> Result<(), String> {
            self.calls.push("make_parent".into());
            Ok(())
        }
        fn write_phase(&mut self, _: &Path, p: Phase) -> Result<(), String> {
            self.calls.push(format!("phase {p:?}"));
            Ok(())
        }
        fn write_recipe(&mut self, _: &Path, j: &str) -> Result<(), String> {
            self.calls.push("write_recipe".into());
            self.recipe_file = Some(j.to_string());
            Ok(())
        }
        fn read_recipe(&self, _: &Path) -> Result<Option<String>, String> {
            Ok(self.recipe_file.clone())
        }
        fn remove_bundle(&mut self, _: &Path) {
            self.calls.push("remove_bundle".into());
        }
        fn host_xcode(&self) -> Option<(PathBuf, XcodeIdentity)> {
            self.host_xcode.clone()
        }
        fn xcode_identity(&self, p: &Path, kind: XcodeKind) -> Result<XcodeIdentity, String> {
            match kind {
                XcodeKind::App => self
                    .host_xcode
                    .as_ref()
                    .map(|(_, i)| i.clone())
                    .ok_or("none".into()),
                XcodeKind::Xip => p
                    .file_name()
                    .and_then(|n| n.to_str())
                    .and_then(XcodeIdentity::from_xip_name)
                    .ok_or("no version".into()),
            }
        }
        fn verify_xcode(&mut self, p: &Path, _: XcodeKind) -> Result<String, String> {
            self.calls.push(format!("verify_xcode {}", p.display()));
            Ok("ok".into())
        }
        fn host_runtimes(&self) -> Result<Vec<HostRuntime>, String> {
            Ok(self.runtimes.clone())
        }
        fn open_url(&mut self, u: &str) -> Result<(), String> {
            self.calls.push(format!("open {u}"));
            Ok(())
        }
        fn wait_for_xip(&mut self) -> Result<PathBuf, String> {
            self.calls.push("wait_for_xip".into());
            Ok(PathBuf::from("/srv/op/Downloads/Xcode_27.1.xip"))
        }
        fn read_from(&self, _: &str) -> Result<Prefill, String> {
            self.prefill.clone().ok_or("no manifest".into())
        }
        fn catalog_latest(&mut self) -> Result<Restore, String> {
            self.calls.push("catalog_latest".into());
            Ok(Restore {
                url: Some("https://updates.cdn-apple.com/r.ipsw".into()),
                build: "26A434".into(),
                os_version: "27.0.1".into(),
            })
        }
        fn verify_ipsw(&mut self, p: &Path) -> Result<Restore, String> {
            self.calls.push(format!("verify_ipsw {}", p.display()));
            Ok(Restore {
                url: None,
                build: "26A434".into(),
                os_version: "27.0.1".into(),
            })
        }
        fn remote_size(&mut self, _: &str) -> Result<u64, String> {
            Ok(25 << 30)
        }
        fn file_len(&self, _: &Path) -> Result<u64, String> {
            Ok(25 << 30)
        }
        fn download(&mut self, _: &str, d: &Path) -> Result<(), String> {
            self.calls.push(format!("download {}", d.display()));
            Ok(())
        }
        fn sha256(&mut self, _: &Path) -> Result<String, String> {
            Ok(self.sha.clone())
        }
        fn remove_file(&mut self, p: &Path) {
            self.calls.push(format!("remove_file {}", p.display()));
        }
        fn install(&mut self, _: &Path, _: &Path, gib: u32) -> Result<(), String> {
            self.calls.push(format!("install {gib}"));
            if self.install_fails {
                Err("boom".into())
            } else {
                Ok(())
            }
        }
        fn boot(&mut self, _: &Path, files: &[(&str, &str)]) -> Result<BootEnd, String> {
            self.calls.push(format!(
                "boot {}",
                files.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(",")
            ));
            Ok(self.boot_end)
        }
    }

    fn req(extra: impl FnOnce(&mut PrepareArgs)) -> PrepareRequest {
        let mut a = args("/srv/op/faber-golden/macos-27");
        a.yes = true;
        extra(&mut a);
        PrepareRequest::new(&a, Path::new("/")).unwrap()
    }

    #[test]
    fn a_fresh_run_installs_boots_and_prints_the_build_command() {
        let mut h = Fake::new();
        let out = prepare(&mut h, &req(|_| {})).unwrap();
        assert_eq!(
            out.command,
            "sudo faber golden build /srv/op/faber-golden/macos-27 --name xcode-27.1 \
             --xcode /Applications/Xcode.app --runtime iOS:26.3.1 --runtime iOS:27.0 \
             --runtime watchOS:26.0"
        );
        let order: Vec<&str> = h.calls.iter().map(String::as_str).collect();
        let at = |p: &str| {
            order
                .iter()
                .position(|c| c.starts_with(p))
                .unwrap_or_else(|| panic!("{p} in {order:?}"))
        };
        assert!(
            at("verify_xcode") < at("install"),
            "the Xcode is checked first: {order:?}"
        );
        assert!(at("make_parent") < at("download"));
        assert!(at("download") < at("verify_ipsw"));
        assert!(at("install") < at("phase Installed"));
        assert!(at("phase Installed") < at("boot"));
        assert!(at("boot") < h.calls.iter().rposition(|c| c == "write_recipe").unwrap());
        assert!(at("write_recipe") < at("phase Booted"));
        // The boot carries arm.sh and the provisioning files.
        assert!(
            h.calls
                .iter()
                .any(|c| c.starts_with("boot arm.sh,provision.sh")),
            "{order:?}"
        );
        // The downloaded restore image is deleted.
        assert!(h.called("remove_file /srv/op/faber-golden/.restore-26A434.ipsw"));
        assert!(h.said.last().unwrap().contains("sudo faber golden build"));
        assert!(h.said.iter().any(|l| l.contains("arm.sh")));
    }

    #[test]
    fn keep_ipsw_and_a_given_image_leave_the_file_alone() {
        let mut h = Fake::new();
        prepare(&mut h, &req(|a| a.keep_ipsw = true)).unwrap();
        assert!(!h.called("remove_file"));
        let mut h = Fake::new();
        prepare(
            &mut h,
            &req(|a| a.ipsw = Some(PathBuf::from("/srv/op/r.ipsw"))),
        )
        .unwrap();
        assert!(!h.called("download") && !h.called("remove_file") && !h.called("catalog_latest"));
        assert!(h.called("verify_ipsw /srv/op/r.ipsw"));
    }

    #[test]
    fn a_pinned_digest_that_does_not_match_stops_before_the_install() {
        let mut h = Fake::new();
        let r = req(|a| a.sha256 = Some("b".repeat(64)));
        let e = prepare(&mut h, &r).unwrap_err();
        assert!(e.to_string().contains("not the pinned"), "{e}");
        assert!(!h.called("install"));
        // A download that fails the pin is removed.
        assert!(h.called("remove_file"));
    }

    #[test]
    fn a_resumed_install_skips_to_the_boot_and_a_finished_boot_to_the_command() {
        let mut h = Fake::new();
        h.facts = facts(true, &FOUR, Some(Phase::Installed));
        let out = prepare(&mut h, &req(|_| {})).unwrap();
        assert!(
            !h.called("download") && !h.called("install") && !h.called("catalog_latest"),
            "{:?}",
            h.calls
        );
        assert!(h.called("boot"));
        assert!(out.command.contains("xcode-27.1"));

        // Booted: nothing runs; the command comes from recipe.json.
        let recipe = out.recipe.to_json();
        let mut h = Fake::new();
        let mut done = FOUR.to_vec();
        done.push("recipe.json");
        h.facts = facts(true, &done, Some(Phase::Booted));
        h.recipe_file = Some(recipe);
        h.desktop = false; // not even a desktop is needed
        h.free = 0;
        let again = prepare(&mut h, &req(|_| {})).unwrap();
        assert_eq!(again.command, out.command);
        assert!(h.calls.is_empty(), "{:?}", h.calls);

        // Booted with its recipe lost: written anew, still no boot.
        let mut h = Fake::new();
        h.facts = facts(true, &FOUR, Some(Phase::Booted));
        let rebuilt = prepare(&mut h, &req(|_| {})).unwrap();
        assert!(h.called("write_recipe") && !h.called("boot") && !h.called("install"));
        assert_eq!(rebuilt.command, out.command);
    }

    #[test]
    fn refusals_come_before_any_work() {
        // Root.
        let mut h = Fake::new();
        h.root = true;
        assert!(prepare(&mut h, &req(|_| {}))
            .unwrap_err()
            .to_string()
            .contains("not as root"));
        // The wrong volume.
        let mut h = Fake::new();
        h.bundle_dev = 2;
        assert!(prepare(&mut h, &req(|_| {}))
            .unwrap_err()
            .to_string()
            .contains("same volume"));
        assert!(h.calls.is_empty());
        // Too little disk for the build, with the numbers.
        let mut h = Fake::new();
        h.free = 80 << 30;
        let e = prepare(&mut h, &req(|_| {})).unwrap_err().to_string();
        assert!(e.contains("80.0 GiB") && e.contains("95.0 GiB"), "{e}");
        assert!(h.calls.is_empty());
        // No desktop: stops before any download.
        let mut h = Fake::new();
        h.desktop = false;
        assert!(prepare(&mut h, &req(|_| {}))
            .unwrap_err()
            .to_string()
            .contains("desktop session"));
        assert!(h.calls.is_empty());
        // A bundle of unknown state.
        let mut h = Fake::new();
        h.facts = facts(true, &FOUR, None);
        assert!(prepare(&mut h, &req(|_| {})).is_err());
        assert!(h.calls.is_empty());
    }

    #[test]
    fn the_download_and_the_install_each_stop_short_of_the_floor() {
        // Room for the start (95 GiB), and for the download's own need (25 + 30 + 10).
        let mut h = Fake::new();
        h.free = 95 << 30;
        assert!(prepare(&mut h, &req(|_| {})).is_ok());
        // Once the image is on disk the install wants 40 GiB: 39 is refused, with nothing
        // installed and the image kept.
        let mut h = Fake::new();
        h.free_after_download = Some(39 << 30);
        let e = prepare(&mut h, &req(|_| {})).unwrap_err().to_string();
        assert!(e.contains("39.0 GiB") && e.contains("the install"), "{e}");
        assert!(
            !h.called("install") && !h.called("remove_file"),
            "{:?}",
            h.calls
        );
        // A download that would not leave room is not started.
        let why = free_problem(DiskStep::Download, 60 << 30, 25 << 30, Path::new("/v")).unwrap();
        assert!(why.contains("65.0 GiB"), "{why}");
        let mut h = Fake::new();
        h.free_before_download = Some(64 << 30);
        assert!(prepare(&mut h, &req(|_| {})).is_err());
        assert!(!h.called("download"), "{:?}", h.calls);
    }

    #[test]
    fn a_failed_install_removes_the_bundle_and_keeps_the_image() {
        let mut h = Fake::new();
        h.install_fails = true;
        let e = prepare(&mut h, &req(|_| {})).unwrap_err().to_string();
        assert!(h.called("remove_bundle"));
        assert!(!h.called("phase Installed") && !h.called("boot"));
        assert!(
            e.contains("--ipsw") && e.contains(".restore-26A434.ipsw"),
            "{e}"
        );
        // The download was not deleted.
        assert!(!h.called("remove_file"));
    }

    #[test]
    fn a_forced_off_guest_is_not_a_finished_boot() {
        let mut h = Fake::new();
        h.boot_end = BootEnd::Forced;
        let e = prepare(&mut h, &req(|_| {})).unwrap_err().to_string();
        assert!(e.contains("forced off"), "{e}");
        assert!(!h.called("phase Booted"));
        assert!(h.called("phase Installed"));
    }

    #[test]
    fn the_recipe_is_confirmed_unless_yes() {
        let mut h = Fake::new();
        let r = req(|a| a.yes = false);
        prepare(&mut h, &r).unwrap();
        assert!(h
            .calls
            .iter()
            .any(|c| c.starts_with("confirm Install this Xcode")));
        assert!(h
            .calls
            .iter()
            .any(|c| c.starts_with("confirm Build this recipe")));
        // Declined: nothing runs.
        let mut h = Fake::new();
        h.answer = false;
        h.host_xcode = None;
        let e = prepare(
            &mut h,
            &req(|a| {
                a.yes = false;
                a.xcode = Some(PathBuf::from("/X/Xcode_27.1.xip"));
            }),
        )
        .unwrap_err();
        assert_eq!(e, PrepareError::Declined);
        assert!(!h.called("install") && !h.called("download"));
        // Non-interactive without --yes.
        let mut h = Fake::new();
        h.tty = false;
        let e = prepare(
            &mut h,
            &req(|a| {
                a.yes = false;
                a.xcode = Some(PathBuf::from("/X/Xcode.app"));
            }),
        )
        .unwrap_err();
        assert!(e.to_string().contains("--yes"), "{e}");
    }

    #[test]
    fn without_an_xcode_it_opens_apples_page_and_waits_for_the_file() {
        let mut h = Fake::new();
        h.host_xcode = None;
        let out = prepare(&mut h, &req(|_| {})).unwrap();
        assert!(
            h.called("open https://developer.apple.com/download/all/?q=Xcode%2027"),
            "{:?}",
            h.calls
        );
        assert!(
            h.calls.iter().position(|c| c == "wait_for_xip")
                < h.calls.iter().position(|c| c.starts_with("verify_xcode"))
        );
        assert!(
            out.command
                .contains("--xcode /srv/op/Downloads/Xcode_27.1.xip"),
            "{}",
            out.command
        );
        // A xip from a different Xcode than the host's gets the matching iOS runtime.
        assert!(out.command.ends_with("--runtime iOS"), "{}", out.command);
        // Non-interactive: print the URL and stop.
        let mut h = Fake::new();
        h.host_xcode = None;
        h.tty = false;
        let e = prepare(&mut h, &req(|_| {})).unwrap_err();
        match e {
            PrepareError::NeedsXcode(u) => assert!(u.starts_with("https://developer.apple.com/")),
            other => panic!("{other:?}"),
        }
        assert!(!h.called("open") && !h.called("install"));
    }

    #[test]
    fn a_declined_host_xcode_goes_to_the_download() {
        let mut h = Fake::new();
        h.answer = false;
        let e = prepare(&mut h, &req(|a| a.yes = false)).unwrap_err();
        // Declining the host's Xcode starts the download wait, which the fake satisfies; the
        // recipe confirmation is then declined too.
        assert_eq!(e, PrepareError::Declined);
        assert!(h.called("wait_for_xip"));
    }

    #[test]
    fn from_prefills_and_flags_override() {
        let mut h = Fake::new();
        h.prefill = Some(Prefill {
            runtimes: vec!["iOS:26.0".into()],
            brew: vec!["ruby".into()],
        });
        let out = prepare(&mut h, &req(|a| a.from = Some("xcode-26.0".into()))).unwrap();
        assert!(
            out.command.contains("--runtime iOS:26.0") && !out.command.contains("watchOS"),
            "{}",
            out.command
        );
        assert!(out.command.contains("--brew ruby"));
        // Flags replace the prefilled parts.
        let mut h2 = Fake::new();
        h2.prefill = h.prefill.clone();
        let out = prepare(
            &mut h2,
            &req(|a| {
                a.from = Some("xcode-26.0".into());
                a.runtimes = vec!["tvOS".into()];
                a.brew = vec!["cocoapods".into()];
            }),
        )
        .unwrap();
        assert!(
            out.command.contains("--runtime tvOS") && !out.command.contains("iOS:26.0"),
            "{}",
            out.command
        );
        assert!(out.command.contains("--brew cocoapods") && !out.command.contains("ruby"));
        // An unreadable manifest stops it before any work.
        let mut h3 = Fake::new();
        assert!(prepare(&mut h3, &req(|a| a.from = Some("old".into()))).is_err());
        assert!(!h3.called("install"));
    }

    #[test]
    fn no_host_runtimes_and_explicit_runtimes_reach_the_command() {
        let mut h = Fake::new();
        let out = prepare(&mut h, &req(|a| a.no_host_runtimes = true)).unwrap();
        assert!(out.command.ends_with("--runtime iOS"), "{}", out.command);
        let mut h = Fake::new();
        let out = prepare(
            &mut h,
            &req(|a| a.runtimes = vec!["iOS:18.5".into(), "watchOS".into()]),
        )
        .unwrap();
        assert!(
            out.command
                .ends_with("--runtime iOS:18.5 --runtime watchOS"),
            "{}",
            out.command
        );
    }

    #[test]
    fn the_recipe_in_the_bundle_is_allowed_by_the_build() {
        use crate::setup::golden_build::prepared_entries_problem;
        let mut entries: Vec<String> = FOUR.iter().map(|s| s.to_string()).collect();
        assert_eq!(prepared_entries_problem(&entries), None);
        entries.push(RECIPE_FILE.to_string());
        assert_eq!(prepared_entries_problem(&entries), None);
        entries.push("stray".into());
        assert!(prepared_entries_problem(&entries)
            .unwrap()
            .contains("stray"));
    }

    // --- ownership, canonical exec, resume notes ---

    fn ef(name: &str, uid: u32, mode: u32) -> EntryFacts {
        EntryFacts {
            name: name.into(),
            uid,
            mode,
        }
    }

    #[test]
    fn a_bundle_is_the_operators_or_it_is_not_booted() {
        let parent = ef("/srv/op", 501, 0o040_700);
        let b = ef("/srv/op/b", 501, 0o040_700);
        let files = [
            ef("disk", 501, 0o100_600),
            ef("recipe.json", 501, 0o100_600),
        ];
        let side = ef(".b.prepare", 501, 0o100_600);
        assert_eq!(
            bundle_trust_problem(501, &parent, Some(&b), &files, Some(&side)),
            None
        );
        assert_eq!(bundle_trust_problem(501, &parent, None, &[], None), None);
        // A parent others can write, sticky or not.
        for mode in [0o040_775, 0o040_777, 0o041_777] {
            let shared = ef("/srv/shared", 0, mode);
            let why = bundle_trust_problem(501, &shared, Some(&b), &files, Some(&side)).unwrap();
            assert!(why.contains("plant a bundle"), "{why}");
        }
        // A parent owned by another account; root's or the operator's is fine (the
        // default location's parent is the operator's own 0700 directory).
        let alien = ef("/srv/other", 502, 0o040_755);
        assert!(
            bundle_trust_problem(501, &alien, Some(&b), &files, Some(&side))
                .unwrap()
                .contains("neither you nor root")
        );
        let rooted = ef("/srv/root", 0, 0o040_755);
        assert_eq!(
            bundle_trust_problem(501, &rooted, Some(&b), &files, Some(&side)),
            None
        );
        // A bundle directory others can write.
        for mode in [0o040_770, 0o040_707, 0o041_777] {
            let open = ef("b", 501, mode);
            assert!(bundle_trust_problem(501, &parent, Some(&open), &[], None)
                .unwrap()
                .contains("writable"));
        }
        // Someone else's bundle, file or sidecar; symlinks of any of them.
        let other = ef("b", 502, 0o040_700);
        assert!(bundle_trust_problem(501, &parent, Some(&other), &[], None)
            .unwrap()
            .contains("not owned by you"));
        let theirs = [ef("disk", 502, 0o100_600)];
        assert!(bundle_trust_problem(501, &parent, Some(&b), &theirs, None)
            .unwrap()
            .contains("disk"));
        let their_side = ef("s", 502, 0o100_600);
        assert!(bundle_trust_problem(501, &parent, Some(&b), &[], Some(&their_side)).is_some());
        let link_bundle = ef("b", 501, 0o120_755);
        assert!(
            bundle_trust_problem(501, &parent, Some(&link_bundle), &[], None)
                .unwrap()
                .contains("symlink")
        );
        let link_file = [ef("disk", 501, 0o120_755)];
        assert!(
            bundle_trust_problem(501, &parent, Some(&b), &link_file, None)
                .unwrap()
                .contains("symlink")
        );
        let link_side = ef("s", 501, 0o120_755);
        assert!(
            bundle_trust_problem(501, &parent, Some(&b), &[], Some(&link_side))
                .unwrap()
                .contains("symlink")
        );
        // A directory where a file belongs.
        let dir = [ef("disk", 501, 0o040_700)];
        assert!(bundle_trust_problem(501, &parent, Some(&b), &dir, None).is_some());
        // An escape in a name is not echoed raw.
        let odd = [ef("a\u{1b}[2J", 502, 0o100_600)];
        let why = bundle_trust_problem(501, &parent, Some(&b), &odd, None).unwrap();
        assert!(!why.contains('\u{1b}'), "{why:?}");
    }

    #[test]
    fn an_untrusted_bundle_stops_every_resume_state() {
        let mut f = facts(true, &FOUR, Some(Phase::Booted));
        f.trust_problem = Some("x is not owned by you".into());
        let e = resume_state(Path::new("/b"), &f).unwrap_err();
        assert!(
            e.contains("not owned by you") && e.contains("Nothing here will be booted"),
            "{e}"
        );
        let mut fresh = facts(false, &[], None);
        fresh.trust_problem = Some("the parent is writable".into());
        assert!(resume_state(Path::new("/b"), &fresh).is_err());
    }

    #[test]
    fn the_tool_that_runs_is_the_path_that_was_checked() {
        let canonical = chain(&[
            ("/opt/real/faber-golden-prep", 0, 0o100_755),
            ("/opt/real", 0, 0o040_755),
            ("/", 0, 0o040_755),
        ]);
        // The candidate is a link; the facts are those of where it resolves.
        let found = locate_prep(&[PathBuf::from("/usr/local/bin/faber-golden-prep")], |_| {
            Ok(canonical.clone())
        })
        .unwrap();
        assert_eq!(found, PathBuf::from("/opt/real/faber-golden-prep"));
    }

    #[test]
    fn a_resumed_run_says_when_its_recipe_differs_and_still_checks_the_volume() {
        // A fresh run records the recipe as soon as the install is done.
        let mut h = Fake::new();
        let first = prepare(&mut h, &req(|_| {})).unwrap();
        let at = |h: &Fake, p: &str| h.calls.iter().position(|c| c.starts_with(p)).unwrap();
        assert!(at(&h, "phase Installed") < at(&h, "write_recipe"));
        assert!(at(&h, "write_recipe") < at(&h, "boot"));

        // Installed, with a different recipe now: said.
        let mut h = Fake::new();
        h.facts = facts(true, &FOUR, Some(Phase::Installed));
        h.recipe_file = Some(first.recipe.to_json());
        prepare(&mut h, &req(|a| a.brew = vec!["ruby".into()])).unwrap();
        assert!(
            h.said
                .iter()
                .any(|l| l.contains("differs from the one recorded")),
            "{:?}",
            h.said
        );
        assert!(h.said.iter().any(|l| l.contains("--brew ruby")));
        // The same recipe: not said.
        let mut h = Fake::new();
        h.facts = facts(true, &FOUR, Some(Phase::Installed));
        h.recipe_file = Some(first.recipe.to_json());
        prepare(&mut h, &req(|_| {})).unwrap();
        assert!(
            !h.said.iter().any(|l| l.contains("differs")),
            "{:?}",
            h.said
        );

        // Booted: flags are said to be ignored, and the recorded recipe stands.
        let mut h = Fake::new();
        let mut done = FOUR.to_vec();
        done.push("recipe.json");
        h.facts = facts(true, &done, Some(Phase::Booted));
        h.recipe_file = Some(first.recipe.to_json());
        let out = prepare(&mut h, &req(|a| a.brew = vec!["ruby".into()])).unwrap();
        assert!(h
            .said
            .iter()
            .any(|l| l.contains("flags you gave are ignored")));
        assert_eq!(out.command, first.command);
        // And the volume is still checked there.
        let mut h = Fake::new();
        h.facts = facts(true, &done, Some(Phase::Booted));
        h.recipe_file = Some(first.recipe.to_json());
        h.bundle_dev = 2;
        assert!(prepare(&mut h, &req(|_| {}))
            .unwrap_err()
            .to_string()
            .contains("same volume"));
    }
}
