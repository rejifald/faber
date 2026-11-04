//! `sudo faber golden build`: the walled, root-driven half of building the golden macOS
//! image (ADR-0049 §2, delivery step 2C).
//!
//! ```text
//! sudo faber golden build <prepared-bundle> --name NAME --xcode Xcode_*.xip|Xcode.app
//!                         [--runtime PLATFORM[:VERSION]]... [--brew FORMULA]... [--replace]
//! ```
//!
//! `<prepared-bundle>` is what `faber golden prepare` left: the operator's macOS install,
//! armed with `arm.sh` (`launcher/golden/`). This command never boots it. In order:
//!
//! 1. The wall must cover the build network (slot 15, the Linux image build's), or nothing
//!    happens: the same check, and the same watch while the VM runs, as the Linux build
//!    ([`super::walled_vm`]).
//! 2. The operator's bundle is probed: exactly the four [`GOLDEN_FILES`], regular files with
//!    one link, owned by the operator, none a symlink. The name is checked against the store
//!    now, so a refusal costs minutes, not the build's hours.
//! 3. The setup disk is built as root, volume `FABERSETUP`: `provision.sh`, `recipe.env`, the
//!    bootstrap and the Xcode. The scripts are compiled into this binary; nothing is read
//!    from a checkout. The image is root:_faber_vm 0440 in a directory only those two enter, so the
//!    launcher accepts it as a tools disk (`requireToolsDisk`) and `_faber_vm` cannot change
//!    it. Root reads only what the operator owns: an `.xip` with one link, or an `.app` the
//!    operator's own account archives, which root only extracts.
//! 4. Root clones the operator's bundle into a build bundle owned by `_faber_vm`, in its home,
//!    and boots that as `_faber_vm` with the setup disk attached read-only. The guest fetches
//!    nothing until the wall is proven (`provision.sh`), and reports over a virtio console.
//! 5. The console is read bounded, by exact line prefix. Nothing else the guest wrote is
//!    parsed, and no guest text is echoed unsanitised.
//! 6. The four files come back into root's staging, `manifest.json` is written beside them,
//!    and the existing golden import ([`super::vm_images::apply::import_golden`]) lands the
//!    bundle in the store, `root:wheel` 0444.
//!
//! On any failure the operator's bundle is untouched, so a retry works, and root's and
//! `_faber_vm`'s staging are removed.
//!
//! # The Homebrew pin
//!
//! [`HOMEBREW_TAG`] and [`HOMEBREW_SHA256`] are the trust anchor for the one thing the guest
//! installs that is not Apple's: the SHA-256 of GitHub's tarball of that release tag of
//! `Homebrew/brew`. `provision.sh` refuses a download that does not match. Changing the tag
//! means downloading it, hashing it, and changing both constants in one commit.
//!
//! # What this cannot prove
//!
//! The recipe file is written by this binary and checked again by the guest with the same
//! rules (see the tests). The installed half of the manifest is what the guest said it
//! installed: a record, not a proof. `faber golden verify` (a later step) checks a fresh
//! clone.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use serde::Serialize;

use super::linux_image::{build_network, Binding, BuildError};
use super::vm_images::{golden_name_problem, GOLDEN_FILES, MAX_OPTIONAL_BYTES};
use super::walled_vm::{watch_launch_until, WatchParams};
use crate::wall::model::VmNetwork;

// ---------------------------------------------------------------------------
// What is pinned, and what is embedded
// ---------------------------------------------------------------------------

/// The `Homebrew/brew` release the guest installs: a tagged release, never `HEAD`.
/// It was the latest stable release on 2026-10-21.
pub const HOMEBREW_TAG: &str = "7.0.10";
/// The SHA-256 of `https://github.com/Homebrew/brew/archive/refs/tags/7.0.10.tar.gz`.
/// Measured on 2026-10-21 by downloading it twice, once from `github.com/.../archive/`
/// and once from `codeload.github.com`, which gave the same digest; the tag's commit,
/// from the Git refs API, was 8f10429b56be28a25032b87463fffc95e556b5bc.
pub const HOMEBREW_SHA256: &str =
    "66883a803c0aaf0dc3621e4c294c208ef2fd18192961bf35b2b7a1eb5fd3d187";

/// The guest's provisioner, compiled in so its version always matches this binary.
pub const PROVISION_SH: &str = include_str!("../../launcher/golden/provision.sh");
/// The bootstrap `provision.sh` installs into the image.
pub const BOOTSTRAP_SH: &str = include_str!("../../launcher/golden/bootstrap.sh");
/// Its LaunchDaemon.
pub const GUEST_AGENT_PLIST: &str =
    include_str!("../../launcher/golden/io.github.rejifald.faber.guest-agent.plist");

/// The requirement `provision.sh` holds an Xcode to (Apple's Mac App Store application
/// signing, team 59GAB85EFG). The host runs the same check on a `.app` before it is copied,
/// and a test pins this text to the one in `provision.sh`.
pub const XCODE_REQUIREMENT: &str = "anchor apple generic and identifier \"com.apple.dt.Xcode\" and (certificate leaf[field.1.2.840.113635.100.6.1.9] or certificate leaf[subject.OU] = \"59GAB85EFG\")";

/// The setup disk's volume name: what the guest's wrapper looks for.
pub const SETUP_VOLUME: &str = "FABERSETUP";
/// The build bundle's directory in `_faber_vm`'s home.
pub const BUNDLE_NAME: &str = "golden-build";
/// The console log in the build bundle.
pub const CONSOLE_FILE: &str = "console.log";
/// The manifest's name in the bundle (ADR-0049 §6).
pub const MANIFEST_FILE: &str = "manifest.json";
/// The most of the console read back: the launcher stops writing its log at 16 MiB.
pub const MAX_CONSOLE: u64 = 16 << 20;
/// Expanding Xcode and downloading runtimes take hours.
pub const LAUNCH_TIMEOUT: Duration = Duration::from_secs(4 * 60 * 60);
/// The build VM's size. Xcode's expansion wants the memory and the cores.
pub const CPUS: u32 = 4;
pub const MEMORY_GIB: u32 = 8;
/// How long the wall may take to bind the build network once the VM starts.
pub const BIND_DEADLINE: Duration = super::linux_image::BIND_DEADLINE;
/// How often the wall is re-read while the VM runs.
pub const WALL_POLL: Duration = super::linux_image::WALL_POLL;
/// The most runtimes and extra formulas one recipe may name: a typo guard, not a policy.
const MAX_RUNTIMES: usize = 8;
const MAX_FORMULAS: usize = 32;
/// The longest guest console line: `FABER-GOLDEN-BUILD ` and what follows, in bytes.
pub const MAX_LINE: usize = 200;
/// The guest's tag, which starts every line it reports.
pub const TAG_PREFIX: &str = "FABER-GOLDEN-BUILD ";
/// The platforms `xcodebuild -downloadPlatform` takes here.
pub const PLATFORMS: [&str; 4] = ["iOS", "watchOS", "tvOS", "visionOS"];

/// Free space the build wants beyond twice the Xcode's size: the guest's disk grows by
/// about 30 GiB with Xcode and its runtimes, and ADR-0024's floor must still hold.
const SPACE_MARGIN: u64 = 50 << 30;

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Why the image was not built. No variant carries unsanitised guest bytes.
#[derive(Debug, thiserror::Error)]
pub enum GoldenError {
    /// An argument, the operator's bundle, the Xcode or the store refused.
    #[error("{0}")]
    Refused(String),
    #[error(transparent)]
    Vm(#[from] BuildError),
    #[error("the build VM reported failed step={} (launcher exit {})", shown_step(.step), shown_code(.code))]
    Failed {
        step: Option<String>,
        code: Option<i32>,
    },
    #[error("the build VM reported no result line (launcher exit {})", shown_code(.code))]
    NoResult { code: Option<i32> },
    #[error("the build VM said ok but did not report {0}; refused")]
    Incomplete(&'static str),
    #[error(
        "the VM could not start because two macOS VMs are already running on this host \
         (Apple allows two). Stop one, or wait for a job to finish, and run the build again"
    )]
    TwoMacVms,
    #[error("building the setup disk failed: {0}")]
    SetupDisk(String),
    #[error("preparing the build bundle failed: {0}")]
    Bundle(String),
    #[error("landing the built image failed: {0}")]
    Land(String),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

fn shown_code(code: &Option<i32>) -> String {
    code.map_or_else(|| "?".to_string(), |c| c.to_string())
}

fn shown_step(step: &Option<String>) -> String {
    step.clone().unwrap_or_else(|| "?".to_string())
}

// ---------------------------------------------------------------------------
// The recipe
// ---------------------------------------------------------------------------

/// How the Xcode comes: a `.xip` the guest expands, or an `.app` it copies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XcodeKind {
    Xip,
    App,
}

impl XcodeKind {
    /// `recipe.env`'s `XCODE_KIND`.
    pub fn as_str(self) -> &'static str {
        match self {
            XcodeKind::Xip => "xip",
            XcodeKind::App => "app",
        }
    }

    /// The name it has on the setup disk, whatever the operator's file is called: one the
    /// guest's grammar always accepts.
    pub fn staged_name(self) -> &'static str {
        match self {
            XcodeKind::Xip => "Xcode.xip",
            XcodeKind::App => "Xcode.app",
        }
    }

    /// By the operator's path: `.xip` or `.app`.
    pub fn of_path(path: &Path) -> Option<Self> {
        match path.extension().and_then(|e| e.to_str()) {
            Some("xip") => Some(XcodeKind::Xip),
            Some("app") => Some(XcodeKind::App),
            _ => None,
        }
    }
}

/// One `--runtime`: a platform and, optionally, a version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSpec {
    pub platform: String,
    pub version: Option<String>,
}

impl RuntimeSpec {
    /// `iOS` or `iOS:18.5`, as `RUNTIMES` in `recipe.env` spells it.
    pub fn token(&self) -> String {
        match &self.version {
            Some(v) => format!("{}:{v}", self.platform),
            None => self.platform.clone(),
        }
    }
}

/// `PLATFORM[:VERSION]`, under the rules `provision.sh` enforces: a platform from
/// [`PLATFORMS`], and a version of digits and dots that neither starts nor ends with a dot.
/// Stricter in one respect: no `..`.
pub fn parse_runtime(s: &str) -> Result<RuntimeSpec, String> {
    let (platform, version) = match s.split_once(':') {
        Some((p, v)) => (p, Some(v)),
        None => (s, None),
    };
    if !PLATFORMS.contains(&platform) {
        return Err(format!(
            "{} is not a runtime platform: one of {} (optionally :VERSION)",
            s.escape_debug(),
            PLATFORMS.join(", ")
        ));
    }
    if let Some(v) = version {
        let ok = !v.is_empty()
            && v.len() <= 16
            && !v.starts_with('.')
            && !v.ends_with('.')
            && !v.contains("..")
            && v.bytes().all(|c| c.is_ascii_digit() || c == b'.');
        if !ok {
            return Err(format!(
                "{} is not a runtime version: digits and dots, such as 18.5",
                v.escape_debug()
            ));
        }
    }
    Ok(RuntimeSpec {
        platform: platform.to_string(),
        version: version.map(str::to_string),
    })
}

/// Why `f` is not a Homebrew formula the guest accepts: letters, digits and `@ . _ + -`,
/// not starting with `-` (an option).
pub fn brew_problem(f: &str) -> Option<String> {
    let ok = !f.is_empty()
        && f.len() <= 64
        && !f.starts_with('-')
        && f.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'@' | b'.' | b'_' | b'+' | b'-'));
    (!ok).then(|| {
        format!(
            "{} is not a Homebrew formula name: letters, digits and '@', '.', '_', '+', '-', \
             not starting with '-'",
            f.escape_debug()
        )
    })
}

/// Everything the guest is told to install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    pub kind: XcodeKind,
    pub runtimes: Vec<RuntimeSpec>,
    pub brew_extra: Vec<String>,
}

impl Recipe {
    /// A recipe from the flags. No `--runtime` means the matching iOS runtime. Repeats are
    /// dropped. Every value is checked against what `provision.sh` accepts, so the guest
    /// never meets a `recipe.env` it would refuse.
    pub fn new(kind: XcodeKind, runtimes: &[String], brew: &[String]) -> Result<Self, String> {
        let mut specs: Vec<RuntimeSpec> = Vec::new();
        for r in runtimes {
            let spec = parse_runtime(r)?;
            if !specs.contains(&spec) {
                specs.push(spec);
            }
        }
        if specs.is_empty() {
            specs.push(RuntimeSpec {
                platform: "iOS".into(),
                version: None,
            });
        }
        let mut formulas: Vec<String> = Vec::new();
        for f in brew {
            if let Some(why) = brew_problem(f) {
                return Err(why);
            }
            if !formulas.contains(f) {
                formulas.push(f.clone());
            }
        }
        if specs.len() > MAX_RUNTIMES {
            return Err(format!("at most {MAX_RUNTIMES} runtimes"));
        }
        if formulas.len() > MAX_FORMULAS {
            return Err(format!("at most {MAX_FORMULAS} extra formulas"));
        }
        Ok(Self {
            kind,
            runtimes: specs,
            brew_extra: formulas,
        })
    }

    /// `recipe.env`, in the format `provision.sh` reads.
    pub fn recipe_env(&self) -> String {
        let runtimes: Vec<String> = self.runtimes.iter().map(RuntimeSpec::token).collect();
        format!(
            "# Written by `faber golden build`. provision.sh validates every value.\n\
             XCODE_KIND={}\n\
             XCODE_FILE={}\n\
             RUNTIMES={}\n\
             BREW_EXTRA={}\n\
             HOMEBREW_TAG={HOMEBREW_TAG}\n\
             HOMEBREW_SHA256={HOMEBREW_SHA256}\n",
            self.kind.as_str(),
            self.kind.staged_name(),
            runtimes.join(" "),
            self.brew_extra.join(" "),
        )
    }
}

// ---------------------------------------------------------------------------
// The request
// ---------------------------------------------------------------------------

/// `faber golden …`.
#[derive(Debug, Clone, PartialEq, Eq, clap::Subcommand)]
pub enum GoldenCommand {
    /// Build the golden macOS image from a prepared bundle, walled, and import it
    /// (ADR-0049). Run it with sudo, from your own account, on a host that has run
    /// `sudo faber setup`. It takes hours and one of the host's two macOS VM places.
    Build {
        /// The bundle `faber golden prepare` left (disk, vz-aux, vz-hwmodel,
        /// vz-identifier). It is cloned, never booted or changed.
        bundle: PathBuf,
        /// The image's name in the store, as a profile's `image` names it.
        #[arg(long)]
        name: String,
        /// The Xcode to install: an `Xcode_*.xip` Apple signed, or an `Xcode.app`.
        #[arg(long, value_name = "PATH")]
        xcode: PathBuf,
        /// A simulator runtime to download, such as `iOS` or `iOS:18.5`. Repeatable.
        /// Default: the matching iOS runtime.
        #[arg(long = "runtime", value_name = "PLATFORM[:VERSION]")]
        runtimes: Vec<String>,
        /// A Homebrew formula to add to jq, gh, zstd and git-lfs. Repeatable.
        #[arg(long = "brew", value_name = "FORMULA")]
        brew: Vec<String>,
        /// Replace a different image already stored under that name.
        #[arg(long)]
        replace: bool,
    },
    /// Install macOS offline and boot it in a window for Setup Assistant: the
    /// operator's half of building the golden image (ADR-0049). Run it as
    /// yourself, without sudo, from a desktop session; it prints the exact
    /// `sudo faber golden build …` to run next.
    Prepare(super::golden_prepare::PrepareArgs),
    /// Ask the running daemon to verify a golden image now, and wait for the
    /// result (ADR-0049 §7): one clone boots as a job's would and its checks
    /// run. Only a verified image runs jobs. Run it as yourself, without sudo.
    Verify {
        /// The image's name in the store, as a profile's `image` names it.
        name: String,
    },
}

/// A validated `golden build`.
#[derive(Debug, Clone)]
pub struct GoldenSpec {
    /// The operator's prepared bundle.
    pub prepared: PathBuf,
    /// The operator, whose files the bundle and the Xcode must be.
    pub operator_uid: u32,
    /// The operator's primary group, for the commands that run as the operator.
    pub operator_gid: u32,
    /// The image's name in the store.
    pub name: String,
    /// The operator's Xcode.
    pub xcode: PathBuf,
    /// The name recorded for it in the manifest.
    pub xcode_file: String,
    pub recipe: Recipe,
    pub replace: bool,
    pub times: WatchParams,
}

impl GoldenSpec {
    /// Check the flags. Nothing is read from disk: that is the host's.
    #[allow(clippy::too_many_arguments)] // one flag each, and the operator's ids
    pub fn new(
        bundle: &Path,
        name: &str,
        xcode: &Path,
        runtimes: &[String],
        brew: &[String],
        replace: bool,
        operator_uid: u32,
        operator_gid: u32,
    ) -> Result<Self, String> {
        if let Some(why) = golden_name_problem(name) {
            return Err(why.replace("--golden-name", "--name"));
        }
        if !bundle.is_absolute() {
            return Err("give the prepared bundle as an absolute path".into());
        }
        if !xcode.is_absolute() {
            return Err("give --xcode as an absolute path".into());
        }
        let kind = XcodeKind::of_path(xcode).ok_or_else(|| {
            "--xcode is an Xcode_*.xip or an Xcode.app: its name must end in .xip or .app"
                .to_string()
        })?;
        let recipe = Recipe::new(kind, runtimes, brew)?;
        let file = xcode
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| {
                n.len() <= 100
                    && n.bytes()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
            })
            .unwrap_or(kind.staged_name());
        Ok(Self {
            prepared: bundle.to_path_buf(),
            operator_uid,
            operator_gid,
            name: name.to_string(),
            xcode: xcode.to_path_buf(),
            xcode_file: file.to_string(),
            recipe,
            replace,
            times: WatchParams {
                launch_timeout: LAUNCH_TIMEOUT,
                bind_deadline: BIND_DEADLINE,
                wall_poll: WALL_POLL,
            },
        })
    }
}

/// One command of staging an `.app`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolStep {
    /// Run as the operator (the only account whose rights read their Xcode), not as root.
    pub as_operator: bool,
    pub program: &'static str,
    pub args: Vec<std::ffi::OsString>,
}

/// How an `.app` reaches the setup disk without root ever reading a file the operator
/// controls: an archiver running AS THE OPERATOR streams the bundle to a pipe, so it cannot
/// read anything the operator's account cannot, whatever links are planted in the bundle;
/// root only extracts that stream, with bsdtar's safe defaults (no `-P`, so no absolute
/// paths and no `..`) and without taking the archive's owners or modes, into a fresh 0700
/// directory (`root_copy`). The extracted tree is then walked ([`mac::bundle_problem`]),
/// opened to every account ([`mac::open_to_every_account`]), and the signature requirement
/// is checked on it, the bytes that are used.
pub fn app_stage_steps(src: &Path, root_copy: &Path) -> Vec<ToolStep> {
    vec![
        // How big the operator's own account says it is, to bound what root extracts.
        ToolStep {
            as_operator: true,
            program: "/usr/bin/du",
            args: vec![DU_APPARENT.into(), src.into()],
        },
        ToolStep {
            as_operator: true,
            program: "/usr/bin/tar",
            args: vec![
                "-c".into(),
                "-f".into(),
                "-".into(),
                "-C".into(),
                src.into(),
                ".".into(),
            ],
        },
        ToolStep {
            as_operator: false,
            program: "/usr/bin/tar",
            args: vec![
                "-x".into(),
                "-f".into(),
                "-".into(),
                "-C".into(),
                root_copy.into(),
                "--no-same-owner".into(),
                "--no-same-permissions".into(),
            ],
        },
        ToolStep {
            as_operator: false,
            program: "/usr/bin/codesign",
            args: xcode_codesign_args(root_copy),
        },
    ]
}

/// The mode a staged Xcode's entry gets, whatever the extraction left it: a folder `0755`, a
/// file `0644`, or `0755` when its owner may run it. The guest copies these modes into
/// `/Applications/Xcode.app` as they are, and a job runs as `runner`, which is neither the
/// owner nor in the group. Without this, the bundle's top folder kept the `0700` it was made
/// with, and a whole image had an Xcode only root could enter (ADR-0049 §7, the 2026-10-26
/// toolchain amendment). No set-id or sticky bit, and no write for anyone but the owner.
pub fn shared_mode(mode: u32, is_dir: bool) -> u32 {
    const OWNER_EXEC: u32 = 0o100;
    if is_dir || mode & OWNER_EXEC != 0 {
        0o755
    } else {
        0o644
    }
}

/// The `codesign` arguments that hold an Xcode.app to Apple's requirement
/// ([`XCODE_REQUIREMENT`]). The build runs them on root's copy; `faber golden prepare` runs
/// them up front on the operator's, so a bad Xcode is found before the hours are spent.
pub fn xcode_codesign_args(app: &Path) -> Vec<std::ffi::OsString> {
    vec![
        "--verify".into(),
        "--deep".into(),
        "--strict".into(),
        format!("-R={XCODE_REQUIREMENT}").into(),
        app.into(),
    ]
}

/// The files that go on the setup disk, beside the Xcode.
#[derive(Debug, Clone)]
pub struct SetupFiles {
    /// `(name on the disk, contents)`.
    pub files: Vec<(&'static str, String)>,
}

impl SetupFiles {
    pub fn for_recipe(recipe: &Recipe) -> Self {
        Self {
            files: vec![
                ("provision.sh", PROVISION_SH.to_string()),
                ("recipe.env", recipe.recipe_env()),
                ("bootstrap.sh", BOOTSTRAP_SH.to_string()),
                (
                    "io.github.rejifald.faber.guest-agent.plist",
                    GUEST_AGENT_PLIST.to_string(),
                ),
            ],
        }
    }
}

// ---------------------------------------------------------------------------
// The operator's bundle
// ---------------------------------------------------------------------------

/// What `faber golden prepare` leaves beside the four files: the recipe it prints the build
/// command from. The build never reads, copies or hashes it.
pub const PREPARED_EXTRAS: [&str; 1] = ["recipe.json"];

/// Why the entries of the operator's prepared bundle are not [`GOLDEN_FILES`], plus at most
/// the [`PREPARED_EXTRAS`]. The import accepts an optional manifest; a prepared bundle has
/// none.
pub fn prepared_entries_problem(entries: &[String]) -> Option<String> {
    let missing: Vec<&str> = GOLDEN_FILES
        .iter()
        .copied()
        .filter(|f| !entries.iter().any(|e| e == f))
        .collect();
    let mut extra: Vec<String> = entries
        .iter()
        .filter(|e| !GOLDEN_FILES.contains(&e.as_str()) && !PREPARED_EXTRAS.contains(&e.as_str()))
        .map(|e| e.escape_debug().to_string())
        .collect();
    extra.sort();
    if missing.is_empty() && extra.is_empty() {
        return None;
    }
    let mut msg = format!(
        "a prepared bundle holds exactly {} (and may hold {}), nothing else",
        GOLDEN_FILES.join(", "),
        PREPARED_EXTRAS.join(", ")
    );
    if !missing.is_empty() {
        msg.push_str(&format!("; missing: {}", missing.join(", ")));
    }
    if !extra.is_empty() {
        msg.push_str(&format!("; also there: {}", extra.join(", ")));
    }
    Some(msg)
}

/// Why `name` in the operator's bundle is not a file the build takes: a regular file, one
/// link, the operator's own.
pub fn prepared_file_problem(
    dir: &Path,
    name: &str,
    is_file: bool,
    uid: u32,
    nlink: u64,
    operator_uid: u32,
) -> Option<String> {
    super::fsx::append_problem(&dir.join(name), is_file, uid, nlink, operator_uid)
}

// ---------------------------------------------------------------------------
// The console
// ---------------------------------------------------------------------------

/// A runtime the guest reported installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RuntimeFact {
    pub platform: String,
    pub version: String,
    pub build: String,
}

/// A Homebrew formula the guest reported installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BrewFact {
    pub formula: String,
    pub version: String,
}

/// What the guest said it installed. Guest-reported: a record, not a proof.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
pub struct Facts {
    pub macos_build: String,
    pub xcode_version: String,
    pub xcode_build: String,
    pub runtimes: Vec<RuntimeFact>,
    pub brew: Vec<BrewFact>,
}

const MAX_RUNTIME_FACTS: usize = 64;
const MAX_BREW_FACTS: usize = 512;

/// A build or version token: ASCII letters and digits, `min..=max` bytes.
fn alnum(s: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&s.len()) && s.bytes().all(|c| c.is_ascii_alphanumeric())
}

/// `N`, `N.N`, `N.N.N`, or `N.N.N.N`.
fn dotted_version(s: &str) -> bool {
    let parts: Vec<&str> = s.split('.').collect();
    (1..=4).contains(&parts.len())
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.len() <= 6 && p.bytes().all(|c| c.is_ascii_digit()))
}

/// Xcode's version line: `27.0`, or `27.0 Beta 2`.
fn xcode_version_text(s: &str) -> bool {
    s.len() <= 32
        && s.bytes().next().is_some_and(|c| c.is_ascii_digit())
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b' '))
        && !s.contains("  ")
        && !s.ends_with(' ')
}

/// A formula's version as `brew list --versions` prints it.
fn brew_version(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'+' | b'-'))
}

/// A step name: `[a-z][a-z0-9-]{0,31}`.
fn step_name(s: &str) -> Option<String> {
    let ok = (1..=32).contains(&s.len())
        && s.as_bytes()[0].is_ascii_lowercase()
        && s.bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-');
    ok.then(|| s.to_string())
}

/// The text after the guest's tag if `line` is a well-formed result line: at most
/// [`MAX_LINE`] bytes, printable ASCII, starting with [`TAG_PREFIX`]. Anything else is
/// not the guest's protocol and is ignored.
fn tagged(line: &str) -> Option<&str> {
    if line.len() > MAX_LINE || !line.bytes().all(|c| (0x20..=0x7e).contains(&c)) {
        return None;
    }
    line.strip_prefix(TAG_PREFIX)
}

/// Apply `fact <key>=<value>`'s `kv` to `facts`; whether it was a fact this reads.
fn apply_fact(facts: &mut Facts, kv: &str) -> bool {
    let Some((key, value)) = kv.split_once('=') else {
        return false;
    };
    match key {
        "macos_build" if alnum(value, 3, 12) => facts.macos_build = value.to_string(),
        "xcode_version" if xcode_version_text(value) => facts.xcode_version = value.to_string(),
        "xcode_build" if alnum(value, 3, 16) => facts.xcode_build = value.to_string(),
        "runtime" => {
            let mut it = value.split(' ');
            let (Some(p), Some(v), Some(b), None) = (it.next(), it.next(), it.next(), it.next())
            else {
                return false;
            };
            if !PLATFORMS.contains(&p)
                || !dotted_version(v)
                || !alnum(b, 3, 16)
                || facts.runtimes.len() >= MAX_RUNTIME_FACTS
            {
                return false;
            }
            facts.runtimes.push(RuntimeFact {
                platform: p.to_string(),
                version: v.to_string(),
                build: b.to_string(),
            });
        }
        "brew" => {
            let mut it = value.split(' ');
            let (Some(f), Some(v), None) = (it.next(), it.next(), it.next()) else {
                return false;
            };
            if brew_problem(f).is_some() || !brew_version(v) || facts.brew.len() >= MAX_BREW_FACTS {
                return false;
            }
            facts.brew.push(BrewFact {
                formula: f.to_string(),
                version: v.to_string(),
            });
        }
        _ => return false,
    }
    true
}

/// Judge the build VM's console. `code` is the launcher's exit code. The console is the
/// guest's, so only lines of the exact protocol count, and nothing is echoed that is not
/// matched by a strict pattern:
///
/// - `wall-not-up` outranks everything: the guest fetched nothing and powered off;
/// - then `no-network`: the guest never had an address on the build network, so it could
///   not judge the wall, fetched nothing and powered off;
/// - then any `failed step=`;
/// - success needs the launcher to exit 0, an `ok` line that is the last line of the
///   protocol, and the three facts that name what was built.
pub fn judge_console(code: Option<i32>, console: &[u8]) -> Result<Facts, GoldenError> {
    let text = String::from_utf8_lossy(console);
    let mut facts = Facts::default();
    let mut wall_down = false;
    let mut no_network = false;
    let mut failed: Option<Option<String>> = None;
    let mut ok = false;
    let mut after_ok = false;
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        let Some(rest) = tagged(line) else { continue };
        let recognised = if rest == "wall-not-up" {
            wall_down = true;
            true
        } else if rest == "no-network" {
            no_network = true;
            true
        } else if rest == "ok" {
            if ok {
                after_ok = true;
            }
            ok = true;
            continue;
        } else if let Some(step) = rest.strip_prefix("failed step=") {
            failed = Some(step_name(step));
            true
        } else if let Some(step) = rest.strip_prefix("step=") {
            step_name(step).is_some()
        } else if let Some(kv) = rest.strip_prefix("fact ") {
            apply_fact(&mut facts, kv)
        } else {
            false
        };
        if recognised && ok {
            after_ok = true;
        }
    }
    if wall_down {
        return Err(BuildError::WallNotUp { code }.into());
    }
    if no_network {
        return Err(BuildError::NoGuestNetwork { code }.into());
    }
    if let Some(step) = failed {
        return Err(GoldenError::Failed { step, code });
    }
    if code != Some(0) || !ok || after_ok {
        return Err(GoldenError::NoResult { code });
    }
    for (what, value) in [
        ("macos_build", &facts.macos_build),
        ("xcode_version", &facts.xcode_version),
        ("xcode_build", &facts.xcode_build),
    ] {
        if value.is_empty() {
            return Err(GoldenError::Incomplete(what));
        }
    }
    Ok(facts)
}

/// Whether the launcher's words say Apple refused a third macOS VM
/// (`VZErrorDomain` code 6, as the launcher's `describe` prints it).
pub fn two_macos_vms(said: &str) -> bool {
    said.contains("[VZErrorDomain 6]")
}

// ---------------------------------------------------------------------------
// The manifest
// ---------------------------------------------------------------------------

/// `manifest.json` (ADR-0049 §6): the recipe, which this binary wrote, and what the guest
/// said it installed, which is a record and not a proof.
pub fn manifest_json(
    spec: &GoldenSpec,
    facts: &Facts,
    built_at: u64,
) -> Result<Vec<u8>, GoldenError> {
    let runtimes: Vec<String> = spec
        .recipe
        .runtimes
        .iter()
        .map(RuntimeSpec::token)
        .collect();
    let value = serde_json::json!({
        "schema": 1,
        "recipe": {
            "xcode_source": spec.recipe.kind.as_str(),
            "xcode_file": spec.xcode_file,
            "runtimes": runtimes,
            "brew_extra": spec.recipe.brew_extra,
            "homebrew_tag": HOMEBREW_TAG,
            "homebrew_sha256": HOMEBREW_SHA256,
        },
        "installed": {
            "reported_by": "guest",
            "macos_build": facts.macos_build,
            "xcode_version": facts.xcode_version,
            "xcode_build": facts.xcode_build,
            "runtimes": facts.runtimes,
            "brew": facts.brew,
        },
        "built_at": built_at,
        "faber_version": env!("FABER_VERSION"),
    });
    let bytes = serde_json::to_vec_pretty(&value)
        .map_err(|e| GoldenError::Land(format!("manifest: {e}")))?;
    if bytes.len() as u64 > MAX_OPTIONAL_BYTES {
        return Err(GoldenError::Land(format!(
            "the manifest would be {} bytes, more than the {MAX_OPTIONAL_BYTES} the import takes",
            bytes.len()
        )));
    }
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// The build
// ---------------------------------------------------------------------------

/// How the launcher ended.
#[derive(Debug, Clone, Default)]
pub struct LaunchRun {
    /// Its exit code, `None` when it was killed.
    pub code: Option<i32>,
    /// Its own words (already sanitised) when it did not exit 0.
    pub said: String,
}

/// Every effect of a build. The real one ([`MacHost`]) is macOS only; tests substitute a
/// fake.
pub trait GoldenHost {
    /// `Ok` only when the wall covers `net` right now (see [`super::linux_image::wall_binding`]).
    fn wall_covers(&self, net: &VmNetwork) -> impl Future<Output = Result<Binding, BuildError>>;
    /// The operator's bundle is exactly the four files, the operator's own.
    fn check_prepared(&self, spec: &GoldenSpec) -> impl Future<Output = Result<(), GoldenError>>;
    /// The store has no image of that name, or `--replace` was given.
    fn check_name(&self, spec: &GoldenSpec) -> impl Future<Output = Result<(), GoldenError>>;
    /// The Xcode is what it claims. Returns its size in bytes.
    fn check_xcode(&self, spec: &GoldenSpec) -> impl Future<Output = Result<u64, GoldenError>>;
    /// Build the setup disk in root's staging and return its path.
    fn make_setup_disk(
        &self,
        spec: &GoldenSpec,
        files: &SetupFiles,
        xcode_bytes: u64,
    ) -> impl Future<Output = Result<PathBuf, GoldenError>>;
    /// Clone the operator's bundle into the build bundle in `_faber_vm`'s home; returns it.
    fn prepare_bundle(
        &self,
        spec: &GoldenSpec,
    ) -> impl Future<Output = Result<PathBuf, GoldenError>>;
    /// Boot the bundle with the setup disk attached; wait for the guest to power off, at
    /// most `timeout` (then kill it), or until `stop` is set (then kill it at once).
    fn run_launcher(
        &self,
        bundle: &Path,
        setup_disk: &Path,
        timeout: Duration,
        stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) -> impl Future<Output = Result<LaunchRun, BuildError>>;
    /// The tail of the console log, at most `cap` bytes.
    fn read_console(
        &self,
        bundle: &Path,
        cap: u64,
    ) -> impl Future<Output = Result<Vec<u8>, BuildError>>;
    /// Bring the bundle's four files into root's staging, write `manifest` beside them,
    /// and import the result as `spec.name`. Returns the name.
    fn land(
        &self,
        spec: &GoldenSpec,
        bundle: &Path,
        manifest: &[u8],
        now_unix: u64,
    ) -> impl Future<Output = Result<String, GoldenError>>;
    /// Remove the build bundle and root's staging. Best effort; always called.
    fn cleanup(&self) -> impl Future<Output = ()>;
    /// Resolves when the operator stops the build (SIGINT, SIGTERM, SIGHUP) while the VM
    /// runs: the VM is stopped, nothing lands, and [`GoldenHost::cleanup`] runs. Tests
    /// substitute their own.
    fn interrupt(&self) -> impl Future<Output = ()> {
        super::walled_vm::terminate_signal()
    }
}

/// Build the image and import it. Returns its name. The operator's bundle is never
/// written; on any failure the build bundle and the staging are removed.
pub async fn build<H: GoldenHost>(
    host: &H,
    spec: &GoldenSpec,
    now_unix: u64,
) -> Result<String, GoldenError> {
    // 0. The wall covers the build VM's network, or nothing happens (#236).
    host.wall_covers(&build_network()).await?;
    // 1. Everything that can be refused cheaply is, before any large work.
    host.check_prepared(spec).await?;
    host.check_name(spec).await?;
    let xcode_bytes = host.check_xcode(spec).await?;
    let result = build_in(host, spec, xcode_bytes, now_unix).await;
    host.cleanup().await;
    result
}

async fn build_in<H: GoldenHost>(
    host: &H,
    spec: &GoldenSpec,
    xcode_bytes: u64,
    now_unix: u64,
) -> Result<String, GoldenError> {
    let files = SetupFiles::for_recipe(&spec.recipe);
    let disk = host.make_setup_disk(spec, &files, xcode_bytes).await?;
    let bundle = host.prepare_bundle(spec).await?;
    // Judged again: the setup disk took a long time, and the wall may have changed.
    host.wall_covers(&build_network()).await?;
    let said = Mutex::new(String::new());
    let (bundle_ref, disk_ref, said_ref) = (&bundle, &disk, &said);
    let watched = watch_launch_until(
        &spec.times,
        |net| async move { host.wall_covers(&net).await },
        |timeout, stop| async move {
            let run = host
                .run_launcher(bundle_ref, disk_ref, timeout, stop)
                .await?;
            *said_ref.lock().unwrap_or_else(|e| e.into_inner()) = run.said;
            Ok(run.code)
        },
        host.interrupt(),
    )
    .await;
    // The launcher failing to start at all would otherwise read as a wall that was never
    // seen binding.
    let said = said.into_inner().unwrap_or_else(|e| e.into_inner());
    if two_macos_vms(&said) {
        return Err(GoldenError::TwoMacVms);
    }
    let code = watched?;
    let console = host.read_console(&bundle, MAX_CONSOLE).await?;
    let facts = judge_console(code, &console)?;
    let manifest = manifest_json(spec, &facts, now_unix)?;
    host.land(spec, &bundle, &manifest, now_unix).await
}

// ---------------------------------------------------------------------------
// df and du, read
// ---------------------------------------------------------------------------

/// The free bytes `df -k` reports for the one path it was asked about.
pub fn df_available_bytes(out: &str) -> Option<u64> {
    let line = out.lines().rfind(|l| !l.trim().is_empty())?;
    let kib: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    kib.checked_mul(1024)
}

/// `du`'s flags for an Xcode's size: its files' apparent bytes, in KiB. Not the blocks it
/// takes on disk: an installed Xcode is APFS-compressed (an Xcode 27.1 is 3.9 GiB on disk,
/// 9.6 GiB apparent), and both `tar` and the copy root extracts carry the full bytes.
pub const DU_APPARENT: &str = "-skA";

/// The size `du -skA` reports, in bytes.
pub fn du_bytes(out: &str) -> Option<u64> {
    let kib: u64 = out.split_whitespace().next()?.parse().ok()?;
    kib.checked_mul(1024)
}

/// What a build needs free on the store's volume for an Xcode of `xcode_bytes`.
pub fn space_needed(xcode_bytes: u64) -> u64 {
    xcode_bytes.saturating_mul(2).saturating_add(SPACE_MARGIN)
}

// ---------------------------------------------------------------------------
// The command
// ---------------------------------------------------------------------------

/// Run a `faber golden` command. Blocking only on the build's own awaits: call it from the
/// runtime.
pub async fn run(cmd: GoldenCommand) -> anyhow::Result<()> {
    let cmd = match cmd {
        GoldenCommand::Prepare(args) => return super::golden_prepare::run(args).await,
        GoldenCommand::Verify { name } => return crate::vm::verify::cli(&name).await,
        build => build,
    };
    #[cfg(target_os = "macos")]
    {
        mac::run(cmd).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = cmd;
        anyhow::bail!("the golden macOS image is built on macOS only")
    }
}

// ---------------------------------------------------------------------------
// The real host (macOS)
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
pub use mac::MacHost;

#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::OsString;
    use std::fs::File;
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
    use std::process::{Command, Stdio};

    use anyhow::{anyhow, bail, Context};

    use super::super::accounts::probe_faber_accounts;
    use super::super::exe;
    use super::super::fsx::{self, Account, Dir, ROOT};
    use super::super::linux_image::{
        launcher_path, BUILD_GUEST, BUILD_SLOT, BUILD_SUBNET, STATE_DIR_NAME,
    };
    use super::super::tools::{self, SystemRunner, SAFE_PATH};
    use super::super::vm_images::{apply, canonical, golden_dir, golden_verdict, probe};
    use super::super::walled_vm::mac::{
        clone_or_copy, give, place_dir, root_own_dir, wall_covers, VmHome,
    };
    use super::*;

    /// How long `ditto`, `codesign` and `du` may take on an Xcode.
    const TOOL_LIMIT: Duration = Duration::from_secs(60 * 60);
    /// How long `hdiutil` may take to make the setup disk.
    const HDIUTIL_LIMIT: Duration = Duration::from_secs(2 * 60 * 60);
    /// The largest disk or auxiliary file one bundle member may be: 2 TiB, a sparse VM disk.
    const MAX_DISK: u64 = 2 << 40;
    /// The other bundle members are small.
    const MAX_SMALL: u64 = 1 << 30;
    /// Root's staging in the image store, and the lock that keeps two builds apart.
    const STAGING_NAME: &str = ".golden-build";
    const LOCK_NAME: &str = ".golden-build.lock";
    const SETUP_DISK: &str = "setup.img";

    /// The real [`GoldenHost`]: runs as root in `sudo faber golden build`.
    pub struct MacHost {
        homes: VmHome,
        launcher: PathBuf,
        /// The image store as root walks it (`/private/var/db/faber/images`).
        store: PathBuf,
        /// Held for the life of the build.
        _lock: File,
    }

    impl MacHost {
        /// Take the build lock. A second build at once is refused, so one cannot delete the
        /// other's bundle.
        pub fn new(
            vm: Account,
            faber: Account,
            vm_home: &Path,
            launcher: &Path,
            store: &Path,
        ) -> Result<Self, GoldenError> {
            let lock = std::fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .mode(0o600)
                .open(store.join(LOCK_NAME))?;
            lock.try_lock().map_err(|_| {
                GoldenError::Refused(
                    "another `faber golden build` is running on this host; wait for it".into(),
                )
            })?;
            Ok(Self {
                homes: VmHome {
                    vm,
                    faber,
                    path: vm_home.to_path_buf(),
                },
                launcher: launcher.to_path_buf(),
                store: store.to_path_buf(),
                _lock: lock,
            })
        }

        fn staging(&self) -> PathBuf {
            self.store.join(STAGING_NAME)
        }

        fn bundle_path(&self) -> PathBuf {
            self.homes.path.join(BUNDLE_NAME)
        }
    }

    impl Drop for MacHost {
        /// Release the lock explicitly. A `flock` belongs to the open file description, so
        /// closing this descriptor does not release it while a child forked by another
        /// thread, and not yet exec'd, still holds a copy; unlocking does.
        fn drop(&mut self) {
            let _ = self._lock.unlock();
        }
    }

    /// Run `cmd` as root with nothing from the environment but a safe `PATH`, and return
    /// its output on success.
    fn tool(mut cmd: Command, what: &str, limit: Duration) -> Result<tools::Output, GoldenError> {
        cmd.env_clear()
            .env("PATH", SAFE_PATH)
            .current_dir("/")
            .stdin(Stdio::null());
        let out = tools::output_within(cmd, limit)
            .map_err(|e| GoldenError::SetupDisk(format!("{what}: {e:#}")))?;
        if !out.success {
            return Err(GoldenError::SetupDisk(format!(
                "{what} failed: {}",
                out.sanitized(2000)
            )));
        }
        Ok(out)
    }

    /// A command for `step`, from nothing but a safe environment.
    fn base_cmd(step: &ToolStep) -> Command {
        let bin = Path::new(step.program);
        assert!(bin.is_absolute());
        let mut cmd = Command::new(bin); // absolute: checked above
        cmd.args(&step.args)
            .env_clear()
            .env("PATH", SAFE_PATH)
            .env("HOME", "/var/empty")
            .current_dir("/");
        cmd
    }

    /// Wait for both children, killing both after `limit`.
    fn wait_pair(
        a: &mut std::process::Child,
        b: &mut std::process::Child,
        limit: Duration,
    ) -> Result<(std::process::ExitStatus, std::process::ExitStatus), GoldenError> {
        let deadline = std::time::Instant::now() + limit;
        loop {
            if let (Some(x), Some(y)) = (a.try_wait()?, b.try_wait()?) {
                return Ok((x, y));
            }
            if std::time::Instant::now() >= deadline {
                let _ = a.kill();
                let _ = b.kill();
                let _ = a.wait();
                let _ = b.wait();
                return Err(GoldenError::SetupDisk(
                    "copying the Xcode took too long".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// Start the extracting `tar` for `step`, reading `stdin`.
    pub(super) fn spawn_extractor(
        step: &ToolStep,
        stdin: Stdio,
    ) -> std::io::Result<std::process::Child> {
        base_cmd(step)
            .stdin(stdin)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
    }

    /// Give `root` and every folder and file under it [`super::shared_mode`]. Links are
    /// left alone (a mode on a link means nothing, and chmod would follow it). The tree is
    /// root's, extracted into a folder only root enters, and [`bundle_problem`] has walked it.
    pub fn open_to_every_account(root: &Path) -> std::io::Result<()> {
        let mut stack = vec![root.to_path_buf()];
        while let Some(path) = stack.pop() {
            let meta = std::fs::symlink_metadata(&path)?;
            let ft = meta.file_type();
            if ft.is_symlink() {
                continue;
            }
            if ft.is_dir() {
                for entry in std::fs::read_dir(&path)? {
                    stack.push(entry?.path());
                }
            }
            let mode = super::shared_mode(meta.mode(), ft.is_dir());
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))?;
        }
        Ok(())
    }

    /// Why the tree extracted at `root` is not fit to image: it may hold only directories,
    /// regular files with one link, and symlinks that resolve inside it. A second name for a
    /// file would be another route to something root owns; a device, socket or FIFO has no
    /// place in an app; a link out of the bundle would be followed by whatever reads it
    /// next. Nothing here follows a link to look at what it names; a link is resolved only
    /// to see where it lands.
    pub fn bundle_problem(root: &Path) -> Result<(), String> {
        let canon = std::fs::canonicalize(root).map_err(|e| format!("{}: {e}", root.display()))?;
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let entries = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            for entry in entries {
                let path = entry.map_err(|e| e.to_string())?.path();
                let meta = std::fs::symlink_metadata(&path)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                let ft = meta.file_type();
                let shown = || {
                    path.strip_prefix(root)
                        .unwrap_or(&path)
                        .display()
                        .to_string()
                };
                if ft.is_dir() {
                    stack.push(path);
                } else if ft.is_file() {
                    if meta.nlink() != 1 {
                        return Err(format!("{} has {} links", shown(), meta.nlink()));
                    }
                } else if ft.is_symlink() {
                    let target = std::fs::read_link(&path).map_err(|e| e.to_string())?;
                    let inside = match std::fs::canonicalize(&path) {
                        Ok(real) => real.starts_with(&canon),
                        // Dangling: no way to see where it lands, so only a plain
                        // relative name that never climbs.
                        Err(_) => {
                            target.is_relative()
                                && !target
                                    .components()
                                    .any(|c| matches!(c, std::path::Component::ParentDir))
                        }
                    };
                    if !inside {
                        return Err(format!("{} is a link out of the bundle", shown()));
                    }
                } else {
                    return Err(format!("{} is not a file, folder or link", shown()));
                }
            }
        }
        Ok(())
    }

    /// Run `cmd` as the operator and nobody else ([`crate::setup::tools::drop_to`]): root's supplementary
    /// groups would still read group-readable system files.
    fn as_the_operator(cmd: &mut Command, operator: Account) {
        crate::setup::tools::drop_to(cmd, operator);
    }

    /// How far past the operator's own measure of the app the archive may run: 10%, or 2 GiB
    /// when that is more (headers, extended attributes).
    pub fn extraction_limit(measured: u64) -> u64 {
        measured.saturating_add((measured / 10).max(2 << 30))
    }

    /// Copy `src` to `dst`, at most `limit` bytes: the copy ends with an error as soon as
    /// more has come than that, having written no more than `limit` plus one chunk. Both
    /// ends are dropped by the caller, so the archiver's next write fails and the
    /// extractor sees a truncated archive.
    pub fn copy_bounded(
        src: &mut dyn std::io::Read,
        dst: &mut dyn Write,
        limit: u64,
    ) -> std::io::Result<u64> {
        let mut buf = vec![0u8; 1 << 20];
        let mut total: u64 = 0;
        loop {
            let n = src.read(&mut buf)?;
            if n == 0 {
                return Ok(total);
            }
            total += n as u64;
            if total > limit {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::FileTooLarge,
                    format!("the archive is larger than the {limit} bytes the app measured"),
                ));
            }
            dst.write_all(&buf[..n])?;
        }
    }

    /// Stage the `.app` as [`app_stage_steps`] says, into `content/Xcode.app`.
    fn stage_app(xcode: &Path, content: &Path, operator: Account) -> Result<(), GoldenError> {
        let root_copy = content.join(XcodeKind::App.staged_name());
        std::fs::DirBuilder::new().mode(0o700).create(&root_copy)?;
        let steps = app_stage_steps(xcode, &root_copy);
        // The operator's own measure of the app.
        let mut du = base_cmd(&steps[0]);
        as_the_operator(&mut du, operator);
        let out = du
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .map_err(|e| GoldenError::SetupDisk(format!("du as the operator: {e}")))?;
        let measured = du_bytes(&String::from_utf8_lossy(&out.stdout)).ok_or_else(|| {
            GoldenError::Refused(format!(
                "{} could not be measured with your account's rights",
                xcode.display()
            ))
        })?;
        let limit = extraction_limit(measured);
        let mut archiver = base_cmd(&steps[1]);
        as_the_operator(&mut archiver, operator);
        archiver
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut archiver = archiver
            .spawn()
            .map_err(|e| GoldenError::SetupDisk(format!("tar as the operator: {e}")))?;
        let mut from = archiver.stdout.take().expect("piped");
        let mut extractor = spawn_extractor(&steps[2], Stdio::piped())
            .map_err(|e| GoldenError::SetupDisk(format!("tar: {e}")))?;
        let mut to = extractor.stdin.take().expect("piped");
        // Root relays the archive and counts it; past the limit the relay ends, both pipes
        // close, and the children stop.
        let relay = std::thread::spawn(move || copy_bounded(&mut from, &mut to, limit));
        let waited = wait_pair(&mut archiver, &mut extractor, TOOL_LIMIT);
        let relayed = relay.join();
        let clean_up = || {
            let _ = std::fs::remove_dir_all(&root_copy);
        };
        if let Ok(Err(e)) = &relayed {
            if e.kind() == std::io::ErrorKind::FileTooLarge {
                let _ = archiver.kill();
                let _ = extractor.kill();
                clean_up();
                return Err(GoldenError::Refused(format!(
                    "{} is larger than the {} MiB your account measured it at, plus the margin; \
                     extraction stopped and was removed",
                    xcode.display(),
                    measured >> 20
                )));
            }
        }
        let (made, took) = match waited {
            Ok(done) => done,
            Err(e) => {
                clean_up();
                return Err(e);
            }
        };
        if !made.success() {
            clean_up();
            return Err(GoldenError::Refused(format!(
                "{} could not be read with your account's rights (tar {made})",
                xcode.display()
            )));
        }
        if !took.success() {
            clean_up();
            return Err(GoldenError::SetupDisk(format!(
                "extracting the Xcode failed (tar {took})"
            )));
        }
        bundle_problem(&root_copy).map_err(|why| {
            GoldenError::Refused(format!(
                "{} is not a plain app bundle: {why}",
                xcode.display()
            ))
        })?;
        open_to_every_account(&root_copy).map_err(|e| {
            GoldenError::SetupDisk(format!("opening the staged Xcode to runner: {e}"))
        })?;
        tool(base_cmd(&steps[3]), "codesign", TOOL_LIMIT).map_err(|e| {
            GoldenError::Refused(format!(
                "{} is not a validly signed Apple Xcode: {e}",
                xcode.display()
            ))
        })?;
        Ok(())
    }

    /// The `.xip` at `path`, opened without following a symlink and judged on the
    /// descriptor: a regular file with one link, owned by the operator, exactly what a
    /// prepared bundle's members must be. Root never reads a file the operator does not own,
    /// nor one with another name that is not theirs. Returns it with its length.
    fn open_xip(path: &Path, operator_uid: u32) -> Result<(File, u64), GoldenError> {
        let refuse = |m: String| GoldenError::Refused(format!("{}: {m}", path.display()));
        let (parent, name) = fsx::split(path).map_err(|e| refuse(e.to_string()))?;
        let file = Dir::open(parent)
            .and_then(|d| d.open_file(name))
            .map_err(|e| refuse(format!("{e} (a symlink is refused)")))?;
        let meta = file.metadata()?;
        if let Some(problem) = prepared_file_problem(
            parent,
            name,
            meta.file_type().is_file(),
            meta.uid(),
            meta.nlink(),
            operator_uid,
        ) {
            return Err(GoldenError::Refused(problem));
        }
        Ok((file, meta.len()))
    }

    impl GoldenHost for MacHost {
        async fn wall_covers(&self, net: &VmNetwork) -> Result<Binding, BuildError> {
            wall_covers(net).await
        }

        async fn check_prepared(&self, spec: &GoldenSpec) -> Result<(), GoldenError> {
            let path = spec.prepared.clone();
            let uid = spec.operator_uid;
            tokio::task::spawn_blocking(move || {
                let refuse = |m: String| GoldenError::Refused(format!("{}: {m}", path.display()));
                let dir = fsx::open_leaf_dir(&path)
                    .map_err(|e| refuse(format!("{e} (a symlink is refused)")))?;
                let entries = dir.names().map_err(|e| refuse(e.to_string()))?;
                if let Some(problem) = prepared_entries_problem(&entries) {
                    return Err(refuse(problem));
                }
                for name in GOLDEN_FILES {
                    let file = dir
                        .open_file(name)
                        .map_err(|e| refuse(format!("{name}: {e} (a symlink is refused)")))?;
                    let meta = file.metadata()?;
                    if let Some(problem) = prepared_file_problem(
                        &path,
                        name,
                        meta.file_type().is_file(),
                        meta.uid(),
                        meta.nlink(),
                        uid,
                    ) {
                        return Err(GoldenError::Refused(problem));
                    }
                }
                Ok(())
            })
            .await
            .map_err(|e| GoldenError::Bundle(e.to_string()))?
        }

        async fn check_name(&self, spec: &GoldenSpec) -> Result<(), GoldenError> {
            let at = golden_dir().join(&spec.name);
            if !spec.replace && std::fs::symlink_metadata(&at).is_ok() {
                return Err(GoldenError::Refused(format!(
                    "{} already exists: pass --replace to replace it, or --name to build beside it",
                    at.display()
                )));
            }
            Ok(())
        }

        async fn check_xcode(&self, spec: &GoldenSpec) -> Result<u64, GoldenError> {
            let path = spec.xcode.clone();
            let kind = spec.recipe.kind;
            let operator = spec.operator_uid;
            let store = self.store.clone();
            tokio::task::spawn_blocking(move || {
                let meta = std::fs::symlink_metadata(&path)
                    .map_err(|e| GoldenError::Refused(format!("{}: {e}", path.display())))?;
                if meta.file_type().is_symlink() {
                    return Err(GoldenError::Refused(format!(
                        "{} is a symlink; give the Xcode itself",
                        path.display()
                    )));
                }
                let bytes = match kind {
                    XcodeKind::Xip => open_xip(&path, operator)?.1,
                    XcodeKind::App => {
                        if !meta.file_type().is_dir() {
                            return Err(GoldenError::Refused(format!(
                                "{} is not an Xcode.app folder",
                                path.display()
                            )));
                        }
                        // Sizes only: stat, no read. The signature is checked later, on the
                        // copy root makes from the operator's own copy.
                        let mut du = Command::new("/usr/bin/du");
                        du.arg(DU_APPARENT).arg(&path);
                        let out = tool(du, "du", TOOL_LIMIT)?;
                        du_bytes(&out.stdout).ok_or_else(|| {
                            GoldenError::SetupDisk("du printed no size".to_string())
                        })?
                    }
                };
                let mut df = Command::new("/bin/df");
                df.arg("-k").arg(&store);
                let out = tool(df, "df", Duration::from_secs(60))?;
                let free = df_available_bytes(&out.stdout)
                    .ok_or_else(|| GoldenError::SetupDisk("df printed no free space".into()))?;
                let need = space_needed(bytes);
                if free < need {
                    return Err(GoldenError::Refused(format!(
                        "the volume holding {} has {} GiB free; this build wants about {} GiB \
                         (the Xcode twice, plus room for the guest's growth). Free some space and \
                         run it again",
                        store.display(),
                        free >> 30,
                        need >> 30
                    )));
                }
                Ok(bytes)
            })
            .await
            .map_err(|e| GoldenError::SetupDisk(e.to_string()))?
        }

        async fn make_setup_disk(
            &self,
            spec: &GoldenSpec,
            files: &SetupFiles,
            _xcode_bytes: u64,
        ) -> Result<PathBuf, GoldenError> {
            let staging = self.staging();
            let (xcode, kind) = (spec.xcode.clone(), spec.recipe.kind);
            let operator = Account {
                uid: spec.operator_uid,
                gid: spec.operator_gid,
            };
            let vm_gid = self.homes.vm.gid;
            let files = files.files.clone();
            let work = {
                let staging = staging.clone();
                tokio::task::spawn_blocking(move || -> Result<PathBuf, GoldenError> {
                    // A leftover from a killed build: this run holds the lock, so it is
                    // nobody's now. The directory is root's, in root's store.
                    match std::fs::remove_dir_all(&staging) {
                        Ok(()) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(e) => return Err(e.into()),
                    }
                    // root:_faber_vm 0750: the launcher, running as `_faber_vm`, reaches the
                    // disk through the group, and nobody else enters. The content is staged
                    // in `work`, which only root enters.
                    std::fs::DirBuilder::new().mode(0o750).create(&staging)?;
                    std::os::unix::fs::chown(&staging, Some(0), Some(vm_gid))?;
                    std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o750))?;
                    let work = staging.join("work");
                    std::fs::DirBuilder::new().mode(0o700).create(&work)?;
                    let content = work.join("setup");
                    std::fs::DirBuilder::new().mode(0o700).create(&content)?;
                    let dir = Dir::open_nofollow(&content)?;
                    for (name, text) in &files {
                        let mut f = dir.create_new(name, 0o644)?;
                        f.write_all(text.as_bytes())?;
                        f.sync_all()?;
                        f.set_permissions(std::fs::Permissions::from_mode(0o644))?;
                    }
                    match kind {
                        XcodeKind::Xip => {
                            // Judged on the descriptor that is cloned.
                            let (src, _) = open_xip(&xcode, operator.uid)?;
                            let dst = clone_or_copy(&src, &dir, kind.staged_name(), u64::MAX)?;
                            give(&dst, &dir, kind.staged_name(), ROOT, 0o644)?;
                        }
                        XcodeKind::App => {
                            stage_app(&xcode, &content, operator)?;
                        }
                    }
                    Ok(work)
                })
                .await
                .map_err(|e| GoldenError::SetupDisk(e.to_string()))??
            };
            let disk = staging.join(SETUP_DISK);
            let built = tokio::time::timeout(
                HDIUTIL_LIMIT,
                crate::vm::tools_disk::build_volume(&work.join("setup"), &disk, SETUP_VOLUME),
            )
            .await
            .map_err(|_| GoldenError::SetupDisk("hdiutil took over two hours".into()))?;
            built.map_err(|e| GoldenError::SetupDisk(format!("hdiutil: {e:#}")))?;
            // The content is on the image now: free the copy.
            let _ = std::fs::remove_dir_all(&work);
            let staging_dir = Dir::open_nofollow(&staging)?;
            let file = staging_dir.open_file(SETUP_DISK)?;
            // root:_faber_vm 0440, in a directory only root and `_faber_vm` enter: the launcher
            // reads the disk and cannot write it (`requireToolsDisk` needs only that, and an
            // owner other than itself).
            let to_vm = Account {
                uid: 0,
                gid: self.homes.vm.gid,
            };
            give(&file, &staging_dir, SETUP_DISK, to_vm, 0o440)?;
            Ok(disk)
        }

        async fn prepare_bundle(&self, spec: &GoldenSpec) -> Result<PathBuf, GoldenError> {
            let prepared = spec.prepared.clone();
            let operator = spec.operator_uid;
            let staging = self.staging();
            let path = self.bundle_path();
            let (vm, vm_home, faber) = (self.homes.vm, self.homes.path.clone(), self.homes.faber);
            tokio::task::spawn_blocking(move || -> Result<PathBuf, GoldenError> {
                let homes = VmHome {
                    vm,
                    faber,
                    path: vm_home,
                };
                let home = homes.home()?;
                // Root's own 0700 directory inside the staging, for what is built before
                // it is given away.
                let scratch_path = staging.join("bundle-scratch");
                std::fs::DirBuilder::new()
                    .mode(0o700)
                    .create(&scratch_path)?;
                let scratch = root_own_dir(&scratch_path)?;
                homes.ensure_state_dir(&home, &scratch)?;
                // A leftover from an interrupted build. `remove_dir_all` removes a symlink
                // in its place rather than following it.
                match std::fs::remove_dir_all(&path) {
                    Ok(()) => {}
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(GoldenError::Bundle(format!("{}: {e}", path.display()))),
                }
                let src = fsx::open_leaf_dir(&prepared)
                    .map_err(|e| GoldenError::Bundle(format!("{}: {e}", prepared.display())))?;
                place_dir(&scratch, &home, BUNDLE_NAME, vm, 0o700, |bundle| {
                    for name in GOLDEN_FILES {
                        let file = src
                            .open_file(name)
                            .map_err(|e| BuildError::Bundle(format!("{name}: {e}")))?;
                        let meta = file.metadata()?;
                        if let Some(problem) = prepared_file_problem(
                            &prepared,
                            name,
                            meta.file_type().is_file(),
                            meta.uid(),
                            meta.nlink(),
                            operator,
                        ) {
                            return Err(BuildError::Bundle(problem));
                        }
                        // A clone where the volume allows: the operator's disk costs
                        // nothing until the guest writes to its copy.
                        let cap = if name == "disk" { MAX_DISK } else { MAX_SMALL };
                        let dst = clone_or_copy(&file, bundle, name, cap)?;
                        give(&dst, bundle, name, vm, 0o600)?;
                    }
                    Ok(())
                })?;
                Ok(path)
            })
            .await
            .map_err(|e| GoldenError::Bundle(e.to_string()))?
        }

        async fn run_launcher(
            &self,
            bundle: &Path,
            setup_disk: &Path,
            timeout: Duration,
            stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        ) -> Result<LaunchRun, BuildError> {
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
                "--reserve-ip".into(),
                BUILD_GUEST.to_string().into(),
                "--console-log".into(),
                console.into(),
                "--console-log-max-mib".into(),
                "16".into(),
                "--cpus".into(),
                CPUS.to_string().into(),
                "--memory-gib".into(),
                MEMORY_GIB.to_string().into(),
                "--tools-disk".into(),
                setup_disk.into(),
            ];
            let run = self
                .homes
                .run_launcher(&self.launcher, args, timeout, stop)
                .await?;
            if !run.said.is_empty() {
                println!("  faber-vm said:\n{}", run.said);
            }
            Ok(LaunchRun {
                code: run.code,
                said: run.said,
            })
        }

        async fn read_console(&self, bundle: &Path, cap: u64) -> Result<Vec<u8>, BuildError> {
            self.homes.read_tail(bundle, BUNDLE_NAME, CONSOLE_FILE, cap)
        }

        async fn land(
            &self,
            spec: &GoldenSpec,
            bundle: &Path,
            manifest: &[u8],
            now_unix: u64,
        ) -> Result<String, GoldenError> {
            let (name, replace) = (spec.name.clone(), spec.replace);
            let (vm, vm_home, faber) = (self.homes.vm, self.homes.path.clone(), self.homes.faber);
            let land_path = self.staging().join("land");
            let bundle = bundle.to_path_buf();
            let manifest = manifest.to_vec();
            tokio::task::spawn_blocking(move || -> Result<String, GoldenError> {
                let homes = VmHome {
                    vm,
                    faber,
                    path: vm_home,
                };
                std::fs::DirBuilder::new().mode(0o700).create(&land_path)?;
                let dir = root_own_dir(&land_path)?;
                for file in GOLDEN_FILES {
                    // `_faber_vm`'s own file, one link, never through a symlink; the
                    // guest's disk is judged on the descriptor that is then cloned.
                    let src = homes.bundle_file(&bundle, BUNDLE_NAME, file)?;
                    let cap = if file == "disk" { MAX_DISK } else { MAX_SMALL };
                    let dst = clone_or_copy(&src, &dir, file, cap)?;
                    // The clone carried `_faber_vm`'s ACL and attributes, if any.
                    give(&dst, &dir, file, ROOT, 0o600)?;
                }
                let mut m = dir.create_new(MANIFEST_FILE, 0o600)?;
                m.write_all(&manifest)?;
                m.sync_all()?;
                let canonical_land = canonical(&land_path);
                let g = probe::golden(&canonical_land, Some(&name));
                match golden_verdict(&g, replace) {
                    Ok(None) => Ok(name),
                    Ok(Some(replace)) => apply::import_golden(&g, replace, &now_unix.to_string())
                        .map_err(|e| GoldenError::Land(format!("{e:#}"))),
                    Err(why) => Err(GoldenError::Land(
                        why.replace("--replace-golden", "--replace")
                            .replace("--golden-name", "--name"),
                    )),
                }
            })
            .await
            .map_err(|e| GoldenError::Land(e.to_string()))?
        }

        async fn cleanup(&self) {
            let bundle = self.bundle_path();
            let staging = self.staging();
            let _ = tokio::task::spawn_blocking(move || {
                // `remove_dir_all` removes a symlink in a path's place, never its target.
                let _ = std::fs::remove_dir_all(&bundle);
                let _ = std::fs::remove_dir_all(&staging);
            })
            .await;
        }
    }

    /// `faber golden build`, as root on macOS.
    pub async fn run(cmd: GoldenCommand) -> anyhow::Result<()> {
        use crate::platform::layout::{Layout, Os, SystemLayout};
        use crate::platform::record::current_euid;
        use crate::term::sanitize;

        let GoldenCommand::Build {
            bundle,
            name,
            xcode,
            runtimes,
            brew,
            replace,
        } = cmd
        else {
            bail!("not a golden build");
        };
        exe::trust_running_binary()?;
        if current_euid() != 0 {
            bail!("run with sudo: sudo faber golden build …");
        }
        if crate::platform::layout() != Layout::System {
            bail!(
                "this host is not set up for faber's own accounts (no trusted \
                 /etc/faber/setup.json); run `sudo faber setup` first: the build runs as \
                 _faber_vm behind the VM wall"
            );
        }
        // Who ran sudo: SUDO_UID, never 0, confirmed by the directory service. A root login
        // shell is refused here, as it is for setup.
        let operator = super::super::probe::operator(Os::MacOs, true, &mut SystemRunner)
            .map_err(|e| anyhow!("{e}"))?;
        let spec = GoldenSpec::new(
            &bundle,
            &name,
            &xcode,
            &runtimes,
            &brew,
            replace,
            operator.uid,
            operator.gid,
        )
        .map_err(|e| anyhow!("{e}"))?;
        let found = probe_faber_accounts(Os::MacOs, &mut SystemRunner)?;
        let account = |n: &str| -> anyhow::Result<Account> {
            let u = found
                .users
                .iter()
                .find(|u| u.name == n)
                .ok_or_else(|| anyhow!("no account named {n}: run `sudo faber setup` first"))?;
            Ok(Account {
                uid: u.uid,
                gid: u.gid,
            })
        };
        let faber = account(crate::platform::layout::FABER_ACCOUNT)?;
        let vm = account(crate::platform::layout::FABER_VM_ACCOUNT)?;
        let layout = SystemLayout::for_os(Os::MacOs);
        // Root's own all the way up, walked without following a symlink.
        let store = canonical(&golden_dir());
        fsx::root_dir_chain(&store)
            .with_context(|| format!("preparing {}", golden_dir().display()))?;
        let host = MacHost::new(vm, faber, &layout.vm_home, &launcher_path(&layout), &store)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        println!(
            "building the golden macOS image {name} behind the wall (Xcode, runtimes and \
             Homebrew {HOMEBREW_TAG}); this takes hours and one of the host's two macOS VM \
             places"
        );
        match build(&host, &spec, now).await {
            Ok(stored) => {
                println!(
                    "golden image built and imported as {stored} in {}; your prepared bundle is \
                     untouched",
                    golden_dir().display()
                );
                Ok(())
            }
            Err(e) => {
                eprintln!(
                    "\nERROR: the golden image was not built: {}\n  Your prepared bundle is \
                     unchanged: fix the cause and run the same command again.",
                    sanitize(&e.to_string())
                );
                Err(anyhow!(
                    "building the golden image: {}",
                    sanitize(&e.to_string())
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use super::*;

    // --- the pin and the embedded files ---

    #[test]
    fn the_pin_is_a_release_tag_and_a_sha256() {
        assert!(HOMEBREW_TAG
            .split('.')
            .all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit())));
        assert_eq!(HOMEBREW_TAG.split('.').count(), 3);
        assert_eq!(HOMEBREW_SHA256.len(), 64);
        assert!(HOMEBREW_SHA256
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c)));
    }

    /// The host checks an `.app` against the requirement the guest checks it against: the
    /// text must be the same, or one side would accept what the other refuses.
    #[test]
    fn the_xcode_requirement_is_the_one_provision_sh_holds() {
        let line = PROVISION_SH
            .lines()
            .find(|l| l.trim_start().starts_with("xcode_req="))
            .expect("provision.sh states the requirement");
        let start = line.find('\'').unwrap() + 1;
        let end = line.rfind('\'').unwrap();
        assert_eq!(&line[start..end], XCODE_REQUIREMENT);
    }

    /// The build locks every admin the verification's `admins_locked` counts
    /// (launcher/agent/Sources/Checks.swift): members by name and by GeneratedUID, nested
    /// groups, and accounts whose primary group is admin's. Neither side may read fewer.
    #[test]
    fn the_build_locks_every_admin_the_verification_counts() {
        let step = PROVISION_SH
            .split("begin lock-admins")
            .nth(1)
            .and_then(|rest| rest.split("\nbegin ").next())
            .expect("provision.sh has a lock-admins step");
        for attr in [
            "GroupMembership",
            "GroupMembers",
            "NestedGroups",
            "PrimaryGroupID",
        ] {
            assert!(
                step.contains(&format!(" {attr})")) || step.contains(&format!(" {attr} ")),
                "lock-admins reads {attr}"
            );
        }
        assert!(
            step.contains("[ -n \"$u\" ] || exit 1") && step.contains("[ -n \"$n\" ] || exit 1"),
            "an unresolved member or nested group fails the build"
        );
        assert!(
            step.contains("dscl . -read /Groups/admin RecordName >/dev/null || exit 1")
                && step.contains("dscl . -read \"/Groups/$g\" RecordName >/dev/null || exit 1")
                && step.contains("*\" root \"*) ;; *) exit 1"),
            "a directory read that fails is no answer, never an empty admin set"
        );
    }

    /// `provision.sh` reads exactly these names from its own volume.
    #[test]
    fn the_setup_files_are_the_ones_provision_sh_reads() {
        let files = SetupFiles::for_recipe(&recipe());
        let names: Vec<&str> = files.files.iter().map(|(n, _)| *n).collect();
        assert_eq!(
            names,
            [
                "provision.sh",
                "recipe.env",
                "bootstrap.sh",
                "io.github.rejifald.faber.guest-agent.plist"
            ]
        );
        for n in &names[1..] {
            assert!(PROVISION_SH.contains(n), "provision.sh never reads {n}");
        }
        assert!(PROVISION_SH.contains(XcodeKind::Xip.staged_name().split('.').next().unwrap()));
        assert!(files.files[0].1.starts_with("#!/bin/sh"));
        assert!(files.files[2].1.starts_with("#!/bin/sh"));
        assert!(files.files[3].1.contains("<plist"));
    }

    // --- the recipe, against the rules provision.sh enforces ---

    /// `provision.sh`'s recipe checks, ported: the guest refuses what this refuses.
    fn guest_accepts(env: &str) -> bool {
        let mut kind = "";
        let mut file = "";
        let mut runtimes = "";
        let mut brew = "";
        let mut tag = "";
        let mut sha = "";
        for line in env.lines() {
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(v) = line.strip_prefix("XCODE_KIND=") {
                kind = v;
            } else if let Some(v) = line.strip_prefix("XCODE_FILE=") {
                file = v;
            } else if let Some(v) = line.strip_prefix("RUNTIMES=") {
                runtimes = v;
            } else if let Some(v) = line.strip_prefix("BREW_EXTRA=") {
                brew = v;
            } else if let Some(v) = line.strip_prefix("HOMEBREW_TAG=") {
                tag = v;
            } else if let Some(v) = line.strip_prefix("HOMEBREW_SHA256=") {
                sha = v;
            } else {
                return false;
            }
        }
        let wild = |f: &str, ext: &str| f.starts_with("Xcode") && f.ends_with(ext);
        let kind_ok = match kind {
            "xip" => wild(file, ".xip"),
            "app" => wild(file, ".app"),
            _ => false,
        };
        let file_ok = !file.starts_with('.')
            && file
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'));
        let rt_ok = runtimes.split_whitespace().all(|r| {
            let p = r.split(':').next().unwrap();
            if !PLATFORMS.contains(&p) {
                return false;
            }
            match r.split_once(':') {
                Some((_, v)) => {
                    !v.is_empty()
                        && !v.starts_with('.')
                        && !v.ends_with('.')
                        && v.bytes().all(|c| c.is_ascii_digit() || c == b'.')
                }
                None => true,
            }
        });
        let brew_ok = brew.split_whitespace().all(|f| {
            !f.starts_with('-')
                && f.bytes().all(|c| {
                    c.is_ascii_alphanumeric() || matches!(c, b'@' | b'.' | b'_' | b'+' | b'-')
                })
        });
        let tag_ok = {
            let parts: Vec<&str> = tag.split('.').collect();
            parts.len() == 3
                && parts
                    .iter()
                    .all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()))
        };
        let sha_ok = sha.len() == 64
            && sha
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c));
        kind_ok && file_ok && rt_ok && brew_ok && tag_ok && sha_ok
    }

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|i| i.to_string()).collect()
    }

    fn recipe() -> Recipe {
        Recipe::new(XcodeKind::Xip, &[], &[]).unwrap()
    }

    #[test]
    fn the_default_recipe_is_the_matching_ios_runtime() {
        let r = recipe();
        assert_eq!(r.runtimes.len(), 1);
        assert_eq!(r.runtimes[0].token(), "iOS");
        let env = r.recipe_env();
        assert!(env.contains("RUNTIMES=iOS\n"), "{env}");
        assert!(env.contains("BREW_EXTRA=\n"), "{env}");
        assert!(env.contains("XCODE_FILE=Xcode.xip\n"));
        assert!(env.contains(&format!("HOMEBREW_TAG={HOMEBREW_TAG}\n")));
        assert!(guest_accepts(&env), "{env}");
    }

    #[test]
    fn a_recipe_the_flags_build_is_one_the_guest_accepts() {
        for kind in [XcodeKind::Xip, XcodeKind::App] {
            let r = Recipe::new(
                kind,
                &s(&["iOS:18.5", "watchOS", "tvOS", "visionOS:2.1", "iOS:18.5"]),
                &s(&["ruby", "cocoapods", "python@3.12", "gcc+", "ruby"]),
            )
            .unwrap();
            // Repeats are dropped.
            assert_eq!(r.runtimes.len(), 4);
            assert_eq!(
                r.brew_extra,
                s(&["ruby", "cocoapods", "python@3.12", "gcc+"])
            );
            let env = r.recipe_env();
            assert!(guest_accepts(&env), "{env}");
        }
    }

    #[test]
    fn values_the_guest_would_reject_are_refused_here() {
        for bad in [
            "macOS",
            "ios",
            "iOS:",
            "iOS:.5",
            "iOS:5.",
            "iOS:1..2",
            "iOS:abc",
            "iOS:1:2",
            "iOS 18",
            "",
            ":18",
            "iOS:99999999999999999999",
            "iOS;rm",
            "iOS\n",
        ] {
            assert!(
                Recipe::new(XcodeKind::Xip, &s(&[bad]), &[]).is_err(),
                "{bad:?}"
            );
        }
        for bad in [
            "",
            "-x",
            "--HEAD",
            "a b",
            "a;b",
            "a/b",
            "$(x)",
            "a`b`",
            "a\nb",
            "é",
            "a'b",
            &"x".repeat(65),
        ] {
            assert!(
                Recipe::new(XcodeKind::Xip, &[], &s(&[bad])).is_err(),
                "{bad:?}"
            );
        }
        let many: Vec<String> = (0..40).map(|i| format!("f{i}")).collect();
        assert!(Recipe::new(XcodeKind::Xip, &[], &many).is_err());
        let rts = s(&[
            "iOS:1", "iOS:2", "iOS:3", "iOS:4", "iOS:5", "iOS:6", "iOS:7", "iOS:8", "iOS:9",
        ]);
        assert!(Recipe::new(XcodeKind::Xip, &rts, &[]).is_err());
    }

    #[test]
    fn the_guest_check_port_does_refuse_what_it_should() {
        let good = recipe().recipe_env();
        assert!(guest_accepts(&good));
        for (from, to) in [
            ("XCODE_KIND=xip", "XCODE_KIND=zip"),
            ("XCODE_FILE=Xcode.xip", "XCODE_FILE=Foo.xip"),
            ("RUNTIMES=iOS", "RUNTIMES=iOS:1:2"),
            ("BREW_EXTRA=", "BREW_EXTRA=-x"),
            ("HOMEBREW_TAG=7.0.10", "HOMEBREW_TAG=HEAD"),
            ("HOMEBREW_SHA256=6", "HOMEBREW_SHA256=Z"),
        ] {
            assert!(!guest_accepts(&good.replace(from, to)), "{to}");
        }
    }

    // --- the request ---

    fn spec() -> GoldenSpec {
        GoldenSpec::new(
            Path::new("/srv/op/golden-build/macos-27"),
            "xcode-27.0",
            Path::new("/srv/op/Downloads/Xcode_27.0.xip"),
            &[],
            &[],
            false,
            501,
            20,
        )
        .unwrap()
    }

    #[test]
    fn the_request_is_checked_before_anything_is_read() {
        let ok = spec();
        assert_eq!(ok.recipe.kind, XcodeKind::Xip);
        assert_eq!(ok.xcode_file, "Xcode_27.0.xip");
        assert_eq!(ok.times.launch_timeout, Duration::from_secs(4 * 3600));
        let go = |name: &str, bundle: &str, xcode: &str| {
            GoldenSpec::new(
                Path::new(bundle),
                name,
                Path::new(xcode),
                &[],
                &[],
                false,
                501,
                20,
            )
        };
        let b = "/srv/op/b";
        let x = "/Applications/Xcode.app";
        assert!(go("x", b, x).is_ok());
        // The store's rules: reserved names, the grammar, and a flag named for this command.
        for name in ["linux", "runner", "macos", "..", "A", "-x", ".hidden", ""] {
            assert!(go(name, b, x).is_err(), "{name:?}");
        }
        let e = go("linux", b, x).unwrap_err();
        assert!(e.contains("--name") && !e.contains("--golden-name"), "{e}");
        assert!(go("x", "relative/b", x).is_err());
        assert!(go("x", b, "Xcode.app").is_err());
        assert!(go("x", b, "/Applications/Xcode.dmg").is_err());
        // An odd file name is staged under a plain one and recorded as that.
        let odd = go("x", b, "/tmp/Xcode 27 (1).xip").unwrap();
        assert_eq!(odd.xcode_file, "Xcode.xip");
        assert_eq!(odd.recipe.kind.staged_name(), "Xcode.xip");
    }

    // --- the operator's bundle ---

    #[test]
    fn a_prepared_bundle_is_exactly_the_four_files() {
        let four = s(&GOLDEN_FILES);
        assert_eq!(prepared_entries_problem(&four), None);
        let mut with_manifest = four.clone();
        with_manifest.push("manifest.json".into());
        let e = prepared_entries_problem(&with_manifest).unwrap();
        assert!(e.contains("also there: manifest.json"), "{e}");
        let e = prepared_entries_problem(&four[1..]).unwrap();
        assert!(e.contains("missing: disk"), "{e}");
        let mut stray = four.clone();
        stray.push("disk.raw".into());
        stray.push("faber-clone".into());
        let e = prepared_entries_problem(&stray).unwrap();
        assert!(e.contains("disk.raw") && e.contains("faber-clone"), "{e}");
        let mut weird = four;
        weird.push("a\x1b[2J".into());
        let e = prepared_entries_problem(&weird).unwrap();
        assert!(!e.contains('\x1b'), "{e:?}");
    }

    #[test]
    fn a_bundle_file_must_be_the_operators_regular_unlinked_file() {
        let dir = Path::new("/b");
        let p = |is_file, uid, nlink| prepared_file_problem(dir, "disk", is_file, uid, nlink, 501);
        assert_eq!(p(true, 501, 1), None);
        assert!(p(false, 501, 1).unwrap().contains("not a regular file"));
        assert!(p(true, 0, 1).unwrap().contains("owned by uid 0"));
        assert!(p(true, 502, 1).is_some());
        assert!(p(true, 501, 2).unwrap().contains("2 links"));
    }

    // --- the console ---

    fn line(rest: &str) -> String {
        format!("{TAG_PREFIX}{rest}\n")
    }

    fn good_console() -> String {
        [
            line("step=wall"),
            line("step=xcode"),
            line("step=facts"),
            line("fact macos_build=26A434"),
            line("fact xcode_version=27.0"),
            line("fact xcode_build=18A5123b"),
            line("fact runtime=iOS 27.0 24A5123"),
            line("fact brew=jq 1.8.1"),
            line("fact brew=gh 2.80.0_1"),
            line("ok"),
        ]
        .concat()
    }

    #[test]
    fn an_ok_console_gives_the_facts() {
        let f = judge_console(Some(0), good_console().as_bytes()).unwrap();
        assert_eq!(f.macos_build, "26A434");
        assert_eq!(f.xcode_version, "27.0");
        assert_eq!(f.xcode_build, "18A5123b");
        assert_eq!(
            f.runtimes,
            [RuntimeFact {
                platform: "iOS".into(),
                version: "27.0".into(),
                build: "24A5123".into()
            }]
        );
        assert_eq!(f.brew.len(), 2);
        assert_eq!(f.brew[1].formula, "gh");
        assert_eq!(f.brew[1].version, "2.80.0_1");
    }

    #[test]
    fn crlf_and_noise_between_lines_are_fine() {
        let console = format!(
            "boot noise\r\n{}garbage\x1b[2J\n{}",
            good_console().replace('\n', "\r\n"),
            ""
        );
        assert!(judge_console(Some(0), console.as_bytes()).is_ok());
    }

    #[test]
    fn wall_not_up_outranks_everything() {
        let console = format!("{}{}", good_console(), line("wall-not-up"));
        let e = judge_console(Some(0), console.as_bytes()).unwrap_err();
        assert!(
            matches!(e, GoldenError::Vm(BuildError::WallNotUp { .. })),
            "{e}"
        );
        let console = format!("{}{}", line("failed step=xcode"), line("wall-not-up"));
        assert!(matches!(
            judge_console(Some(0), console.as_bytes()).unwrap_err(),
            GoldenError::Vm(BuildError::WallNotUp { .. })
        ));
    }

    /// A guest that never had an address says so, and is not reported as a leaky wall;
    /// `wall-not-up` still outranks it.
    #[test]
    fn no_network_is_its_own_verdict_below_wall_not_up() {
        let console = format!("{}{}", line("failed step=wall"), line("no-network"));
        let e = judge_console(Some(0), console.as_bytes()).unwrap_err();
        assert!(
            matches!(
                e,
                GoldenError::Vm(BuildError::NoGuestNetwork { code: Some(0) })
            ),
            "{e}"
        );
        assert!(e.to_string().contains("never had an address"), "{e}");
        let console = format!("{}{}", line("no-network"), line("wall-not-up"));
        assert!(matches!(
            judge_console(Some(0), console.as_bytes()).unwrap_err(),
            GoldenError::Vm(BuildError::WallNotUp { .. })
        ));
    }

    #[test]
    fn a_failed_line_fails_the_build_even_beside_ok() {
        let console = format!("{}{}", line("failed step=runtimes"), good_console());
        let e = judge_console(Some(0), console.as_bytes()).unwrap_err();
        match e {
            GoldenError::Failed { step, code } => {
                assert_eq!(step.as_deref(), Some("runtimes"));
                assert_eq!(code, Some(0));
            }
            other => panic!("{other}"),
        }
        // A step name outside the grammar is reported as unknown, not echoed.
        let console = line("failed step=$(id); x");
        let e = judge_console(Some(0), console.as_bytes()).unwrap_err();
        assert!(matches!(e, GoldenError::Failed { step: None, .. }), "{e}");
        assert!(e.to_string().contains("step=?"), "{e}");
        let console = line("failed step=Xcode");
        assert!(matches!(
            judge_console(Some(1), console.as_bytes()).unwrap_err(),
            GoldenError::Failed {
                step: None,
                code: Some(1)
            }
        ));
    }

    #[test]
    fn success_needs_ok_and_a_clean_exit() {
        let no_ok = good_console().replace(&line("ok"), "");
        assert!(matches!(
            judge_console(Some(0), no_ok.as_bytes()).unwrap_err(),
            GoldenError::NoResult { code: Some(0) }
        ));
        assert!(matches!(
            judge_console(Some(1), good_console().as_bytes()).unwrap_err(),
            GoldenError::NoResult { code: Some(1) }
        ));
        assert!(matches!(
            judge_console(None, good_console().as_bytes()).unwrap_err(),
            GoldenError::NoResult { code: None }
        ));
        assert!(matches!(
            judge_console(Some(0), b"").unwrap_err(),
            GoldenError::NoResult { .. }
        ));
    }

    #[test]
    fn ok_must_be_the_last_word_of_the_guest() {
        for after in [line("step=facts"), line("fact brew=jq 1.8"), line("ok")] {
            let console = format!("{}{after}", good_console());
            assert!(
                matches!(
                    judge_console(Some(0), console.as_bytes()).unwrap_err(),
                    GoldenError::NoResult { .. }
                ),
                "{after:?}"
            );
        }
        // Noise that is not the protocol does not count.
        let console = format!("{}{}", good_console(), "login: \nFABER-GOLDEN-BUILD-X ok\n");
        assert!(judge_console(Some(0), console.as_bytes()).is_ok());
    }

    #[test]
    fn ok_without_the_facts_that_name_the_build_is_refused() {
        for drop in [
            "fact macos_build=26A434",
            "fact xcode_version=27.0",
            "fact xcode_build=18A5123b",
        ] {
            let console = good_console().replace(&line(drop), "");
            let e = judge_console(Some(0), console.as_bytes()).unwrap_err();
            assert!(matches!(e, GoldenError::Incomplete(_)), "{drop}: {e}");
        }
    }

    #[test]
    fn oversize_and_non_ascii_lines_are_not_the_protocol() {
        let long = format!("{TAG_PREFIX}fact macos_build={}\n", "A".repeat(300));
        let fine = good_console().replace(&line("fact macos_build=26A434"), "");
        let console = format!("{long}{fine}");
        // The long line is ignored, so the build never learns its macos_build.
        assert!(matches!(
            judge_console(Some(0), console.as_bytes()).unwrap_err(),
            GoldenError::Incomplete("macos_build")
        ));
        // Exactly 200 bytes is still a line; 201 is not.
        let at = |n: usize| {
            let pad = n - TAG_PREFIX.len() - "fact brew=".len() - " 1".len();
            format!("{TAG_PREFIX}fact brew={} 1\n", "a".repeat(pad))
        };
        assert_eq!(at(200).trim_end().len(), 200);
        assert!(tagged(at(200).trim_end()).is_some());
        assert!(tagged(at(201).trim_end()).is_none());
        for bad in [
            "fact macos_build=26A\u{e9}34",
            "ok\u{0}",
            "ok\t",
            "ok\u{7f}",
        ] {
            assert!(tagged(&format!("{TAG_PREFIX}{bad}")).is_none(), "{bad:?}");
        }
        // Invalid UTF-8 and NULs anywhere are fine: those lines are skipped.
        let mut bytes = vec![0xff, 0xfe, 0, b'\n'];
        bytes.extend_from_slice(good_console().as_bytes());
        assert!(judge_console(Some(0), &bytes).is_ok());
    }

    #[test]
    fn facts_are_read_by_strict_patterns_only() {
        let mut f = Facts::default();
        for ok in [
            "macos_build=26A434",
            "xcode_version=27.0",
            "xcode_version=27.0 Beta 2",
            "xcode_build=18A5123b",
            "runtime=iOS 27.0 24A5123",
            "runtime=visionOS 2.1.1 22N5000",
            "brew=git-lfs 3.7.0",
            "brew=python@3.12 3.12.7_1",
            "brew=gcc+ 14.2.0",
        ] {
            assert!(apply_fact(&mut f, ok), "{ok}");
        }
        for bad in [
            "",
            "macos_build=",
            "macos_build=26A 434",
            "macos_build=26A434;rm",
            "macos_build=$(id)",
            "macos_build=\u{1b}[2J",
            "macos_build=AB",
            "xcode_version=beta",
            "xcode_version=27.0  1",
            "xcode_version=27.0 ",
            "xcode_build=",
            "runtime=Linux 1.0 AAA",
            "runtime=iOS 27.0",
            "runtime=iOS 27.0 24A5123 extra",
            "runtime=iOS 27..0 24A5123",
            "runtime=iOS x 24A5123",
            "runtime=iOS 27.0 24A5123;",
            "brew=-x 1.0",
            "brew=jq",
            "brew=jq 1.0 2.0",
            "brew=jq $(id)",
            "brew=j\u{1b}q 1.0",
            "extra=1",
            "macos_build",
        ] {
            let mut g = Facts::default();
            assert!(!apply_fact(&mut g, bad), "{bad:?}");
            assert_eq!(g, Facts::default(), "{bad:?} changed the facts");
        }
        assert_eq!(f.runtimes.len(), 2);
        assert_eq!(f.brew.len(), 3);
    }

    #[test]
    fn hostile_values_never_reach_an_error_message() {
        let hostile = "\u{1b}]0;pwned\u{7}";
        let console = format!(
            "{}{}{}",
            line(&format!("fact macos_build={hostile}")),
            line(&format!("step={hostile}")),
            line(&format!("failed step={hostile}")),
        );
        let e = judge_console(Some(0), console.as_bytes()).unwrap_err();
        assert!(!e.to_string().contains('\u{1b}'), "{e}");
    }

    #[test]
    fn the_flood_of_facts_is_capped() {
        let mut console = String::new();
        for i in 0..2000 {
            console.push_str(&line(&format!("fact brew=f{i} 1.0")));
            console.push_str(&line("fact runtime=iOS 27.0 24A5123"));
        }
        console.push_str(&good_console());
        let f = judge_console(Some(0), console.as_bytes()).unwrap();
        assert_eq!(f.brew.len(), MAX_BREW_FACTS);
        assert_eq!(f.runtimes.len(), MAX_RUNTIME_FACTS);
    }

    #[test]
    fn a_third_macos_vm_is_named_plainly() {
        assert!(two_macos_vms(
            "The maximum supported number of active virtual machines has been reached. [VZErrorDomain 6]"
        ));
        assert!(!two_macos_vms("something failed [VZErrorDomain 3]"));
        assert!(!two_macos_vms(""));
        assert!(GoldenError::TwoMacVms.to_string().contains("two macOS VMs"));
    }

    // --- the manifest ---

    #[test]
    fn the_manifest_has_the_recipe_and_marks_the_installed_half_as_the_guest_s() {
        let facts = judge_console(Some(0), good_console().as_bytes()).unwrap();
        let mut spec = spec();
        spec.recipe =
            Recipe::new(XcodeKind::App, &s(&["iOS:18.5", "watchOS"]), &s(&["ruby"])).unwrap();
        let bytes = manifest_json(&spec, &facts, 1_760_000_000).unwrap();
        assert!(bytes.len() < 4096);
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["schema"], 1);
        assert_eq!(v["built_at"], 1_760_000_000u64);
        assert_eq!(v["faber_version"], env!("FABER_VERSION"));
        let r = &v["recipe"];
        assert_eq!(r["xcode_source"], "app");
        assert_eq!(r["xcode_file"], "Xcode_27.0.xip");
        assert_eq!(r["runtimes"], serde_json::json!(["iOS:18.5", "watchOS"]));
        assert_eq!(r["brew_extra"], serde_json::json!(["ruby"]));
        assert_eq!(r["homebrew_tag"], HOMEBREW_TAG);
        assert_eq!(r["homebrew_sha256"], HOMEBREW_SHA256);
        let i = &v["installed"];
        assert_eq!(i["reported_by"], "guest");
        assert_eq!(i["macos_build"], "26A434");
        assert_eq!(i["xcode_version"], "27.0");
        assert_eq!(i["xcode_build"], "18A5123b");
        assert_eq!(i["runtimes"][0]["platform"], "iOS");
        assert_eq!(i["runtimes"][0]["build"], "24A5123");
        assert_eq!(i["brew"][1]["formula"], "gh");
        // Nothing in it but what these fields hold.
        assert_eq!(v.as_object().unwrap().len(), 5);
        assert_eq!(r.as_object().unwrap().len(), 6);
    }

    #[test]
    fn the_largest_possible_manifest_fits_the_import_s_bound() {
        let mut facts = Facts {
            macos_build: "A".repeat(12),
            xcode_version: "9".repeat(32),
            xcode_build: "A".repeat(16),
            ..Facts::default()
        };
        for _ in 0..MAX_RUNTIME_FACTS {
            facts.runtimes.push(RuntimeFact {
                platform: "visionOS".into(),
                version: "999999.999999.999999.999999".into(),
                build: "A".repeat(16),
            });
        }
        for _ in 0..MAX_BREW_FACTS {
            facts.brew.push(BrewFact {
                formula: "f".repeat(64),
                version: "v".repeat(64),
            });
        }
        let bytes = manifest_json(&spec(), &facts, 1).unwrap();
        assert!((bytes.len() as u64) < MAX_OPTIONAL_BYTES);
    }

    #[test]
    fn df_and_du_are_read_exactly() {
        let df = "Filesystem   1024-blocks      Used Available Capacity iused ifree %iused  Mounted on\n\
                  /dev/disk9s9  1000000000 600000000 300000000    67% 1 2 0%   /System/Volumes/Data\n";
        assert_eq!(df_available_bytes(df), Some(300_000_000 * 1024));
        assert_eq!(df_available_bytes(""), None);
        assert_eq!(df_available_bytes("Filesystem 1 2 3\nx y z\n"), None);
        assert_eq!(df_available_bytes("h\nx 1 2 notanumber\n"), None);
        assert_eq!(
            du_bytes("36700160\t/Applications/Xcode.app\n"),
            Some(36_700_160 * 1024)
        );
        assert_eq!(du_bytes("x"), None);
        assert_eq!(du_bytes(""), None);
        assert_eq!(space_needed(10 << 30), (20 << 30) + SPACE_MARGIN);
        assert_eq!(space_needed(u64::MAX), u64::MAX);
    }

    // --- the build, against a fake host ---

    use std::collections::HashMap;

    #[derive(Default)]
    struct Fake {
        /// How many `wall_covers` calls pass before the wall is refused.
        wall_ok: Option<usize>,
        console: String,
        exit: Option<i32>,
        said: String,
        bound_after: usize,
        repend: bool,
        run_for: Duration,
        refuse_prepared: bool,
        refuse_name: bool,
        refuse_xcode: bool,
        land_fails: bool,
        /// The operator stops the build this long after the launcher starts.
        interrupt_after: Option<Duration>,
        calls: Mutex<Vec<&'static str>>,
        landed: Mutex<HashMap<&'static str, Vec<u8>>>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                console: good_console(),
                exit: Some(0),
                bound_after: 1,
                run_for: Duration::from_secs(5),
                ..Self::default()
            }
        }
        fn called(&self, what: &'static str) {
            self.calls.lock().unwrap().push(what);
        }
        fn calls(&self) -> Vec<&'static str> {
            self.calls.lock().unwrap().clone()
        }
        fn without(&self, what: &'static str) -> Vec<&'static str> {
            self.calls().into_iter().filter(|c| *c != what).collect()
        }
    }

    impl GoldenHost for Fake {
        async fn wall_covers(&self, net: &VmNetwork) -> Result<Binding, BuildError> {
            assert_eq!(net, &build_network());
            self.called("wall");
            let calls = self.calls();
            let asked = calls.iter().filter(|c| **c == "wall").count();
            if self.wall_ok.is_some_and(|ok| asked > ok) {
                return Err(BuildError::Wall("the stored wall has no network 16".into()));
            }
            let Some(at) = calls.iter().position(|c| *c == "launch") else {
                return Ok(Binding::Pending);
            };
            let polls = calls[at..].iter().filter(|c| **c == "wall").count();
            let bound = polls > self.bound_after && !(self.repend && polls > self.bound_after + 2);
            Ok(if bound {
                Binding::Bound
            } else {
                Binding::Pending
            })
        }

        async fn check_prepared(&self, _: &GoldenSpec) -> Result<(), GoldenError> {
            self.called("check-prepared");
            if self.refuse_prepared {
                return Err(GoldenError::Refused("not a prepared bundle".into()));
            }
            Ok(())
        }

        async fn check_name(&self, _: &GoldenSpec) -> Result<(), GoldenError> {
            self.called("check-name");
            if self.refuse_name {
                return Err(GoldenError::Refused("already exists".into()));
            }
            Ok(())
        }

        async fn check_xcode(&self, _: &GoldenSpec) -> Result<u64, GoldenError> {
            self.called("check-xcode");
            if self.refuse_xcode {
                return Err(GoldenError::Refused("not signed".into()));
            }
            Ok(10 << 30)
        }

        async fn make_setup_disk(
            &self,
            spec: &GoldenSpec,
            files: &SetupFiles,
            xcode_bytes: u64,
        ) -> Result<PathBuf, GoldenError> {
            self.called("setup-disk");
            assert_eq!(xcode_bytes, 10 << 30);
            assert_eq!(files.files.len(), 4);
            let env = &files.files[1].1;
            assert_eq!(env, &spec.recipe.recipe_env());
            Ok(PathBuf::from("/staging/setup.img"))
        }

        async fn prepare_bundle(&self, _: &GoldenSpec) -> Result<PathBuf, GoldenError> {
            self.called("bundle");
            Ok(PathBuf::from("/staging/golden-build"))
        }

        async fn run_launcher(
            &self,
            bundle: &Path,
            setup_disk: &Path,
            timeout: Duration,
            stop: Arc<AtomicBool>,
        ) -> Result<LaunchRun, BuildError> {
            self.called("launch");
            assert_eq!(bundle, Path::new("/staging/golden-build"));
            assert_eq!(setup_disk, Path::new("/staging/setup.img"));
            assert_eq!(timeout, Duration::from_secs(4 * 3600));
            let started = tokio::time::Instant::now();
            while started.elapsed() < self.run_for {
                if stop.load(Ordering::SeqCst) {
                    self.called("killed");
                    return Ok(LaunchRun::default());
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Ok(LaunchRun {
                code: self.exit,
                said: self.said.clone(),
            })
        }

        async fn read_console(&self, _: &Path, cap: u64) -> Result<Vec<u8>, BuildError> {
            self.called("console");
            assert_eq!(cap, MAX_CONSOLE);
            Ok(self.console.clone().into_bytes())
        }

        async fn land(
            &self,
            spec: &GoldenSpec,
            _: &Path,
            manifest: &[u8],
            now: u64,
        ) -> Result<String, GoldenError> {
            self.called("land");
            assert_eq!(now, 42);
            if self.land_fails {
                return Err(GoldenError::Land("import refused".into()));
            }
            self.landed
                .lock()
                .unwrap()
                .insert("manifest", manifest.to_vec());
            Ok(spec.name.clone())
        }

        async fn cleanup(&self) {
            self.called("cleanup");
        }

        async fn interrupt(&self) {
            match self.interrupt_after {
                Some(after) => tokio::time::sleep(after).await,
                None => std::future::pending().await,
            }
        }
    }

    fn test_spec() -> GoldenSpec {
        let mut s = spec();
        s.times = WatchParams {
            launch_timeout: Duration::from_secs(4 * 3600),
            bind_deadline: Duration::from_secs(3),
            wall_poll: Duration::from_millis(500),
        };
        s
    }

    #[tokio::test(start_paused = true)]
    async fn a_good_build_lands_with_its_manifest_and_cleans_up() {
        let host = Fake::new();
        let name = build(&host, &test_spec(), 42).await.unwrap();
        assert_eq!(name, "xcode-27.0");
        let calls = host.calls();
        let order: Vec<&str> = calls.iter().copied().filter(|c| *c != "wall").collect();
        assert_eq!(
            order,
            [
                "check-prepared",
                "check-name",
                "check-xcode",
                "setup-disk",
                "bundle",
                "launch",
                "console",
                "land",
                "cleanup"
            ]
        );
        // The wall is read before anything else, and again before the VM boots.
        assert_eq!(calls[0], "wall");
        let boot = calls.iter().position(|c| *c == "launch").unwrap();
        assert_eq!(calls[boot - 1], "wall");
        let landed = host.landed.lock().unwrap();
        let v: serde_json::Value = serde_json::from_slice(&landed["manifest"]).unwrap();
        assert_eq!(v["installed"]["xcode_build"], "18A5123b");
        assert_eq!(v["built_at"], 42);
    }

    /// The wall not green refuses before anything is touched, cleanup included.
    #[tokio::test(start_paused = true)]
    async fn a_wall_that_is_not_green_refuses_before_anything() {
        let mut host = Fake::new();
        host.wall_ok = Some(0);
        let e = build(&host, &test_spec(), 42).await.unwrap_err();
        assert!(matches!(e, GoldenError::Vm(BuildError::Wall(_))), "{e}");
        assert_eq!(host.calls(), ["wall"]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_wall_lost_before_the_boot_stops_after_the_setup_disk_only() {
        let mut host = Fake::new();
        host.wall_ok = Some(1);
        let e = build(&host, &test_spec(), 42).await.unwrap_err();
        assert!(matches!(e, GoldenError::Vm(BuildError::Wall(_))), "{e}");
        let calls = host.calls();
        assert!(!calls.contains(&"launch"));
        assert!(!calls.contains(&"land"));
        assert_eq!(calls.last(), Some(&"cleanup"));
    }

    #[tokio::test(start_paused = true)]
    async fn the_cheap_refusals_come_before_any_large_work() {
        type Setter = fn(&mut Fake);
        let cases: [(&str, Setter); 3] = [
            ("check-prepared", |h| h.refuse_prepared = true),
            ("check-name", |h| h.refuse_name = true),
            ("check-xcode", |h| h.refuse_xcode = true),
        ];
        for (what, set) in cases {
            let mut host = Fake::new();
            set(&mut host);
            let e = build(&host, &test_spec(), 42).await.unwrap_err();
            assert!(matches!(e, GoldenError::Refused(_)), "{what}: {e}");
            let calls = host.without("wall");
            assert_eq!(calls.last(), Some(&what), "{calls:?}");
            for big in ["setup-disk", "bundle", "launch", "land", "cleanup"] {
                assert!(!calls.contains(&big), "{what}: {big} ran");
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_wall_that_goes_away_while_the_vm_runs_kills_it_and_lands_nothing() {
        let mut host = Fake::new();
        host.run_for = Duration::from_secs(600);
        // Four wall reads happen before the launch (open, boot, then two polls bound);
        // the next one is refused.
        host.wall_ok = Some(4);
        let e = build(&host, &test_spec(), 42).await.unwrap_err();
        assert!(matches!(e, GoldenError::Vm(BuildError::WallLost(_))), "{e}");
        let calls = host.calls();
        assert!(calls.contains(&"killed"), "{calls:?}");
        assert!(!calls.contains(&"land"));
        assert!(!calls.contains(&"console"));
        assert_eq!(calls.last(), Some(&"cleanup"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_wall_that_never_binds_kills_the_vm() {
        let mut host = Fake::new();
        host.run_for = Duration::from_secs(600);
        host.bound_after = usize::MAX;
        let e = build(&host, &test_spec(), 42).await.unwrap_err();
        assert!(e.to_string().contains("still not bound"), "{e}");
        assert!(host.calls().contains(&"killed"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_wall_that_goes_back_to_pending_kills_the_vm() {
        let mut host = Fake::new();
        host.run_for = Duration::from_secs(600);
        host.repend = true;
        let e = build(&host, &test_spec(), 42).await.unwrap_err();
        assert!(e.to_string().contains("back to pending"), "{e}");
        assert!(host.calls().contains(&"killed"));
        assert!(!host.calls().contains(&"land"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_vm_that_ends_before_the_wall_was_seen_bound_fails() {
        let mut host = Fake::new();
        host.bound_after = usize::MAX;
        host.run_for = Duration::from_millis(100);
        let e = build(&host, &test_spec(), 42).await.unwrap_err();
        assert!(e.to_string().contains("stopped before"), "{e}");
        assert!(!host.calls().contains(&"land"));
    }

    #[tokio::test(start_paused = true)]
    async fn the_consoles_verdicts_decide_what_lands() {
        // wall-not-up outranks a console that also says ok.
        let mut host = Fake::new();
        host.console = format!("{}{}", good_console(), line("wall-not-up"));
        let e = build(&host, &test_spec(), 42).await.unwrap_err();
        assert!(
            matches!(e, GoldenError::Vm(BuildError::WallNotUp { .. })),
            "{e}"
        );
        assert!(!host.calls().contains(&"land"));
        assert_eq!(host.calls().last(), Some(&"cleanup"));
        // failed
        let mut host = Fake::new();
        host.console = line("failed step=homebrew");
        let e = build(&host, &test_spec(), 42).await.unwrap_err();
        assert!(matches!(e, GoldenError::Failed { .. }), "{e}");
        assert!(!host.calls().contains(&"land"));
        // no ok
        let mut host = Fake::new();
        host.console = good_console().replace(&line("ok"), "");
        let e = build(&host, &test_spec(), 42).await.unwrap_err();
        assert!(matches!(e, GoldenError::NoResult { .. }), "{e}");
        // a launcher that did not exit 0
        let mut host = Fake::new();
        host.exit = Some(1);
        let e = build(&host, &test_spec(), 42).await.unwrap_err();
        assert!(matches!(e, GoldenError::NoResult { code: Some(1) }), "{e}");
        assert!(!host.calls().contains(&"land"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_third_macos_vm_is_reported_plainly_not_as_a_missing_wall() {
        let mut host = Fake::new();
        host.exit = Some(1);
        host.bound_after = usize::MAX;
        host.run_for = Duration::from_millis(100);
        host.said = "VZErrorDomain failure [VZErrorDomain 6]".into();
        let e = build(&host, &test_spec(), 42).await.unwrap_err();
        assert!(matches!(e, GoldenError::TwoMacVms), "{e}");
        assert!(!host.calls().contains(&"land"));
        assert_eq!(host.calls().last(), Some(&"cleanup"));
    }

    /// SIGINT, SIGTERM or SIGHUP while the VM runs: it is stopped, nothing lands, and the
    /// staging and the build bundle are removed.
    #[tokio::test(start_paused = true)]
    async fn a_signal_while_the_vm_runs_stops_it_lands_nothing_and_cleans_up() {
        let mut host = Fake::new();
        host.run_for = Duration::from_secs(600);
        host.interrupt_after = Some(Duration::from_secs(2));
        let e = build(&host, &test_spec(), 42).await.unwrap_err();
        assert!(matches!(e, GoldenError::Vm(BuildError::Interrupted)), "{e}");
        let calls = host.calls();
        assert!(calls.contains(&"killed"), "{calls:?}");
        assert!(!calls.contains(&"land") && !calls.contains(&"console"));
        assert_eq!(calls.last(), Some(&"cleanup"));
    }

    #[test]
    fn an_app_is_archived_by_the_operator_and_checked_where_it_is_used() {
        let src = Path::new("/Applications/Xcode.app");
        let root = Path::new("/store/.golden-build/work/setup/Xcode.app");
        let steps = app_stage_steps(src, root);
        assert_eq!(steps.len(), 4);
        // The only reader of what the operator named is the operator's own account,
        // for the size and for the archive.
        assert!(steps[0].as_operator && steps[0].program == "/usr/bin/du");
        // Apparent bytes: the archive carries them, and a compressed app's blocks undercount.
        assert_eq!(steps[0].args[0], DU_APPARENT);
        assert!(steps[1].as_operator && steps[1].program == "/usr/bin/tar");
        assert_eq!(steps[1].args[0], "-c");
        assert!(steps[1].args.iter().any(|a| a == src.as_os_str()));
        // Root only extracts, never `-P`, and takes neither owners nor modes.
        let x = &steps[2];
        assert!(!x.as_operator && x.program == "/usr/bin/tar");
        assert_eq!(x.args[0], "-x");
        for flag in ["--no-same-owner", "--no-same-permissions"] {
            assert!(x.args.iter().any(|a| a == flag), "{flag}");
        }
        assert!(!x.args.iter().any(|a| a == "-P" || a == "-p"));
        assert!(!x.args.iter().any(|a| a == src.as_os_str()));
        assert!(x.args.iter().any(|a| a == root.as_os_str()));
        // The requirement is checked on the bytes that are used.
        let c = &steps[3];
        assert!(!c.as_operator && c.program == "/usr/bin/codesign");
        assert_eq!(c.args.last().unwrap(), root.as_os_str());
        assert!(c
            .args
            .iter()
            .any(|a| *a == std::ffi::OsString::from(format!("-R={XCODE_REQUIREMENT}"))));
        // Nothing run as root names the operator's path.
        assert!(steps
            .iter()
            .filter(|s| !s.as_operator)
            .all(|s| !s.args.iter().any(|a| a == src.as_os_str())));
    }

    #[test]
    fn a_staged_xcode_is_one_every_account_can_enter_and_read() {
        // The broken image's top folder: made 0700, and extraction never changed it.
        assert_eq!(shared_mode(0o700, true), 0o755);
        // A umask of 027 on the extraction.
        assert_eq!(shared_mode(0o750, true), 0o755);
        assert_eq!(shared_mode(0o640, false), 0o644);
        assert_eq!(shared_mode(0o750, false), 0o755);
        // Nobody but the owner writes, and no set-id or sticky bit survives.
        assert_eq!(shared_mode(0o4777, false), 0o755);
        assert_eq!(shared_mode(0o1777, true), 0o755);
        assert_eq!(shared_mode(0o666, false), 0o644);
    }

    #[tokio::test(start_paused = true)]
    async fn a_failed_import_cleans_up_and_says_so() {
        let mut host = Fake::new();
        host.land_fails = true;
        let e = build(&host, &test_spec(), 42).await.unwrap_err();
        assert!(matches!(e, GoldenError::Land(_)), "{e}");
        assert_eq!(host.calls().last(), Some(&"cleanup"));
    }
}

/// The real host's checks that need no root: the operator's bundle, the name, the lock.
#[cfg(all(test, target_os = "macos"))]
mod host_tests {
    use std::fs::File;
    use std::os::unix::fs::symlink;
    use std::process::{Command, Stdio};

    use super::mac::{bundle_problem, copy_bounded, extraction_limit, spawn_extractor};
    use super::*;
    use crate::setup::fsx::tests::{me, Scratch};

    fn host(s: &Scratch) -> MacHost {
        std::fs::create_dir_all(s.path("store")).unwrap();
        let acct = me(s);
        MacHost::new(
            acct,
            acct,
            &s.path("home"),
            &s.path("faber-vm"),
            &s.path("store"),
        )
        .unwrap()
    }

    fn bundle(s: &Scratch) -> PathBuf {
        let b = s.path("prepared");
        std::fs::create_dir_all(&b).unwrap();
        for f in GOLDEN_FILES {
            std::fs::write(b.join(f), f).unwrap();
        }
        b
    }

    fn spec_for(b: &Path, uid: u32, name: &str, replace: bool) -> GoldenSpec {
        GoldenSpec::new(
            b,
            name,
            Path::new("/Applications/Xcode.app"),
            &[],
            &[],
            replace,
            uid,
            20,
        )
        .unwrap()
    }

    async fn refusal(h: &MacHost, spec: GoldenSpec) -> String {
        match h.check_prepared(&spec).await.unwrap_err() {
            GoldenError::Refused(m) => m,
            other => panic!("{other}"),
        }
    }

    #[tokio::test]
    async fn a_good_prepared_bundle_passes_and_every_oddity_is_refused() {
        let s = Scratch::new("golden-prepared");
        let h = host(&s);
        let uid = me(&s).uid;
        let b = bundle(&s);
        h.check_prepared(&spec_for(&b, uid, "x", false))
            .await
            .unwrap();

        // Owned by someone else than the operator.
        let e = refusal(&h, spec_for(&b, uid + 1, "x", false)).await;
        assert!(e.contains("owned by uid"), "{e}");
        // A stray file, a manifest included, and a missing one.
        std::fs::write(b.join("manifest.json"), "{}").unwrap();
        let e = refusal(&h, spec_for(&b, uid, "x", false)).await;
        assert!(e.contains("also there: manifest.json"), "{e}");
        std::fs::remove_file(b.join("manifest.json")).unwrap();
        std::fs::remove_file(b.join("vz-aux")).unwrap();
        let e = refusal(&h, spec_for(&b, uid, "x", false)).await;
        assert!(e.contains("missing: vz-aux"), "{e}");
        // A symlink in place of a member.
        std::fs::write(s.path("elsewhere"), "x").unwrap();
        symlink(s.path("elsewhere"), b.join("vz-aux")).unwrap();
        let e = refusal(&h, spec_for(&b, uid, "x", false)).await;
        assert!(e.contains("vz-aux") && e.contains("symlink"), "{e}");
        std::fs::remove_file(b.join("vz-aux")).unwrap();
        // A second name for a member.
        std::fs::write(b.join("vz-aux"), "x").unwrap();
        std::fs::hard_link(b.join("vz-aux"), s.path("other-name")).unwrap();
        let e = refusal(&h, spec_for(&b, uid, "x", false)).await;
        assert!(e.contains("2 links"), "{e}");
        // A directory in place of a member.
        std::fs::remove_file(b.join("vz-aux")).unwrap();
        std::fs::create_dir(b.join("vz-aux")).unwrap();
        let e = refusal(&h, spec_for(&b, uid, "x", false)).await;
        assert!(e.contains("vz-aux"), "{e}");
        // The bundle itself a symlink.
        symlink(&b, s.path("link")).unwrap();
        let e = refusal(&h, spec_for(&s.path("link"), uid, "x", false)).await;
        assert!(e.contains("symlink"), "{e}");
        // Not there at all.
        let e = refusal(&h, spec_for(&s.path("nothing"), uid, "x", false)).await;
        assert!(e.contains("nothing"), "{e}");
    }

    async fn xip_refusal(h: &MacHost, xip: &Path, uid: u32) -> String {
        let spec =
            GoldenSpec::new(Path::new("/srv/b"), "x", xip, &[], &[], false, uid, 20).unwrap();
        match h.check_xcode(&spec).await.unwrap_err() {
            GoldenError::Refused(m) => m,
            other => panic!("{other}"),
        }
    }

    /// Root reads only an Xcode the operator owns, as it reads only their bundle.
    #[tokio::test]
    async fn an_xip_the_operator_does_not_own_or_that_has_other_names_is_refused() {
        let s = Scratch::new("golden-xip");
        let h = host(&s);
        let uid = me(&s).uid;
        let xip = s.path("Xcode_27.0.xip");
        std::fs::write(&xip, "x").unwrap();
        // Owned by someone else than the operator.
        let e = xip_refusal(&h, &xip, uid + 1).await;
        assert!(e.contains("owned by uid"), "{e}");
        // A second name for it: could be a file root owns.
        std::fs::hard_link(&xip, s.path("other.xip")).unwrap();
        let e = xip_refusal(&h, &xip, uid).await;
        assert!(e.contains("2 links"), "{e}");
        // A symlink in its place.
        symlink(&xip, s.path("link.xip")).unwrap();
        let e = xip_refusal(&h, &s.path("link.xip"), uid).await;
        assert!(e.contains("symlink"), "{e}");
        // A directory named .xip.
        std::fs::create_dir(s.path("dir.xip")).unwrap();
        let e = xip_refusal(&h, &s.path("dir.xip"), uid).await;
        assert!(e.contains("not a regular file"), "{e}");
    }

    // --- extraction of the operator's archive ---

    /// One ustar entry: `typeflag` is the type byte, `link` the link name.
    fn ustar(name: &str, data: &[u8], typeflag: u8, link: &str) -> Vec<u8> {
        let mut h = [0u8; 512];
        h[..name.len()].copy_from_slice(name.as_bytes());
        h[100..107].copy_from_slice(b"0000644");
        h[108..115].copy_from_slice(b"0000000");
        h[116..123].copy_from_slice(b"0000000");
        h[124..135].copy_from_slice(format!("{:011o}", data.len()).as_bytes());
        h[136..147].copy_from_slice(b"00000000000");
        h[156] = typeflag;
        h[157..157 + link.len()].copy_from_slice(link.as_bytes());
        h[257..263].copy_from_slice(b"ustar\0");
        h[263..265].copy_from_slice(b"00");
        h[148..156].copy_from_slice(b"        ");
        let sum: u32 = h.iter().map(|&b| u32::from(b)).sum();
        h[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        h[155] = b' ';
        let mut out = h.to_vec();
        out.extend_from_slice(data);
        out.resize(512 + data.len().div_ceil(512) * 512, 0);
        out
    }

    fn extract(s: &Scratch, entries: &[Vec<u8>]) -> PathBuf {
        let mut tar: Vec<u8> = entries.concat();
        tar.extend_from_slice(&[0u8; 1024]);
        std::fs::write(s.path("a.tar"), tar).unwrap();
        let dest = s.path("out");
        std::fs::create_dir_all(&dest).unwrap();
        let step = &app_stage_steps(Path::new("/src"), &dest)[2];
        let mut child =
            spawn_extractor(step, Stdio::from(File::open(s.path("a.tar")).unwrap())).unwrap();
        let _ = child.wait();
        dest
    }

    /// The extraction's own result, opened: a fresh 0700 folder (as `stage_app` makes it)
    /// under a umask of 027 ends with every folder 0755 and every file 0644 or 0755, so the
    /// guest's copy is one `runner` can use. A link is kept, and its target untouched.
    #[test]
    fn a_staged_xcode_is_opened_to_every_account_and_its_links_are_left_alone() {
        use std::os::unix::fs::PermissionsExt;
        let s = Scratch::new("golden-open");
        let root = s.path("Xcode.app");
        let dev = root.join("Contents/Developer");
        std::fs::create_dir_all(&dev).unwrap();
        std::fs::write(dev.join("tool"), "x").unwrap();
        std::fs::write(dev.join("data"), "x").unwrap();
        let outside = s.path("outside");
        std::fs::write(&outside, "x").unwrap();
        symlink(&outside, dev.join("link")).unwrap();
        let set = |p: &Path, m: u32| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap()
        };
        set(&outside, 0o600);
        set(&dev.join("tool"), 0o750);
        set(&dev.join("data"), 0o640);
        set(&dev, 0o750);
        set(&root.join("Contents"), 0o750);
        set(&root, 0o700);
        super::mac::open_to_every_account(&root).unwrap();
        let mode = |p: &Path| std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode(&root), 0o755);
        assert_eq!(mode(&root.join("Contents")), 0o755);
        assert_eq!(mode(&dev), 0o755);
        assert_eq!(mode(&dev.join("tool")), 0o755);
        assert_eq!(mode(&dev.join("data")), 0o644);
        assert_eq!(mode(&outside), 0o600, "a link's target is not touched");
    }

    #[test]
    fn an_archive_cannot_write_outside_the_extraction_directory() {
        let s = Scratch::new("golden-tar");
        let abs = format!("/tmp/fbr-golden-abs-{}", std::process::id());
        let dest = extract(
            &s,
            &[
                ustar("ok.txt", b"fine", b'0', ""),
                ustar(
                    &format!("../fbr-escape-{}", std::process::id()),
                    b"x",
                    b'0',
                    "",
                ),
                ustar("a/../../fbr-escape2", b"x", b'0', ""),
                ustar(&abs, b"x", b'0', ""),
            ],
        );
        assert!(!s
            .path(&format!("fbr-escape-{}", std::process::id()))
            .exists());
        assert!(!s.path("fbr-escape2").exists());
        assert!(!Path::new(&abs).exists(), "an absolute entry was written");
        let _ = std::fs::remove_file(&abs);
        // Nothing but the extraction directory's own contents is touched.
        assert!(!dest.join("..").join("fbr-escape2").exists());
    }

    /// CVE-2016-5418: a hard link entry that carries data, aimed at an absolute path or
    /// through a symlinked directory, must write nothing outside the extraction directory.
    #[test]
    fn a_hardlink_entry_with_data_cannot_write_outside_either() {
        let s = Scratch::new("golden-tar-hl");
        let pid = std::process::id();
        let victim = format!("/tmp/fbr-cve-{pid}-victim");
        let outside = format!("/tmp/fbr-cve-{pid}-out");
        std::fs::write(&victim, "orig").unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        extract(
            &s,
            &[
                ustar("ok.txt", b"fine", b'0', ""),
                // A hard link to an absolute path, carrying data.
                ustar("hl", b"PWNED", b'1', &victim),
                // A symlinked directory, then a hard link written through it.
                ustar("lnk", b"", b'2', &outside),
                ustar("lnk/evil", b"PWNED", b'1', "ok.txt"),
                ustar("lnk/evil2", b"PWNED", b'0', ""),
            ],
        );
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "orig");
        assert!(!Path::new(&outside).join("evil").exists());
        assert!(!Path::new(&outside).join("evil2").exists());
        let _ = std::fs::remove_file(&victim);
        let _ = std::fs::remove_dir_all(&outside);
    }

    #[test]
    fn the_relay_stops_at_the_limit() {
        let data = vec![7u8; 5 << 20];
        let mut out = Vec::new();
        let n = copy_bounded(&mut &data[..], &mut out, 10 << 20).unwrap();
        assert_eq!(n, 5 << 20);
        assert_eq!(out.len(), 5 << 20);
        let mut out = Vec::new();
        let e = copy_bounded(&mut &data[..], &mut out, 3 << 20).unwrap_err();
        assert_eq!(e.kind(), std::io::ErrorKind::FileTooLarge);
        // No more than the limit plus the chunk that crossed it was ever written.
        assert!(out.len() as u64 <= 3 << 20);
        // The margin: 10%, or 2 GiB when that is more.
        assert_eq!(extraction_limit(100 << 30), (100 << 30) + (10 << 30));
        assert_eq!(extraction_limit(1 << 30), (1 << 30) + (2 << 30));
        assert_eq!(extraction_limit(u64::MAX), u64::MAX);
    }

    #[test]
    fn an_archive_s_device_entry_is_not_made_or_is_refused_by_the_walk() {
        let s = Scratch::new("golden-tar-dev");
        // A character device, major 1 minor 3 (like /dev/null).
        let dest = extract(&s, &[ustar("null", b"", b'3', "")]);
        let node = dest.join("null");
        if std::fs::symlink_metadata(&node).is_ok() {
            assert!(bundle_problem(&dest).unwrap_err().contains("not a file"));
        }
    }

    #[test]
    fn the_walk_refuses_whatever_is_not_a_plain_bundle() {
        let s = Scratch::new("golden-walk");
        let root = s.path("Xcode.app");
        std::fs::create_dir_all(root.join("Contents/Frameworks/F.framework/Versions/A")).unwrap();
        std::fs::write(root.join("Contents/Info.plist"), "x").unwrap();
        symlink(
            "A",
            root.join("Contents/Frameworks/F.framework/Versions/Current"),
        )
        .unwrap();
        symlink("../Info.plist", root.join("Contents/Frameworks/link")).unwrap();
        symlink("nowhere", root.join("dangling")).unwrap();
        bundle_problem(&root).unwrap();

        let refused = |what: &str, make: &dyn Fn(&Path)| {
            let r = s.path(&format!("r-{what}"));
            std::fs::create_dir_all(&r).unwrap();
            make(&r);
            let e = bundle_problem(&r).unwrap_err();
            assert!(!e.is_empty(), "{what}");
        };
        refused("abs", &|r| symlink("/etc", r.join("l")).unwrap());
        refused("up", &|r| symlink("../..", r.join("l")).unwrap());
        refused("dangling-up", &|r| symlink("../nope", r.join("l")).unwrap());
        refused("hardlink", &|r| {
            std::fs::write(r.join("f"), "x").unwrap();
            std::fs::hard_link(r.join("f"), r.join("g")).unwrap();
        });
        refused("fifo", &|r| {
            assert!(Command::new("/usr/bin/mkfifo")
                .arg(r.join("p"))
                .status()
                .unwrap()
                .success());
        });
        // A chain that is inside lexically and out for real: d/e/s -> ../a, and
        // t -> d/e/s/../../.. reaches above the bundle.
        refused("chain", &|r| {
            std::fs::create_dir_all(r.join("d/e")).unwrap();
            std::fs::create_dir_all(r.join("d/a")).unwrap();
            symlink("../a", r.join("d/e/s")).unwrap();
            symlink("d/e/s/../../..", r.join("t")).unwrap();
        });
    }

    #[tokio::test]
    async fn a_name_nobody_has_stored_passes_with_or_without_replace() {
        let s = Scratch::new("golden-name");
        let h = host(&s);
        let b = s.path("prepared");
        let name = format!("fbr-test-{}-never-stored", std::process::id());
        h.check_name(&spec_for(&b, 501, &name, false))
            .await
            .unwrap();
        h.check_name(&spec_for(&b, 501, &name, true)).await.unwrap();
    }

    #[test]
    fn two_builds_at_once_are_refused() {
        let s = Scratch::new("golden-lock");
        let first = host(&s);
        let acct = me(&s);
        let again = || {
            MacHost::new(
                acct,
                acct,
                &s.path("home"),
                &s.path("faber-vm"),
                &s.path("store"),
            )
        };
        match again() {
            Err(GoldenError::Refused(m)) => assert!(m.contains("another"), "{m}"),
            Err(other) => panic!("{other}"),
            Ok(_) => panic!("a second build took the lock"),
        }
        drop(first);
        assert!(again().is_ok());
    }
}
