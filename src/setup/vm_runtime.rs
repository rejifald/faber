//! What `sudo faber setup` installs for faber's VMs (ADR-0026), macOS only:
//!
//! 1. **The launcher**, `faber-vm` in [`LAUNCHER_DIR`], `_faber`'s own 0755
//!    like the daemon copy, so a release can swap it. Not in `D`: the
//!    supervisor runs it as `_faber_vm`, which cannot traverse `D`
//!    (`_faber:_faber` 0750). Its directory is `_faber`'s 0755, and every
//!    directory above it must be root's and writable by nobody else, or the
//!    plan refuses. Root copies it and never runs it: its `--version` is asked
//!    by running the staged copy as `_faber` ([`super::tools::run_as`]), and
//!    must be this binary's stamp. A launcher a release before the move left
//!    at `D/bin/faber-vm` (and its `.prev`) is removed.
//!
//!    The pinned Docker CLI the daemon runs sits with the launcher in the plan,
//!    at [`crate::platform::layout::DOCKER_CLI`], root's 0755
//!    ([`super::docker_cli`]); it is installed with the binaries, before
//!    anything is drained, since it downloads.
//! 2. **`_faber_vm`'s directories** in its home: `run/` (`_faber_vm:_faber`
//!    0710, so `_faber` reaches the supervisor's socket and the Linux VM's
//!    `docker.sock` and lists nothing), `state/`, `clones/` and `linux/`
//!    (0700). The home itself is `_faber_vm:_faber` 0710 for the same reason
//!    ([`super::plan::dir_specs`]).
//! 3. **The tools-disk cache**, [`TOOLS_DISKS`]: `_faber`'s 0755, outside
//!    `_faber`'s state directory, so `_faber_vm` can read what it attaches
//!    without any right to `_faber`'s home (the constant's docs say why).
//! 4. **The image store**, [`IMAGE_STORE`], root's 0755 all the way up, and
//!    the imports into it ([`super::vm_images`]). It is the supervisor's
//!    `--image-dir`: golden images and the Linux image `linux` sit side by
//!    side in it, each at `IMAGE_STORE/<name>`.
//! 5. **The network plan**, `/etc/faber/wall/plan.json`: sixteen networks,
//!    one per launcher slot ([`network_plan`]). Slots 0 to 13 are macOS job
//!    VMs, slot 14 the Linux engine (claim `linux-engine`, image `linux`),
//!    and slot 15 the throwaway VM that builds the Linux image, exactly as
//!    [`crate::setup::linux_image::build_network`] gives it: no profile, so no
//!    grant reaches it, and its builder boots it only once the stored set
//!    holds it in that shape and the checker's verdict for that set is green.
//! 6. **The stored rules**, `/etc/faber/wall/stored.json`, rendered from the
//!    plan and `/etc/faber/grants.toml` through the one function `sudo faber
//!    grant` stores with ([`crate::wall::grant_cmd::render_wall`]), and the
//!    verdict directory, `/var/db/faber/wall`.
//! 7. **Two LaunchDaemons**: the anchor checker, root running the CLI copy as
//!    `faber wall-daemon`, and the VM supervisor, `_faber_vm` running the
//!    launcher's `supervise` with `AbandonProcessGroup`
//!    ([`crate::platform::service::render_wall_plist`],
//!    [`crate::platform::service::render_supervisor_plist`]).
//!
//! Each is a plan [`Step`], from read-only probes, so `--dry-run` shows all of
//! it and a re-run on a set-up host plans nothing and restarts nothing. A
//! daemon is restarted only when what it runs changed: its plist, or for the
//! supervisor the launcher binary or the network plan it reads once at start,
//! or for the checker the stored rules or the CLI copy it runs. A restart
//! boots it out and bootstraps it again, never `kickstart -k`, which leaves
//! launchd pinned to the replaced binary (#479). A supervisor restart stops
//! no VM: its children are in process groups of their own, and launchd leaves
//! them.
//!
//! # What lists which profile
//!
//! Slots 0 to 13 are macOS VMs and list every `backend = "macos-vm"` profile
//! of `config.toml`; slot 14 is the Linux VM and lists every `backend =
//! "docker"` profile, since a Docker grant covers the whole Linux VM. The
//! names come from the same read of `config.toml` as `sudo faber grant`'s
//! ([`crate::wall::grant_cmd::ConfigFacts`]): `_faber`'s file, as data, which
//! can at most move root's grants between the VMs a profile already runs in.
//!
//! When this run migrates or seeds the config, the plan and the rules are
//! rendered after it is in place, and the plan says so; the next run then
//! finds them as they should be.

use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};

use crate::platform::layout::{
    Os, SystemLayout, FABER_ACCOUNT, FABER_OPERATOR_GROUP, FABER_VM_ACCOUNT, IMAGE_STORE,
    TOOLS_DISKS,
};
use crate::platform::record::Account;
use crate::platform::service::{self, SupervisorArgs};
use crate::wall::grant_cmd::{render_wall, ConfigFacts};
use crate::wall::grants::Grants;
use crate::wall::model::{Ipv4Net, NetworkPlan, VmNetwork, VmSlot};
use crate::wall::paths;
use crate::wall::stored::StoredWall;

use super::exe::{self, PathFacts};
use super::plan::{
    self, step, unknown, Action, BinProbe, DirProbe, FileProbe, HostState, Owner, Section,
    ServiceProbe, Status, Step,
};
use super::vm_images::{self, GoldenProbe, TarProbe};

/// The launcher slots macOS VMs use.
pub const MAC_SLOTS: std::ops::RangeInclusive<u8> = 0..=13;
/// The Linux VM's launcher slot.
pub const LINUX_SLOT: u8 = 14;

/// The names a launcher may have beside the running binary: the installed
/// name, and the release asset's.
pub const LAUNCHER_NAMES: [&str; 2] = ["faber-vm", "faber-launcher-aarch64-apple-darwin"];

/// The names the guest agent may have in the launcher's directory: the
/// installed name, and the release asset's (`update.rs`'s `AGENT_ASSET_NAME`).
pub const AGENT_NAMES: [&str; 2] = ["faber-agent", "faber-agent-aarch64-apple-darwin"];

/// The new `sudo faber setup` options.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct VmOpts {
    /// `--launcher PATH`: the launcher to install. Without it, a regular file
    /// named one of [`LAUNCHER_NAMES`] beside the running binary.
    pub launcher: Option<PathBuf>,
    /// `--golden-image PATH`: a macOS bundle to import.
    pub golden_image: Option<PathBuf>,
    /// `--golden-name NAME`: its name in the store. Default: the bundle
    /// directory's name.
    pub golden_name: Option<String>,
    /// `--replace-golden`: replace a different image of the same name.
    pub replace_golden: bool,
    /// `--runner-image-tar PATH`: a `docker save` archive to keep.
    pub runner_image_tar: Option<PathBuf>,
}

/// One of `_faber_vm`'s directories in its home.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmDir {
    /// The supervisor's socket and the Linux VM's `docker.sock`.
    Run,
    /// Slot locks and claim markers.
    State,
    /// The macOS clones.
    Clones,
    /// The Linux VM's clone of its image.
    Linux,
}

impl VmDir {
    pub const ALL: [VmDir; 4] = [VmDir::Run, VmDir::State, VmDir::Clones, VmDir::Linux];

    pub fn name(self) -> &'static str {
        match self {
            VmDir::Run => "run",
            VmDir::State => "state",
            VmDir::Clones => "clones",
            VmDir::Linux => "linux",
        }
    }

    pub fn owner(self) -> Owner {
        match self {
            VmDir::Run => Owner::FaberVmFaberGroup,
            _ => Owner::FaberVm,
        }
    }

    pub fn mode(self) -> u32 {
        match self {
            VmDir::Run => 0o710,
            _ => 0o700,
        }
    }
}

/// Where a launcher to install comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LauncherSource {
    /// None given and none beside this binary.
    None,
    /// A regular file, its SHA-256, and the chain from it to `/`, which the
    /// plan judges with the same rule as the running binary.
    Found {
        path: PathBuf,
        sha256: String,
        chain: Vec<PathFacts>,
    },
    Refused {
        path: PathBuf,
        why: String,
    },
}

/// A root-owned file's bytes, as found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileBytes {
    Missing,
    Bytes(Vec<u8>),
    Unknown(String),
}

/// Everything the VM steps need to know. Gathered by read-only probes on
/// macOS ([`probe`]); empty elsewhere, where none of it is planned.
#[derive(Debug, Clone, PartialEq)]
pub struct VmRuntimeProbe {
    pub launcher_source: LauncherSource,
    pub launcher: BinProbe,
    /// The launcher's directory, [`launcher_dir`].
    pub launcher_dir: DirProbe,
    /// Each directory above it that exists, from `/` down, canonical:
    /// [`fsx::root_dir_chain`](super::fsx::root_dir_chain) creates the rest
    /// root's 0755.
    pub launcher_parents: Result<Vec<PathFacts>, String>,
    /// Which of [`old_launchers`] exist: a release before the launcher moved
    /// out of `D` left them.
    pub old_launchers: Vec<PathBuf>,
    /// The guest agent to install: one of [`AGENT_NAMES`] in the launcher's
    /// directory, judged like the launcher.
    pub agent_source: LauncherSource,
    /// The installed agent, [`agent_path`].
    pub agent: BinProbe,
    pub vm_dirs: Vec<(VmDir, DirProbe)>,
    pub tools_dir: DirProbe,
    /// [`IMAGE_STORE`].
    pub image_dirs: Vec<(PathBuf, DirProbe)>,
    pub wall_dir: DirProbe,
    pub verdict_dir: DirProbe,
    pub plan_json: FileBytes,
    pub stored_json: FileBytes,
    /// `/etc/faber/grants.toml`'s text, `None` when there is none.
    pub grants: Result<Option<String>, String>,
    pub config: Result<ConfigFacts, String>,
    pub host_addrs: Result<Vec<Ipv4Addr>, String>,
    pub anchor: ServiceProbe,
    pub supervisor: ServiceProbe,
    /// [`service::WALL_LOG`] and [`service::SUPERVISOR_LOG`], in that order.
    pub logs: Vec<(PathBuf, FileProbe)>,
    pub golden: Option<GoldenProbe>,
    pub runner_tar: Option<TarProbe>,
    /// The pinned Docker CLI the daemon runs ([`super::docker_cli`]).
    pub docker_cli: super::docker_cli::DockerCliProbe,
}

impl VmRuntimeProbe {
    /// A host with none of it, and nothing known: what a non-macOS probe
    /// returns, and the fixtures' fresh host.
    pub fn nothing() -> Self {
        Self {
            launcher_source: LauncherSource::None,
            launcher: BinProbe::Missing,
            launcher_dir: DirProbe::Missing,
            launcher_parents: Ok(Vec::new()),
            old_launchers: Vec::new(),
            agent_source: LauncherSource::None,
            agent: BinProbe::Missing,
            vm_dirs: VmDir::ALL.iter().map(|d| (*d, DirProbe::Missing)).collect(),
            tools_dir: DirProbe::Missing,
            image_dirs: image_dirs()
                .into_iter()
                .map(|p| (p, DirProbe::Missing))
                .collect(),
            wall_dir: DirProbe::Missing,
            verdict_dir: DirProbe::Missing,
            plan_json: FileBytes::Missing,
            stored_json: FileBytes::Missing,
            grants: Ok(None),
            config: Ok(ConfigFacts::default()),
            host_addrs: Ok(Vec::new()),
            anchor: ServiceProbe {
                installed: None,
                loaded: false,
                stuck: None,
            },
            supervisor: ServiceProbe {
                installed: None,
                loaded: false,
                stuck: None,
            },
            logs: log_paths()
                .into_iter()
                .map(|p| (p, FileProbe::Missing))
                .collect(),
            golden: None,
            runner_tar: None,
            docker_cli: super::docker_cli::DockerCliProbe::nothing(),
        }
    }
}

/// The two LaunchDaemons' logs: the wall's, then the supervisor's.
pub fn log_paths() -> Vec<PathBuf> {
    vec![
        PathBuf::from(service::WALL_LOG),
        PathBuf::from(service::SUPERVISOR_LOG),
    ]
}

/// A log's step: created empty when missing, so launchd's open of it works
/// whichever account it opens it as; fixed when its owner or mode is off.
fn log_step(
    s: &HostState,
    path: &Path,
    probe: &FileProbe,
    owner: Account,
    shown: &str,
    prior: &[u32],
    action: VmAction,
) -> Step {
    let subject = path.display().to_string();
    let remove = format!("`sudo rm \"{}\"`", path.display());
    match probe {
        FileProbe::Missing => step(
            sec(),
            Status::WillCreate,
            subject,
            format!("empty, {shown}"),
        )
        .doing(Action::Vm(action)),
        FileProbe::File { uid, gid, mode } => {
            if !prior.contains(uid) {
                step(
                    sec(),
                    Status::Refused,
                    subject,
                    format!(
                        "owned by uid {uid}, not by the account setup gives it to: something \
                         else created it. Remove it ({remove}) and re-run"
                    ),
                )
            } else if (*uid, *gid, mode & 0o7777) == (owner.uid, owner.gid, 0o640) {
                step(sec(), Status::Ok, subject, shown.to_string())
            } else {
                step(
                    sec(),
                    Status::WillChange,
                    subject,
                    format!("{uid}:{gid} {:o} -> {shown}", mode & 0o7777),
                )
                .doing(Action::Vm(action))
            }
        }
        FileProbe::NotRegular(what) => step(
            sec(),
            Status::Refused,
            subject,
            format!("{what} where a log file belongs. Remove it ({remove}) and re-run"),
        ),
        FileProbe::Unknown(e) => unknown(s, sec(), subject, e),
    }
}

/// [`IMAGE_STORE`]: golden images live directly in it.
pub fn image_dirs() -> Vec<PathBuf> {
    vec![PathBuf::from(IMAGE_STORE)]
}

/// A change the VM steps make. Each is idempotent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VmAction {
    /// Create or fix the launcher's directory, [`launcher_dir`].
    EnsureLauncherDir,
    /// Copy the launcher into [`launcher_dir`], after its staged copy, run as
    /// `_faber`, reports this binary's version.
    InstallLauncher,
    /// Remove [`old_launchers`], never following a symlink.
    RemoveOldLauncher,
    /// Copy the guest agent to [`agent_path`], after its staged copy, run as
    /// `_faber`, reports this binary's version.
    InstallAgent,
    /// Create or fix one of `_faber_vm`'s directories.
    EnsureVmDir(VmDir),
    /// Create or fix [`TOOLS_DISKS`].
    EnsureToolsDir,
    /// Create or fix the image store and its `macos/`.
    EnsureImageStore,
    /// Write `/etc/faber/wall/plan.json`.
    WritePlan,
    /// Write `/etc/faber/wall/stored.json`.
    WriteStored,
    /// Create or fix `/var/db/faber/wall`.
    EnsureVerdictDir,
    /// Import `--golden-image`; `replace` a different bundle of that name.
    ImportGolden { replace: bool },
    /// Copy `--runner-image-tar` into the store.
    ImportRunnerTar,
    /// Install and start the anchor checker's LaunchDaemon; with `restart`,
    /// boot it out and bootstrap it again
    /// ([`crate::platform::service::launchd::reload`]).
    InstallAnchor { restart: bool },
    /// The same for the VM supervisor's.
    InstallSupervisor { restart: bool },
    /// Create the supervisor's (`supervisor`) or the anchor's log file, so it
    /// exists before launchd opens it.
    EnsureLog { supervisor: bool },
    /// Download, check and install the pinned Docker CLI the daemon runs
    /// ([`super::docker_cli`]). Applied with the binaries, before anything is
    /// drained, not with the rest of these steps: it downloads.
    InstallDockerCli,
}

/// The network plan for `config`: job slot `n` (0 to 14) on network `n + 1`,
/// `192.168.(240 + n).0/24`, gateway `.1` (vmnet's gateway is the subnet's
/// second address), guest `.2` (the launcher's reservation default, the
/// third); then the build network, as
/// [`crate::setup::linux_image::build_network`] gives it (network 16,
/// slot 15, `192.168.239.0/24`), which neither id nor subnet collides with.
/// Pure.
pub fn network_plan(config: &ConfigFacts) -> NetworkPlan {
    let mut networks: Vec<VmNetwork> = MAC_SLOTS
        .chain(std::iter::once(LINUX_SLOT))
        .map(|slot| {
            let third = 240 + slot;
            let profiles = if slot == LINUX_SLOT {
                config.docker_profiles.clone()
            } else {
                config.vm_profiles.clone()
            };
            VmNetwork {
                id: slot + 1,
                gateway: Ipv4Addr::new(192, 168, third, 1),
                subnet: Ipv4Net::new(Ipv4Addr::new(192, 168, third, 0), 24)
                    .expect("a /24 on its network address"),
                slots: vec![VmSlot {
                    id: slot,
                    addr: Ipv4Addr::new(192, 168, third, 2),
                    profiles,
                }],
            }
        })
        .collect();
    networks.push(super::linux_image::build_network());
    NetworkPlan { networks }
}

/// The plan as setup writes it: pretty JSON and a final newline.
pub fn plan_bytes(plan: &NetworkPlan) -> Vec<u8> {
    let mut out = serde_json::to_vec_pretty(plan).expect("a plan always serializes");
    out.push(b'\n');
    out
}

/// Each of the host's own addresses that falls in a planned subnet, with the
/// network: a VM there could not tell the host from a guest. The network's own
/// gateway is not one: vmnet puts it on the bridge of a VM running there, so a
/// host with faber's VMs up holds it, and the wall binds that network by it. A
/// gateway held by any other interface is the wall's to refuse (two holders,
/// `crate::wall::model::bind`).
pub fn host_overlaps(plan: &NetworkPlan, addrs: &[Ipv4Addr]) -> Vec<(Ipv4Addr, Ipv4Net)> {
    plan.networks
        .iter()
        .flat_map(|n| {
            addrs
                .iter()
                .filter(move |a| n.subnet.contains(**a) && **a != n.gateway)
                .map(move |a| (*a, n.subnet))
        })
        .collect()
}

/// The supervisor's arguments on `layout`, serving `faber_uid`.
pub fn supervisor_args(faber_uid: u32) -> SupervisorArgs {
    SupervisorArgs {
        peer_uid: faber_uid,
        image_dir: vm_images::golden_dir(),
        tools_dir: PathBuf::from(TOOLS_DISKS),
        network_plan: PathBuf::from(paths::PLAN_CANONICAL),
    }
}

/// Where the launcher goes: `linux_image`'s answer, which builds with the same
/// binary, and is [`SystemLayout::launcher_bin`] (`/var/db/faber/bin/faber-vm`).
pub fn launcher_path(layout: &SystemLayout) -> PathBuf {
    super::linux_image::launcher_path(layout)
}

/// Where the guest agent goes: `D/bin/faber-agent`, the file the updater
/// replaces (`UpdatePaths::agent`) and every tools disk is built from. In `D`,
/// unlike the launcher: only `_faber` reads it, and it is never run on the host
/// but to ask its version.
pub fn agent_path(layout: &SystemLayout) -> PathBuf {
    layout.bin_dir.join("faber-agent")
}

/// The launcher's directory: [`launcher_path`]'s parent, [`LAUNCHER_DIR`] on
/// macOS.
///
/// [`LAUNCHER_DIR`]: crate::platform::layout::LAUNCHER_DIR
pub fn launcher_dir(layout: &SystemLayout) -> PathBuf {
    launcher_path(layout)
        .parent()
        .expect("the launcher path has a parent")
        .to_path_buf()
}

/// Where a release before the move kept the launcher, in `D/bin`: the
/// launcher and its `.prev`. The updater's own staged copy there is left; no
/// release reads it, and uninstall removes it with `D`.
pub fn old_launchers(layout: &SystemLayout) -> Vec<PathBuf> {
    ["faber-vm", "faber-vm.prev"]
        .iter()
        .map(|n| layout.bin_dir.join(n))
        .filter(|p| *p != launcher_path(layout))
        .collect()
}

/// Why the directories above the launcher's (`chain`, the ones that exist) are
/// not fit for it: each must be root's and writable by nobody else. `_faber_vm`
/// executes the launcher, so whoever could rename a directory on that path
/// could choose what it runs. Pure.
pub fn launcher_parents_problem(chain: &[PathFacts]) -> Option<String> {
    const S_IFMT: u32 = 0o170_000;
    const S_IFDIR: u32 = 0o040_000;
    chain.iter().find_map(|f| {
        super::fsx::root_dir_problem(&f.path, f.mode & S_IFMT == S_IFDIR, f.uid, f.mode).or_else(
            || {
                // `_faber_vm` is neither the owner nor in the group of a
                // root directory: it reaches the launcher through "other".
                (f.mode & 0o001 == 0).then(|| {
                    format!(
                        "{} is not searchable by other accounts (mode {:o}), so {FABER_VM_ACCOUNT} \
                         could not reach the launcher",
                        f.path.display(),
                        f.mode & 0o7777
                    )
                })
            },
        )
    })
}

/// Whether the agent's `--version` output is this binary's stamp: `faber-agent
/// <version> protocol=<n>`, the line the updater binds to a release.
pub fn agent_version_problem(stdout: &str, version: &str) -> Option<String> {
    let want = format!("faber-agent {version}");
    let line = stdout.trim();
    let ok = line
        .strip_prefix(want.as_str())
        .and_then(|rest| rest.strip_prefix(" protocol="))
        .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()));
    (!ok).then(|| {
        format!(
            "it reports {:?}, not \"{want} protocol=<n>\": install the agent from the same \
             release as this binary",
            super::tools::sanitize_child_output(line, 200)
        )
    })
}

/// Whether the launcher's `--version` output is this binary's stamp.
pub fn launcher_version_problem(stdout: &str, version: &str) -> Option<String> {
    let want = format!("faber-vm {version}");
    (stdout.trim() != want).then(|| {
        format!(
            "it reports {:?}, not {want:?}: install the launcher from the same release as this \
             binary",
            super::tools::sanitize_child_output(stdout.trim(), 200)
        )
    })
}

/// The stored rules for the plan, from the grants and config probed. Errors
/// are refusals, in the words the plan prints.
pub fn desired_stored(
    plan: &NetworkPlan,
    grants: Option<&str>,
    config: &ConfigFacts,
) -> Result<(StoredWall, Vec<String>), String> {
    let grants = match grants {
        None => Grants::default(),
        Some(text) => Grants::parse(text).map_err(|e| format!("{}: {e}", paths::GRANTS))?,
    };
    let r = render_wall(&grants, config, Some(plan)).map_err(|e| format!("{e:#}"))?;
    let stored = r
        .stored
        .ok_or_else(|| "rendering with a plan gave no rules".to_string())?;
    Ok((stored, r.warnings))
}

fn sec() -> Section {
    Section::VmRuntime
}

/// A directory's step: `want` owner and mode, `prior` the owners an existing
/// one may already have.
#[allow(clippy::too_many_arguments)]
fn dir_step(
    s: &HostState,
    path: &Path,
    probe: &DirProbe,
    want: Account,
    mode: u32,
    prior: &[u32],
    shown: &str,
    action: VmAction,
) -> Step {
    let subject = path.display().to_string();
    match probe {
        DirProbe::Missing => {
            step(sec(), Status::WillCreate, subject, shown.to_string()).doing(Action::Vm(action))
        }
        DirProbe::Dir { uid, gid, mode: m } => {
            if !prior.contains(uid) {
                step(
                    sec(),
                    Status::Refused,
                    subject,
                    format!(
                        "owned by uid {uid}, not by the account setup gives it to: something \
                         else put it there. If nothing in it is yours, remove it (`sudo rm -rf \
                         \"{}\"`) and re-run",
                        path.display()
                    ),
                )
            } else if (*uid, *gid, m & 0o7777) == (want.uid, want.gid, mode) {
                step(sec(), Status::Ok, subject, shown.to_string())
            } else {
                step(
                    sec(),
                    Status::WillChange,
                    subject,
                    format!("{uid}:{gid} {:o} -> {shown}", m & 0o7777),
                )
                .doing(Action::Vm(action))
            }
        }
        DirProbe::NotADir(what) => step(
            sec(),
            Status::Refused,
            subject,
            format!("{what} where a directory belongs; move it aside and re-run"),
        ),
        DirProbe::Unknown(e) => unknown(s, sec(), subject, e),
    }
}

/// Whether the daemon step keeps a daemon copy newer than this binary: the
/// launcher installed beside it is then kept too, so the two stay one
/// release.
fn keeps_newer_daemon(s: &HostState) -> bool {
    match &s.daemon_bin {
        BinProbe::File {
            sha256,
            version: Some(v),
            ..
        } => {
            !s.opts.replace_daemon
                && s.exe_sha256.as_deref() != Some(sha256.as_str())
                && crate::update::is_newer(&super::marker::version_chars(v), &s.version)
        }
        _ => false,
    }
}

/// The launcher's directory: `_faber`'s 0755, under directories only root can
/// change.
fn launcher_dir_step(s: &HostState, faber: Account) -> Step {
    let dir = launcher_dir(&s.layout);
    let subject = dir.display().to_string();
    match &s.vm_runtime.launcher_parents {
        Err(e) => return unknown(s, sec(), subject, e),
        Ok(chain) => {
            if let Some(problem) = launcher_parents_problem(chain) {
                return step(
                    sec(),
                    Status::Refused,
                    subject,
                    format!(
                        "{problem}. Every directory above the launcher's must be root's and \
                         writable by nobody else: {FABER_VM_ACCOUNT} runs the launcher, and \
                         whoever could rename one of them could choose what it runs. Fix it \
                         (`sudo chown root:wheel`, `sudo chmod go-w`) and re-run"
                    ),
                );
            }
        }
    }
    dir_step(
        s,
        &dir,
        &s.vm_runtime.launcher_dir,
        faber,
        0o755,
        &[0, faber.uid],
        &format!(
            "{FABER_ACCOUNT}:{FABER_ACCOUNT} 755, under directories only root can change: the \
             supervisor ({FABER_VM_ACCOUNT}) runs the launcher in it, and a release swaps it \
             there"
        ),
        VmAction::EnsureLauncherDir,
    )
    // The security property stays in the default view (ADR-0050).
    .brief("only root can change the directories above it")
}

/// A release binary setup installs as `_faber`'s 0755 beside the daemon's
/// own copies, so a release can swap it: the launcher, and the guest agent.
struct Companion<'a> {
    /// Where it is installed.
    dest: PathBuf,
    /// Its owner and group as setup writes it: its directory's.
    owner: String,
    /// What `--version` names it.
    program: &'static str,
    /// "the launcher", "the guest agent".
    what: &'static str,
    source: &'a LauncherSource,
    installed: &'a BinProbe,
    action: VmAction,
    /// Why a host without one is only warned, and how to give it one.
    missing: String,
    /// How to reinstall one that is not `_faber`'s 0755.
    reinstall: &'static str,
}

fn launcher_step(s: &HostState, ids: plan::Ids) -> Step {
    let v = &s.vm_runtime;
    companion_step(
        s,
        ids,
        Companion {
            dest: launcher_path(&s.layout),
            owner: format!("{FABER_ACCOUNT}:{FABER_ACCOUNT}"),
            program: "faber-vm",
            what: "the launcher",
            source: &v.launcher_source,
            installed: &v.launcher,
            action: VmAction::InstallLauncher,
            missing: format!(
                "warning: no launcher to install: none beside this binary ({}) and no \
                 --launcher PATH. VM jobs cannot run until it is installed; re-run setup with it",
                LAUNCHER_NAMES.join(" or ")
            ),
            reinstall: "pass --launcher PATH to reinstall it",
        },
    )
}

/// The guest agent (#260): the file every tools disk is built from, at the
/// path the updater replaces ([`agent_path`]). It is the release asset found
/// in the launcher's directory: the one `--launcher` names, else beside this
/// binary.
fn agent_step(s: &HostState, ids: plan::Ids) -> Step {
    let v = &s.vm_runtime;
    companion_step(
        s,
        ids,
        Companion {
            dest: agent_path(&s.layout),
            owner: format!("{FABER_ACCOUNT}:{FABER_OPERATOR_GROUP}"),
            program: "faber-agent",
            what: "the guest agent",
            source: &v.agent_source,
            installed: &v.agent,
            action: VmAction::InstallAgent,
            missing: format!(
                "warning: no guest agent to install: none named {} in the launcher's directory. \
                 macOS VM jobs cannot build their tools disk until it is installed; re-run setup \
                 with the release's assets in one directory",
                AGENT_NAMES.join(" or ")
            ),
            reinstall: "re-run setup with the release's faber-agent beside the launcher",
        },
    )
}

fn companion_step(s: &HostState, ids: plan::Ids, c: Companion<'_>) -> Step {
    let subject = c.dest.display().to_string();
    let what = c.what;
    let (faber, vm) = (ids.faber, ids.vm);
    // `_faber`'s, 0755, with either of the groups setup or the updater gives
    // a file in its directory: neither can write it.
    let owned = |uid: &u32, gid: &u32, mode: &u32| {
        *uid == faber.uid && [faber.gid, ids.operator_gid].contains(gid) && mode & 0o7777 == 0o755
    };
    let owner = &c.owner;
    let check = format!(
        "its staged copy is run as {FABER_ACCOUNT} (never as root) and must report {} {}",
        c.program, s.version
    );
    match (c.source, c.installed) {
        (_, BinProbe::NotRegular) => step(
            sec(),
            Status::Refused,
            subject,
            "not a regular file; move it aside and re-run",
        ),
        (_, BinProbe::Unknown(e)) => unknown(s, sec(), subject, e),
        (LauncherSource::Refused { path, why }, _) => step(
            sec(),
            Status::Refused,
            subject,
            format!("{what} to install, {}, {why}", path.display()),
        ),
        (
            LauncherSource::Found {
                path,
                sha256,
                chain,
            },
            installed,
        ) => {
            if let Err(why) = exe::verdict(chain, &plan::untrusted(s, Some((faber, vm)))) {
                return step(
                    sec(),
                    Status::Refused,
                    subject,
                    format!(
                        "{what} to install, {}: {why}. Root copies only a file faber's own \
                         accounts could not have replaced",
                        path.display()
                    ),
                );
            }
            match installed {
                BinProbe::File {
                    sha256: have,
                    uid,
                    gid,
                    mode,
                    ..
                } if have == sha256 && owned(uid, gid, mode) => step(
                    sec(),
                    Status::Ok,
                    subject,
                    format!("{}, {owner} 0755", path.display()),
                ),
                BinProbe::File { .. } if keeps_newer_daemon(s) => step(
                    sec(),
                    Status::Ok,
                    subject,
                    "kept with the newer daemon copy: they are one release",
                ),
                BinProbe::Missing => step(
                    sec(),
                    Status::WillCreate,
                    subject,
                    format!(
                        "copied from {}, {owner} 0755 so a release can swap it; {check}",
                        path.display()
                    ),
                )
                .doing(Action::Vm(c.action)),
                _ => step(
                    sec(),
                    Status::WillChange,
                    subject,
                    format!("replaced from {}; {check}", path.display()),
                )
                .doing(Action::Vm(c.action)),
            }
        }
        (LauncherSource::None, BinProbe::File { uid, gid, mode, .. }) => {
            if owned(uid, gid, mode) {
                step(
                    sec(),
                    Status::Ok,
                    subject,
                    format!("kept: {FABER_ACCOUNT}'s, 0755 (none new given)"),
                )
            } else {
                step(
                    sec(),
                    Status::Refused,
                    subject,
                    format!(
                        "{uid}:{gid} {:o}, not {FABER_ACCOUNT}'s 0755: {}",
                        mode & 0o7777,
                        c.reinstall
                    ),
                )
            }
        }
        (LauncherSource::None, BinProbe::Missing) => {
            step(sec(), Status::Skipped, subject, c.missing)
        }
    }
}

/// Whether `steps` holds `action`.
fn has(steps: &[Step], action: &VmAction) -> bool {
    steps
        .iter()
        .any(|st| st.action == Some(Action::Vm(action.clone())))
}

/// A LaunchDaemon's step: installed and started, restarted when `triggered`
/// names what changed under it.
fn daemon_step(
    subject: &str,
    rendered: &str,
    probe: &ServiceProbe,
    triggered: Option<&str>,
    what: &str,
    make: fn(bool) -> VmAction,
) -> Step {
    let changed = probe.installed.as_deref() != Some(rendered);
    let start = "plutil -lint, then launchctl enable, bootstrap and kickstart";
    match (&probe.installed, probe.loaded, changed, triggered) {
        (None, _, _, _) => step(
            sec(),
            Status::WillCreate,
            subject,
            format!("root:wheel 0644: {what}; {start}"),
        )
        .doing(Action::Vm(make(false))),
        (Some(_), loaded, true, _) => step(
            sec(),
            Status::WillChange,
            subject,
            if loaded {
                format!("rewritten: {what}; launchctl bootout, then {start}")
            } else {
                format!("rewritten: {what}; {start}")
            },
        )
        .doing(Action::Vm(make(loaded))),
        (Some(_), false, false, _) => step(
            sec(),
            Status::WillChange,
            subject,
            format!("installed but not running: {start}"),
        )
        .doing(Action::Vm(make(false))),
        (Some(_), true, false, Some(why)) => step(
            sec(),
            Status::WillChange,
            subject,
            format!("restarted (bootout + bootstrap): {why}"),
        )
        .doing(Action::Vm(make(true))),
        (Some(_), true, false, None) => match &probe.stuck {
            Some(why) => step(
                sec(),
                Status::WillChange,
                subject,
                format!(
                    "loaded, but launchd does not run it ({why}): launchctl bootout, then \
                     {start}"
                ),
            )
            .doing(Action::Vm(make(true))),
            None => step(sec(), Status::Ok, subject, format!("running: {what}")),
        },
    }
}

/// The VM steps, appended to `steps` (macOS only). `ids` are both accounts'
/// planned ids; without them the accounts are already refused, and nothing
/// here is planned.
pub fn plan_steps(s: &HostState, ids: Option<plan::Ids>, steps: &mut Vec<Step>) {
    if s.os != Os::MacOs {
        return;
    }
    let Some(ids) = ids else {
        return;
    };
    let (faber, vm) = (ids.faber, ids.vm);
    let v = &s.vm_runtime;
    let own = |owner: Owner| plan::owner_account(owner, ids);
    // 1. The launcher: its directory, then the launcher, then the old copies
    // in `D/bin`, once the new one is in place. 1b. The pinned Docker CLI the
    // daemon runs (ADR-0026, amended 2026-10-21).
    let mut out = vec![
        launcher_dir_step(s, faber),
        launcher_step(s, ids),
        agent_step(s, ids),
        super::docker_cli::plan_step(s, &v.docker_cli, &super::docker_cli::PIN),
    ];
    out.extend(super::docker_cli::plugin_dir_steps(s, &v.docker_cli));
    for old in &v.old_launchers {
        out.push(
            step(
                sec(),
                Status::WillChange,
                old.display().to_string(),
                format!(
                    "removed: the launcher now lives in {}, since {FABER_VM_ACCOUNT} runs it and \
                     cannot enter {}",
                    launcher_dir(&s.layout).display(),
                    s.layout.state_dir.display()
                ),
            )
            .doing(Action::Vm(VmAction::RemoveOldLauncher)),
        );
    }

    // 2. `_faber_vm`'s directories.
    for (d, probe) in &v.vm_dirs {
        let owner = own(d.owner());
        let shown = format!(
            "{}:{} {:o}",
            FABER_VM_ACCOUNT,
            if d.owner() == Owner::FaberVmFaberGroup {
                FABER_ACCOUNT
            } else {
                FABER_VM_ACCOUNT
            },
            d.mode()
        );
        out.push(dir_step(
            s,
            &s.layout.vm_home.join(d.name()),
            probe,
            owner,
            d.mode(),
            &[vm.uid],
            &shown,
            VmAction::EnsureVmDir(*d),
        ));
    }

    // 3. The tools-disk cache.
    out.push(dir_step(
        s,
        Path::new(TOOLS_DISKS),
        &v.tools_dir,
        faber,
        0o755,
        &[0, faber.uid],
        &format!(
            "{FABER_ACCOUNT}:{FABER_ACCOUNT} 755: the supervisor ({FABER_VM_ACCOUNT}) reads the \
             tools disks {FABER_ACCOUNT} builds"
        ),
        VmAction::EnsureToolsDir,
    ));

    // 4. The image store.
    let image_steps: Vec<Step> = v
        .image_dirs
        .iter()
        .map(|(path, probe)| {
            dir_step(
                s,
                path,
                probe,
                fsx_root(),
                0o755,
                &[0],
                "root:wheel 755, and every directory above it root's",
                VmAction::EnsureImageStore,
            )
        })
        .collect();
    out.extend(image_steps);

    // 4b. The imports.
    if let Some(g) = &v.golden {
        out.push(golden_step(g, s.opts.vm.replace_golden));
    }
    if let Some(t) = &v.runner_tar {
        out.push(tar_step(t));
    }

    // 5 and 6. The network plan and the stored rules.
    wall_steps(s, steps, &mut out);

    // 7. The two daemons' logs, then the daemons: launchd opens a log as
    // whichever account the job runs as, and only if it is there.
    if let [(wall_log, wall_probe), (sup_log, sup_probe)] = v.logs.as_slice() {
        out.push(log_step(
            s,
            wall_log,
            wall_probe,
            fsx_root(),
            "root:wheel 640",
            &[0],
            VmAction::EnsureLog { supervisor: false },
        ));
        out.push(log_step(
            s,
            sup_log,
            sup_probe,
            Account {
                uid: vm.uid,
                gid: 0,
            },
            &format!("{FABER_VM_ACCOUNT}:wheel 640"),
            &[0, vm.uid],
            VmAction::EnsureLog { supervisor: true },
        ));
    }
    let layout = &s.layout;
    let stored_changes = has(&out, &VmAction::WriteStored);
    let cli_changes = steps.iter().any(|st| st.action == Some(Action::InstallCli));
    let anchor_why = match (stored_changes, cli_changes) {
        (true, true) => Some("its stored rules and the CLI copy it runs change"),
        (true, false) => Some("its stored rules change"),
        (false, true) => Some("the CLI copy it runs changes"),
        (false, false) => None,
    };
    out.push(daemon_step(
        service::WALL_PLIST,
        &service::render_wall_plist(layout),
        &v.anchor,
        anchor_why,
        &format!(
            "root runs {} wall-daemon, enforcing the stored rules; logs to {}",
            layout.cli_bin.display(),
            service::WALL_LOG
        ),
        |restart| VmAction::InstallAnchor { restart },
    ));

    let launcher_there =
        matches!(v.launcher, BinProbe::File { .. }) || has(&out, &VmAction::InstallLauncher);
    if launcher_there {
        let launcher_changes = has(&out, &VmAction::InstallLauncher);
        let plan_changes = has(&out, &VmAction::WritePlan);
        let why = match (launcher_changes, plan_changes) {
            (true, true) => Some("the launcher and the network plan change; running VMs are left"),
            (true, false) => Some("the launcher changes; running VMs finish on the old one"),
            (false, true) => {
                Some("the network plan it reads at start changes; running VMs are left")
            }
            (false, false) => None,
        };
        out.push(daemon_step(
            service::SUPERVISOR_PLIST,
            &service::render_supervisor_plist(layout, &supervisor_args(faber.uid)),
            &v.supervisor,
            why,
            &format!(
                "{FABER_VM_ACCOUNT} runs {} supervise, AbandonProcessGroup so its VMs outlive it; \
                 logs to {}",
                launcher_path(layout).display(),
                service::SUPERVISOR_LOG
            ),
            |restart| VmAction::InstallSupervisor { restart },
        ));
    } else {
        out.push(step(
            sec(),
            Status::Skipped,
            service::SUPERVISOR_PLIST,
            "warning: not installed: it runs the launcher, which is not installed",
        ));
    }
    steps.extend(out);
}

fn fsx_root() -> Account {
    super::fsx::ROOT
}

fn golden_step(g: &GoldenProbe, replace_ok: bool) -> Step {
    let name = g.name.as_deref().unwrap_or("?");
    let subject = vm_images::golden_dir().join(name).display().to_string();
    let readable = format!(
        "root:wheel, files 0444 in a 0755 bundle under 0755 directories: {FABER_VM_ACCOUNT} \
         reads it and only root can change it. A profile uses it as image = \"{name}\""
    );
    match vm_images::golden_verdict(g, replace_ok) {
        Ok(None) => step(sec(), Status::Ok, subject, readable),
        Ok(Some(false)) => step(
            sec(),
            Status::WillCreate,
            subject,
            format!(
                "imported from {} (cloned on APFS, each file's SHA-256 checked): {readable}",
                g.path.display()
            ),
        )
        .doing(Action::Vm(VmAction::ImportGolden { replace: false })),
        Ok(Some(true)) => step(
            sec(),
            Status::WillChange,
            subject,
            format!(
                "replaced from {} (--replace-golden; the old one is removed once the new one is \
                 in place): {readable}",
                g.path.display()
            ),
        )
        .doing(Action::Vm(VmAction::ImportGolden { replace: true })),
        Err(why) => step(sec(), Status::Refused, subject, why),
    }
}

fn tar_step(t: &TarProbe) -> Step {
    let name = t.name.as_deref().unwrap_or("?");
    let subject = vm_images::runner_dir().join(name).display().to_string();
    let seam = format!(
        "the daemon loads it into the Linux VM each time the engine comes up, reading the \
         tarballs in {} as {FABER_ACCOUNT}",
        vm_images::runner_dir().display()
    );
    match vm_images::tar_verdict(t) {
        Ok(false) => step(
            sec(),
            Status::Ok,
            subject,
            format!("kept, root:{FABER_ACCOUNT} 0440; {seam}"),
        ),
        Ok(true) => {
            let len = t.source.as_ref().map_or(0, |(_, len)| *len);
            step(
                sec(),
                Status::WillCreate,
                subject,
                format!(
                    "copied from {} ({len} bytes, a `docker save` archive), root:{FABER_ACCOUNT} \
                     0440 in a root:{FABER_ACCOUNT} 0750 directory, kept as the image's backup; \
                     {seam}",
                    t.path.display()
                ),
            )
            .doing(Action::Vm(VmAction::ImportRunnerTar))
        }
        Err(why) => step(sec(), Status::Refused, subject, why),
    }
}

/// Whether this run rewrites `config.toml` before the wall is rendered.
fn rewrites_config(steps: &[Step]) -> bool {
    steps.iter().any(|st| match &st.action {
        Some(Action::SeedConfig) => true,
        Some(Action::Migrate(item)) => item.kind == plan::FileKind::Config,
        _ => false,
    })
}

fn wall_steps(s: &HostState, before: &[Step], out: &mut Vec<Step>) {
    let v = &s.vm_runtime;
    let plan_subject = paths::PLAN.to_string();
    let stored_subject = paths::STORED.to_string();

    match &v.wall_dir {
        DirProbe::Dir { uid: 0, mode, .. } if mode & 0o022 == 0 => {}
        DirProbe::Missing => {}
        DirProbe::Dir { uid, mode, .. } => {
            out.push(step(
                sec(),
                Status::Refused,
                paths::WALL_DIR,
                format!(
                    "uid {uid}, mode {:o}: it must be root's and writable by nobody else",
                    mode & 0o7777
                ),
            ));
            return;
        }
        DirProbe::NotADir(what) => {
            out.push(step(
                sec(),
                Status::Refused,
                paths::WALL_DIR,
                format!("{what} where a directory belongs; move it aside and re-run"),
            ));
            return;
        }
        DirProbe::Unknown(e) => {
            out.push(unknown(s, sec(), paths::WALL_DIR.to_string(), e));
            return;
        }
    }

    if rewrites_config(before) {
        let later = "rendered once this run's config.toml is in place, from the macos-vm and \
                     docker profiles it names";
        out.push(
            step(
                sec(),
                Status::WillChange,
                plan_subject,
                format!("16 VM networks: slots 0-13 macOS, 14 the Linux engine, 15 the image build, {later}"),
            )
            .doing(Action::Vm(VmAction::WritePlan)),
        );
        out.push(
            step(
                sec(),
                Status::WillChange,
                stored_subject,
                format!("the rules for that plan and {}, {later}", paths::GRANTS),
            )
            .doing(Action::Vm(VmAction::WriteStored)),
        );
    } else {
        let config = match &v.config {
            Ok(c) => c,
            Err(e) => {
                out.push(unknown(
                    s,
                    sec(),
                    plan_subject,
                    &format!("config.toml: {e}"),
                ));
                return;
            }
        };
        let plan = network_plan(config);
        if let Err(e) = plan.validate() {
            out.push(step(
                sec(),
                Status::Refused,
                plan_subject,
                format!("the plan from config.toml's profiles is invalid: {e}"),
            ));
            return;
        }
        match &v.host_addrs {
            Err(e) => {
                out.push(unknown(
                    s,
                    sec(),
                    plan_subject,
                    &format!("this host's addresses: {e}"),
                ));
                return;
            }
            Ok(addrs) => {
                let hits = host_overlaps(&plan, addrs);
                if !hits.is_empty() {
                    let list: Vec<String> =
                        hits.iter().map(|(a, n)| format!("{a} in {n}")).collect();
                    out.push(step(
                        sec(),
                        Status::Refused,
                        plan_subject,
                        format!(
                            "this host has an address in a VM subnet ({}): a guest there could \
                             not tell the host from its gateway. Move that interface off \
                             192.168.239.0/24 and 192.168.240.0/20, then re-run",
                            list.join(", ")
                        ),
                    ));
                    return;
                }
            }
        }
        let want = plan_bytes(&plan);
        let profiles = format!(
            "macOS slots 0-13 list {}; the Linux engine's slot 14 lists {}; the build VM's \
             slot 15 lists none",
            list_or_none(&config.vm_profiles),
            list_or_none(&config.docker_profiles)
        );
        out.push(match &v.plan_json {
            FileBytes::Bytes(b) if *b == want => step(
                sec(),
                Status::Ok,
                plan_subject,
                format!("16 VM networks (192.168.240-254.0/24, and the build VM's 192.168.239.0/24); {profiles}"),
            ),
            FileBytes::Unknown(e) => unknown(s, sec(), plan_subject, e),
            found => step(
                sec(),
                if *found == FileBytes::Missing {
                    Status::WillCreate
                } else {
                    Status::WillChange
                },
                plan_subject,
                format!(
                    "root:wheel 0644: 16 VM networks, job slot n on 192.168.(240+n).0/24, guest \
                     .2, gateway .1, and the image build VM's slot 15 on 192.168.239.0/24 with \
                     no profile; {profiles}"
                ),
            )
            .doing(Action::Vm(VmAction::WritePlan)),
        });

        let grants = match &v.grants {
            Ok(g) => g.as_deref(),
            Err(e) => {
                out.push(unknown(
                    s,
                    sec(),
                    stored_subject,
                    &format!("{}: {e}", paths::GRANTS),
                ));
                return;
            }
        };
        match desired_stored(&plan, grants, config) {
            Err(why) => out.push(step(sec(), Status::Refused, stored_subject, why)),
            Ok((stored, warnings)) => {
                let notes = if warnings.is_empty() {
                    String::new()
                } else {
                    format!("; note: {}", warnings.join("; "))
                };
                let source = if grants.is_some() {
                    format!("the plan and {}", paths::GRANTS)
                } else {
                    "the plan, with no grants (VM jobs reach the public internet only)".into()
                };
                let have = match &v.stored_json {
                    FileBytes::Bytes(b) => StoredWall::from_json(b).ok(),
                    _ => None,
                };
                out.push(match (&v.stored_json, have) {
                    (_, Some(h)) if h == stored => step(
                        sec(),
                        Status::Ok,
                        stored_subject,
                        format!("rendered from {source}{notes}"),
                    ),
                    (FileBytes::Unknown(e), _) => unknown(s, sec(), stored_subject, e),
                    (found, _) => step(
                        sec(),
                        if *found == FileBytes::Missing {
                            Status::WillCreate
                        } else {
                            Status::WillChange
                        },
                        stored_subject,
                        format!(
                            "root:wheel 0644, rendered from {source} by the same function `sudo \
                             faber grant` stores with (digest {}){notes}",
                            &stored.digest[..12]
                        ),
                    )
                    .doing(Action::Vm(VmAction::WriteStored)),
                });
            }
        }
    }

    out.push(dir_step(
        s,
        Path::new(paths::VERDICT_DIR),
        &v.verdict_dir,
        fsx_root(),
        0o755,
        &[0],
        "root:wheel 755: only root writes the checker's verdict, and _faber reads it",
        VmAction::EnsureVerdictDir,
    ));
}

fn list_or_none(names: &[String]) -> String {
    if names.is_empty() {
        "no profile".to_string()
    } else {
        names.join(", ")
    }
}

/// The read-only probes, on macOS.
#[cfg(target_os = "macos")]
pub mod probe {
    use std::os::unix::fs::MetadataExt;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::platform::layout::SystemLayout;
    use crate::platform::service::launchd;
    use crate::setup::fsx;
    use crate::setup::tools::{Cmd, Runner, Tool};
    use crate::wall::grant_cmd::system;

    fn file_bytes(subdir: Option<&str>, name: &str, full: &str) -> FileBytes {
        match system::read_root_file(subdir, name, full) {
            Ok(None) => FileBytes::Missing,
            Ok(Some(b)) => FileBytes::Bytes(b),
            Err(e) => FileBytes::Unknown(format!("{e:#}")),
        }
    }

    /// Where the launcher comes from: `--launcher`, else beside `exe`.
    fn launcher_source(given: Option<&Path>, exe: &Path) -> LauncherSource {
        match given {
            Some(p) => binary_source(p.to_path_buf()),
            None => named_in(exe.parent(), &LAUNCHER_NAMES),
        }
    }

    /// Where the guest agent comes from: the launcher's directory, the one
    /// `--launcher` names, else `exe`'s. A release's assets sit side by side,
    /// so one flag finds both, and they cannot come from two releases by a
    /// slip of the second path.
    fn agent_source(given_launcher: Option<&Path>, exe: &Path) -> LauncherSource {
        named_in(given_launcher.unwrap_or(exe).parent(), &AGENT_NAMES)
    }

    /// The first of `names` in `dir` that exists, judged; `None` when none does.
    pub(crate) fn named_in(dir: Option<&Path>, names: &[&str]) -> LauncherSource {
        let Some(dir) = dir else {
            return LauncherSource::None;
        };
        match names
            .iter()
            .map(|n| dir.join(n))
            .find(|p| std::fs::symlink_metadata(p).is_ok())
        {
            Some(p) => binary_source(p),
            None => LauncherSource::None,
        }
    }

    /// A release binary to copy: refused unless it is an absolute path to a
    /// regular file with one link, its digest and chain read.
    fn binary_source(path: PathBuf) -> LauncherSource {
        let refused = |why: String| LauncherSource::Refused {
            path: path.clone(),
            why,
        };
        if !path.is_absolute() {
            return refused("is not an absolute path".into());
        }
        match std::fs::symlink_metadata(&path) {
            Err(e) => return refused(format!("cannot be read: {e}")),
            Ok(m) if m.file_type().is_symlink() => {
                return refused("is a symlink; give the file itself".into())
            }
            Ok(m) if !m.file_type().is_file() => return refused("is not a regular file".into()),
            Ok(m) if m.nlink() != 1 => {
                return refused(format!("has {} links; give a file with one", m.nlink()))
            }
            Ok(_) => {}
        }
        let digest = match fsx::file_digest(&path) {
            Ok(Some(d)) => d,
            Ok(None) => return refused("is not there".into()),
            Err(e) => return refused(format!("cannot be read: {e}")),
        };
        match exe::chain_of(&path) {
            Ok(chain) => LauncherSource::Found {
                path,
                sha256: digest.sha256,
                chain,
            },
            Err(e) => refused(format!("cannot be resolved: {e}")),
        }
    }

    /// The facts of each directory on `dir`'s canonical path that exists, from
    /// `/` down, without following a symlink: the walk
    /// [`fsx::root_dir_chain`] makes, read only.
    fn existing_chain(dir: &Path) -> Result<Vec<PathFacts>, String> {
        let canon = vm_images::canonical(dir);
        let mut chain = Vec::new();
        let mut at = PathBuf::from("/");
        let mut parts = vec![at.clone()];
        for c in canon.components().skip(1) {
            at.push(c);
            parts.push(at.clone());
        }
        for path in parts {
            match std::fs::symlink_metadata(&path) {
                Ok(m) => chain.push(PathFacts {
                    path,
                    uid: m.uid(),
                    gid: m.gid(),
                    mode: m.mode(),
                }),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
        }
        Ok(chain)
    }

    /// Everything [`VmRuntimeProbe`] holds. `faber_uid` is `_faber`'s, when
    /// the account exists: `config.toml` is read only if it owns it.
    pub fn gather(
        layout: &SystemLayout,
        exe: &Path,
        opts: &VmOpts,
        faber_uid: Option<u32>,
        runner: &mut dyn Runner,
        probe_dir: fn(&Path) -> DirProbe,
        probe_bin: fn(&Path) -> BinProbe,
    ) -> VmRuntimeProbe {
        let service = |runner: &mut dyn Runner, plist: &str, label: &str| {
            let printed = runner.run(&Cmd::new(
                Tool::Launchctl,
                ["print".to_string(), service::system_target_of(label)],
            ));
            let loaded = printed.as_ref().is_ok_and(|out| out.success);
            let stuck =
                printed.ok().filter(|_| loaded).and_then(|out| {
                    match launchd::print_status(&out.stdout) {
                        (Some("running"), _) => None,
                        (state, last_exit) => Some(format!(
                            "state {}, last exit code {}",
                            state.unwrap_or("unknown"),
                            last_exit.unwrap_or("unknown")
                        )),
                    }
                });
            ServiceProbe {
                installed: std::fs::read_to_string(plist).ok(),
                loaded,
                stuck,
            }
        };
        let grants = system::read_root_file(None, paths::GRANTS_NAME, paths::GRANTS)
            .map_err(|e| format!("{e:#}"))
            .and_then(|b| {
                b.map(|b| String::from_utf8(b).map_err(|_| "not UTF-8".to_string()))
                    .transpose()
            });
        let config = match faber_uid {
            Some(uid) => system::read_config_as(uid).map_err(|e| format!("{e:#}")),
            None => Ok(ConfigFacts::default()),
        };
        VmRuntimeProbe {
            launcher_source: launcher_source(opts.launcher.as_deref(), exe),
            launcher: probe_bin(&launcher_path(layout)),
            launcher_dir: probe_dir(&launcher_dir(layout)),
            launcher_parents: launcher_dir(layout)
                .parent()
                .map_or(Ok(Vec::new()), existing_chain),
            old_launchers: old_launchers(layout)
                .into_iter()
                .filter(|p| std::fs::symlink_metadata(p).is_ok())
                .collect(),
            agent_source: agent_source(opts.launcher.as_deref(), exe),
            agent: probe_bin(&agent_path(layout)),
            vm_dirs: VmDir::ALL
                .iter()
                .map(|d| (*d, probe_dir(&layout.vm_home.join(d.name()))))
                .collect(),
            tools_dir: probe_dir(Path::new(TOOLS_DISKS)),
            image_dirs: image_dirs()
                .into_iter()
                .map(|p: PathBuf| {
                    let probe = probe_dir(&p);
                    (p, probe)
                })
                .collect(),
            wall_dir: probe_dir(Path::new(paths::WALL_DIR)),
            verdict_dir: probe_dir(Path::new(paths::VERDICT_DIR)),
            plan_json: file_bytes(Some(paths::WALL_DIR_NAME), paths::PLAN_NAME, paths::PLAN),
            stored_json: file_bytes(
                Some(paths::WALL_DIR_NAME),
                paths::STORED_NAME,
                paths::STORED,
            ),
            grants,
            config,
            host_addrs: system::host_addrs().map_err(|e| format!("{e:#}")),
            anchor: service(runner, service::WALL_PLIST, service::WALL_LABEL),
            supervisor: service(runner, service::SUPERVISOR_PLIST, service::SUPERVISOR_LABEL),
            logs: log_paths()
                .into_iter()
                .map(|p| {
                    let probe = crate::setup::probe::probe_file(&p);
                    (p, probe)
                })
                .collect(),
            golden: opts
                .golden_image
                .as_deref()
                .map(|p| vm_images::probe::golden(p, opts.golden_name.as_deref())),
            runner_tar: opts
                .runner_image_tar
                .as_deref()
                .map(vm_images::probe::runner_tar),
            docker_cli: {
                let cli = Path::new(crate::platform::layout::DOCKER_CLI);
                crate::setup::docker_cli::DockerCliProbe {
                    installed: probe_bin(cli),
                    parents: cli.parent().map_or(Ok(Vec::new()), existing_chain),
                    apple_silicon: cfg!(target_arch = "aarch64"),
                    // All four, present or not: an absent one is judged by
                    // the directories that would hold it.
                    plugin_dirs: crate::setup::docker_cli::PLUGIN_DIRS
                        .iter()
                        .map(|d| crate::setup::docker_cli::probe_plugin_dir(Path::new(d)))
                        .collect(),
                }
            },
        }
    }
}

/// Executing the VM steps, on macOS.
#[cfg(target_os = "macos")]
pub mod apply {
    use std::ffi::OsStr;
    use std::path::Path;
    use std::time::Duration;

    use anyhow::{anyhow, bail, Context, Result};

    use super::*;
    use crate::platform::layout::SystemLayout;
    use crate::platform::service::launchd;
    use crate::setup::fsx::{self, Dir, ROOT};
    use crate::setup::tools::{self, Runner};
    use crate::wall::grant_cmd::system;

    /// What applying needs from the run.
    pub struct VmCtx<'a> {
        pub s: &'a HostState,
        pub steps: &'a [Step],
        pub faber: Account,
        pub vm: Account,
        /// The operator group's gid: the guest agent's, in `D/bin`.
        pub operator_gid: u32,
        pub ts: &'a str,
    }

    impl VmCtx<'_> {
        fn has(&self, a: &VmAction) -> bool {
            has(self.steps, a)
        }

        fn any(&self, f: impl Fn(&VmAction) -> bool) -> bool {
            self.steps.iter().any(|st| match &st.action {
                Some(Action::Vm(a)) => f(a),
                _ => false,
            })
        }
    }

    /// `_faber_vm`'s home, held open, and refused unless it is `_faber_vm`'s.
    fn vm_home(c: &VmCtx<'_>) -> Result<Dir> {
        use std::os::unix::fs::MetadataExt;
        let home = &c.s.layout.vm_home;
        let dir =
            fsx::open_leaf_dir(home).with_context(|| format!("opening {}", home.display()))?;
        let uid = dir.metadata()?.uid();
        if uid != c.vm.uid {
            bail!(
                "{} is owned by uid {uid}, not {FABER_VM_ACCOUNT}",
                home.display()
            );
        }
        Ok(dir)
    }

    fn install_launcher(c: &VmCtx<'_>) -> Result<()> {
        let bin = open_launcher_dir(c)?;
        install_companion(
            c,
            &c.s.vm_runtime.launcher_source,
            Placement {
                dir: &bin,
                dir_path: &launcher_dir(&c.s.layout),
                path: &launcher_path(&c.s.layout),
                owner: c.faber,
            },
            "launcher",
            launcher_version_problem,
        )
    }

    /// The guest agent into `D/bin`, the directory the daemon copy is in, held
    /// open and refused unless it is `_faber`'s: the updater replaces the agent
    /// there, and the tools disks are built from it.
    fn install_agent(c: &VmCtx<'_>) -> Result<()> {
        use std::os::unix::fs::MetadataExt;
        let dest = agent_path(&c.s.layout);
        let bin = fsx::open_leaf_dir(&c.s.layout.state_dir)
            .and_then(|d| d.subdir("bin"))
            .with_context(|| {
                format!(
                    "opening {} (a symlink is refused)",
                    c.s.layout.bin_dir.display()
                )
            })?;
        let meta = bin.metadata()?;
        if meta.uid() != c.faber.uid || ![c.faber.gid, c.operator_gid].contains(&meta.gid()) {
            bail!(
                "{} is {}:{}, not {FABER_ACCOUNT}'s",
                c.s.layout.bin_dir.display(),
                meta.uid(),
                meta.gid()
            );
        }
        install_companion(
            c,
            &c.s.vm_runtime.agent_source,
            Placement {
                dir: &bin,
                dir_path: &c.s.layout.bin_dir,
                path: &dest,
                owner: Account {
                    uid: c.faber.uid,
                    gid: c.operator_gid,
                },
            },
            "guest agent",
            agent_version_problem,
        )
    }

    /// Where [`install_companion`] puts a file: as `path` in `dir`, held open,
    /// which is `dir_path`, owned by `owner`: `_faber`, with that directory's
    /// group.
    struct Placement<'a> {
        dir: &'a Dir,
        dir_path: &'a Path,
        path: &'a Path,
        owner: Account,
    }

    /// Copy `source` to `at`, 0755: staged beside it with exactly the bytes
    /// the plan hashed, its `--version` run as `_faber` (never as root) and
    /// checked by `version_problem`, then renamed into place. A failed check
    /// removes the staged copy and installs nothing.
    fn install_companion(
        c: &VmCtx<'_>,
        source: &LauncherSource,
        at: Placement<'_>,
        what: &str,
        version_problem: fn(&str, &str) -> Option<String>,
    ) -> Result<()> {
        let Placement {
            dir: bin,
            dir_path: bin_dir,
            path: dest,
            owner,
        } = at;
        let LauncherSource::Found { path, sha256, .. } = source else {
            bail!("the plan installs a {what} but found none to install");
        };
        let (parent, name) = fsx::split(path)?;
        let src_dir = Dir::open(parent).with_context(|| format!("opening {}", parent.display()))?;
        let src = src_dir
            .open_file(name)
            .with_context(|| format!("opening {} (a symlink is refused)", path.display()))?;
        if !src.metadata()?.file_type().is_file() {
            bail!("{} is no longer a regular file", path.display());
        }
        let (dir, target) = fsx::split(dest)?;
        if dir != bin_dir {
            bail!("{} is not in {}", dest.display(), bin_dir.display());
        }
        let stage = format!(".{target}.setup-stage");
        let _ = bin.unlink(&stage);
        let mut from = &src;
        bin.write_atomic_expecting(&stage, &mut from, owner, 0o755, sha256)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::InvalidData => anyhow!(
                    "{} changed after the plan was made ({e}); nothing was installed",
                    path.display()
                ),
                _ => anyhow!(e),
            })?;
        // The version check runs the staged copy as `_faber`, never as root.
        let staged = dir.join(&stage);
        let checked = tools::run_as(
            &staged,
            &[OsStr::new("--version")],
            None,
            Some(c.faber),
            &c.s.layout.state_dir,
            Duration::from_secs(30),
        )
        .with_context(|| format!("running the staged {what}'s --version as {FABER_ACCOUNT}"))
        .and_then(|out| {
            if !out.success {
                bail!("its --version failed: {}", out.sanitized(400));
            }
            match version_problem(&out.stdout, &c.s.version) {
                Some(why) => bail!("{why}"),
                None => Ok(()),
            }
        });
        if let Err(e) = checked {
            let _ = bin.unlink(&stage);
            return Err(e.context(format!("{} was not installed", dest.display())));
        }
        bin.rename(&stage, target)?;
        println!("  installed {}", dest.display());
        Ok(())
    }

    /// The launcher's directory, walked from `/` through root's directories
    /// only ([`fsx::root_dir_chain`]), and refused unless it is `_faber`'s
    /// 0755.
    fn open_launcher_dir(c: &VmCtx<'_>) -> Result<Dir> {
        use std::os::unix::fs::MetadataExt;
        let dir = launcher_dir(&c.s.layout);
        let (parent, name) = canon_split_path(&dir)?;
        let chain = fsx::root_dir_chain(&parent)
            .with_context(|| format!("opening {}", parent.display()))?;
        let bin = chain
            .subdir(&name)
            .with_context(|| format!("opening {} (a symlink is refused)", dir.display()))?;
        let meta = bin.metadata()?;
        if (meta.uid(), meta.gid(), meta.mode() & 0o7777) != (c.faber.uid, c.faber.gid, 0o755) {
            bail!(
                "{} is {}:{} {:o}, not {FABER_ACCOUNT}'s 0755",
                dir.display(),
                meta.uid(),
                meta.gid(),
                meta.mode() & 0o7777
            );
        }
        Ok(bin)
    }

    /// Remove the launcher and its `.prev` a release before the move left in
    /// `D/bin`. `unlinkat` removes a symlink there, never what it points at.
    fn remove_old_launchers(c: &VmCtx<'_>) -> Result<()> {
        let bin = fsx::open_leaf_dir(&c.s.layout.state_dir)?
            .subdir("bin")
            .context("opening faber's bin directory")?;
        for old in old_launchers(&c.s.layout) {
            let (_, name) = fsx::split(&old)?;
            match bin.unlink(name) {
                Ok(()) => println!("  removed {}", old.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(anyhow!("removing {}: {e}", old.display())),
            }
        }
        Ok(())
    }

    fn ensure_vm_dir(c: &VmCtx<'_>, d: VmDir) -> Result<()> {
        let home = vm_home(c)?;
        let owner = plan::owner_account(
            d.owner(),
            plan::Ids {
                faber: c.faber,
                vm: c.vm,
                operator_gid: c.operator_gid,
            },
        );
        // The home is `_faber_vm`'s: made in root's staging beside it and
        // moved in, never created and reopened there (#228).
        let staging = fsx::Staging::beside(&c.s.layout.vm_home).with_context(|| {
            format!(
                "making root's staging beside {}",
                c.s.layout.vm_home.display()
            )
        })?;
        fsx::ensure_dir_beneath(&staging, &home, d.name(), owner, d.mode()).with_context(|| {
            format!("preparing {}", c.s.layout.vm_home.join(d.name()).display())
        })?;
        println!(
            "  prepared {} ({}:{} {:o})",
            c.s.layout.vm_home.join(d.name()).display(),
            owner.uid,
            owner.gid,
            d.mode()
        );
        Ok(())
    }

    /// The directory `name` in root's chain `parent` (canonical), as `owner`
    /// at `mode`.
    fn ensure_in_root_chain(
        parent: &Path,
        name: &str,
        owner: Account,
        mode: u32,
        prior: &[u32],
    ) -> Result<()> {
        let dir = fsx::root_dir_chain(parent)
            .with_context(|| format!("preparing {}", parent.display()))?;
        fsx::ensure_dir(&dir, name, owner, mode, prior)
            .with_context(|| format!("preparing {}", parent.join(name).display()))?;
        Ok(())
    }

    fn canon_split(path: &str) -> Result<(std::path::PathBuf, String)> {
        canon_split_path(Path::new(path))
    }

    fn canon_split_path(path: &Path) -> Result<(std::path::PathBuf, String)> {
        let canon = vm_images::canonical(path);
        let (parent, name) = fsx::split(&canon)?;
        Ok((parent.to_path_buf(), name.to_string()))
    }

    fn write_wall(c: &VmCtx<'_>) -> Result<()> {
        let config = system::read_config_as(c.faber.uid).context("reading config.toml")?;
        let plan = network_plan(&config);
        plan.validate().context("the network plan")?;
        let addrs = system::host_addrs()?;
        let hits = host_overlaps(&plan, &addrs);
        if !hits.is_empty() {
            bail!(
                "this host has an address in a VM subnet: {}",
                hits.iter()
                    .map(|(a, n)| format!("{a} in {n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        if c.has(&VmAction::WritePlan) {
            system::store_plan(&plan_bytes(&plan))?;
            println!("  wrote {}", paths::PLAN);
        }
        if c.has(&VmAction::WriteStored) {
            let grants = system::read_root_file(None, paths::GRANTS_NAME, paths::GRANTS)?
                .map(|b| {
                    String::from_utf8(b).map_err(|_| anyhow!("{} is not UTF-8", paths::GRANTS))
                })
                .transpose()?;
            let (stored, warnings) =
                desired_stored(&plan, grants.as_deref(), &config).map_err(|e| anyhow!(e))?;
            system::store_stored(&stored)?;
            println!(
                "  wrote {} (digest {})",
                paths::STORED,
                &stored.digest[..12]
            );
            for w in warnings {
                println!("  note: {}", crate::term::sanitize(&w));
            }
        }
        Ok(())
    }

    fn install_daemon(
        runner: &mut dyn Runner,
        plist: &str,
        label: &str,
        rendered: &str,
        restart: bool,
    ) -> Result<()> {
        let path = Path::new(plist);
        let (parent, name) = fsx::split(path)?;
        let dir = fsx::open_leaf_dir(parent)?;
        if std::fs::read_to_string(path).ok().as_deref() != Some(rendered) {
            dir.write_atomic(name, &mut rendered.as_bytes(), ROOT, 0o644)?;
            println!("  wrote {plist}");
        }
        launchd::lint(runner, path)?;
        let target = service::system_target_of(label);
        launchd::enable(runner, &target)?;
        if restart {
            // Booted out and in again, never `kickstart -k`: what it runs was
            // just replaced, and launchd would keep it pinned to the old code
            // (#479).
            launchd::reload(
                runner,
                "system",
                &target,
                path,
                JOB_EXIT_WAIT,
                Duration::from_secs(1),
            )?;
        } else {
            if !launchd::is_loaded(runner, &target) {
                launchd::bootstrap(runner, "system", path, Duration::from_secs(2))?;
            }
            launchd::kickstart(runner, &target, false)?;
        }
        launchd::wait_running(runner, &target, JOB_START_WAIT, Duration::from_millis(500))?;
        println!(
            "  {} {target}",
            if restart { "restarted" } else { "started" }
        );
        Ok(())
    }

    /// How long a booted-out checker or supervisor has to exit: launchd's
    /// default `ExitTimeOut` is 20 s, after which it SIGKILLs the job anyway.
    const JOB_EXIT_WAIT: Duration = Duration::from_secs(30);

    /// How long a started checker or supervisor has to be running before
    /// setup calls it failed. launchd respawns a KeepAlive job at most every
    /// 10 s, so this sees at least one retry.
    const JOB_START_WAIT: Duration = Duration::from_secs(15);

    /// How long setup waits for the restarted checker's first verdict.
    const VERDICT_WAIT: Duration = Duration::from_secs(30);

    /// Wait, at most `limit`, until the checker has written a verdict since
    /// `since`. Only the file's time is read: whether it is green, and for
    /// which stored set, is the build's own check. A timeout is reported, and
    /// the build then refuses with the checker's reason.
    fn wait_for_verdict(since: std::time::SystemTime, limit: Duration) {
        let deadline = std::time::Instant::now() + limit;
        loop {
            let fresh = std::fs::symlink_metadata(paths::VERDICT_CANONICAL)
                .and_then(|m| m.modified())
                .is_ok_and(|t| t >= since);
            if fresh {
                println!("  the wall's checker wrote its verdict");
                return;
            }
            if std::time::Instant::now() >= deadline {
                println!(
                    "  the wall's checker has written no verdict within {}s; see {}",
                    limit.as_secs(),
                    service::WALL_LOG
                );
                return;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    }

    /// Execute the VM steps the plan made, in the order the module docs give.
    /// After the setup record, before the main service starts: the daemon
    /// finds the supervisor and the wall in place.
    pub fn apply(c: &VmCtx<'_>, runner: &mut dyn Runner) -> Result<()> {
        if !c.any(|_| true) {
            return Ok(());
        }
        let layout: &SystemLayout = &c.s.layout;
        for d in VmDir::ALL {
            if c.has(&VmAction::EnsureVmDir(d)) {
                ensure_vm_dir(c, d)?;
            }
        }
        if c.has(&VmAction::EnsureToolsDir) {
            let (parent, name) = canon_split(TOOLS_DISKS)?;
            ensure_in_root_chain(&parent, &name, c.faber, 0o755, &[0, c.faber.uid])?;
            println!("  prepared {TOOLS_DISKS}");
        }
        if c.has(&VmAction::EnsureImageStore) {
            let store = vm_images::canonical(Path::new(IMAGE_STORE));
            fsx::root_dir_chain(&store)
                .with_context(|| format!("preparing {}", store.display()))?
                .set_owner_mode(ROOT, 0o755)?;
            println!("  prepared {IMAGE_STORE}");
        }
        if c.has(&VmAction::EnsureLauncherDir) {
            let dir = launcher_dir(layout);
            let (parent, name) = canon_split_path(&dir)?;
            ensure_in_root_chain(&parent, &name, c.faber, 0o755, &[0, c.faber.uid])?;
            println!("  prepared {}", dir.display());
        }
        if c.has(&VmAction::InstallLauncher) {
            install_launcher(c)?;
        }
        if c.has(&VmAction::RemoveOldLauncher) {
            remove_old_launchers(c)?;
        }
        if c.has(&VmAction::InstallAgent) {
            install_agent(c)?;
        }
        if c.any(|a| matches!(a, VmAction::ImportGolden { .. })) {
            let g =
                c.s.vm_runtime
                    .golden
                    .as_ref()
                    .context("no golden image probed")?;
            let replace = c.has(&VmAction::ImportGolden { replace: true });
            println!("  importing the golden image (cloning, then checking every file)");
            let name = vm_images::apply::import_golden(g, replace, c.ts)?;
            println!(
                "  imported {}: a profile uses it as image = \"{name}\"",
                vm_images::golden_dir().join(&name).display()
            );
        }
        if c.has(&VmAction::ImportRunnerTar) {
            let t =
                c.s.vm_runtime
                    .runner_tar
                    .as_ref()
                    .context("no tarball probed")?;
            let name = vm_images::apply::import_runner_tar(t, c.faber.gid)?;
            println!("  kept {}", vm_images::runner_dir().join(name).display());
        }
        if c.has(&VmAction::WritePlan) || c.has(&VmAction::WriteStored) {
            write_wall(c)?;
        }
        if c.has(&VmAction::EnsureVerdictDir) {
            let (parent, name) = canon_split(paths::VERDICT_DIR)?;
            ensure_in_root_chain(&parent, &name, ROOT, 0o755, &[0])?;
            println!("  prepared {}", paths::VERDICT_DIR);
        }
        for supervisor in [false, true] {
            if c.has(&VmAction::EnsureLog { supervisor }) {
                let (path, owner, prior) = if supervisor {
                    (
                        service::SUPERVISOR_LOG,
                        Account {
                            uid: c.vm.uid,
                            gid: 0,
                        },
                        vec![0, c.vm.uid],
                    )
                } else {
                    (service::WALL_LOG, ROOT, vec![0])
                };
                let (parent, name) = canon_split(path)?;
                let dir = fsx::root_dir_chain(&parent)
                    .with_context(|| format!("preparing {}", parent.display()))?;
                dir.ensure_file(&name, owner, 0o640, &prior)
                    .with_context(|| format!("preparing {path}"))?;
                println!("  prepared {path}");
            }
        }
        let started = std::time::SystemTime::now();
        let mut anchor_started = false;
        for restart in [false, true] {
            if c.has(&VmAction::InstallAnchor { restart }) {
                install_daemon(
                    runner,
                    service::WALL_PLIST,
                    service::WALL_LABEL,
                    &service::render_wall_plist(layout),
                    restart,
                )?;
                anchor_started = true;
            }
        }
        // The Linux image's build, last in setup, boots its VM only on a
        // green verdict for the stored set just written
        // (`linux_image::wall_verdict`). The checker writes its first verdict
        // on its first tick, at start; wait for that here, so the build
        // judges this run's verdict, not a missing or older one.
        if anchor_started
            && c.steps
                .iter()
                .any(|st| st.action == Some(Action::BuildLinuxImage))
        {
            wait_for_verdict(started, VERDICT_WAIT);
        }
        for restart in [false, true] {
            if c.has(&VmAction::InstallSupervisor { restart }) {
                install_daemon(
                    runner,
                    service::SUPERVISOR_PLIST,
                    service::SUPERVISOR_LABEL,
                    &service::render_supervisor_plist(layout, &supervisor_args(c.faber.uid)),
                    restart,
                )?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::platform::layout::SystemLayout;
    use crate::setup::plan::tests::render;
    use crate::setup::plan::tests::{fresh_mac, set_up_mac};
    use crate::setup::plan::{counts, plan};
    use crate::wall::grants::{Grant, Proto};

    pub(crate) const LAUNCHER_SHA: &str =
        "1111111111111111111111111111111111111111111111111111111111111111";
    pub(crate) const AGENT_SHA: &str =
        "a9e1a9e1a9e1a9e1a9e1a9e1a9e1a9e1a9e1a9e1a9e1a9e1a9e1a9e1a9e1a9e1";

    fn faber() -> Account {
        Account { uid: 451, gid: 451 }
    }

    fn vm() -> Account {
        Account { uid: 452, gid: 452 }
    }

    pub(crate) fn config() -> ConfigFacts {
        ConfigFacts {
            profiles: vec!["mac".into(), "docker".into()],
            vm_profiles: vec!["mac".into()],
            docker_profiles: vec!["docker".into()],
            ..ConfigFacts::default()
        }
    }

    fn root_chain() -> Vec<PathFacts> {
        [
            "/usr/local/bin/faber-vm",
            "/usr/local/bin",
            "/usr/local",
            "/usr",
            "/",
        ]
        .iter()
        .enumerate()
        .map(|(i, p)| PathFacts {
            path: p.into(),
            uid: 0,
            gid: 0,
            mode: if i == 0 { 0o100_755 } else { 0o040_755 },
        })
        .collect()
    }

    /// `/private/var/db/faber` and every directory above it, root's 0755.
    fn launcher_parents() -> Vec<PathFacts> {
        [
            "/",
            "/private",
            "/private/var",
            "/private/var/db",
            "/private/var/db/faber",
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

    fn dir(owner: Account, mode: u32) -> DirProbe {
        DirProbe::Dir {
            uid: owner.uid,
            gid: owner.gid,
            mode: 0o040_000 | mode,
        }
    }

    /// The VM side of a host setup has finished on: everything in place,
    /// both daemons running.
    pub(crate) fn installed(layout: &SystemLayout) -> VmRuntimeProbe {
        let root = Account { uid: 0, gid: 0 };
        let cfg = config();
        let plan = network_plan(&cfg);
        let (stored, _) = desired_stored(&plan, None, &cfg).unwrap();
        VmRuntimeProbe {
            launcher_source: LauncherSource::Found {
                path: "/usr/local/bin/faber-vm".into(),
                sha256: LAUNCHER_SHA.into(),
                chain: root_chain(),
            },
            launcher: BinProbe::File {
                sha256: LAUNCHER_SHA.into(),
                uid: 451,
                gid: 451,
                mode: 0o100_755,
                version: None,
            },
            launcher_dir: dir(faber(), 0o755),
            launcher_parents: Ok(launcher_parents()),
            old_launchers: vec![],
            agent_source: LauncherSource::Found {
                path: "/usr/local/bin/faber-agent".into(),
                sha256: AGENT_SHA.into(),
                chain: root_chain(),
            },
            agent: BinProbe::File {
                sha256: AGENT_SHA.into(),
                uid: 451,
                gid: 451,
                mode: 0o100_755,
                version: None,
            },
            vm_dirs: VmDir::ALL
                .iter()
                .map(|d| {
                    let owner = plan::owner_account(
                        d.owner(),
                        plan::Ids {
                            faber: faber(),
                            vm: vm(),
                            operator_gid: 453,
                        },
                    );
                    (*d, dir(owner, d.mode()))
                })
                .collect(),
            tools_dir: dir(faber(), 0o755),
            image_dirs: image_dirs()
                .into_iter()
                .map(|p| (p, dir(root, 0o755)))
                .collect(),
            wall_dir: dir(root, 0o755),
            verdict_dir: dir(root, 0o755),
            plan_json: FileBytes::Bytes(plan_bytes(&plan)),
            stored_json: FileBytes::Bytes(stored.to_json()),
            grants: Ok(None),
            config: Ok(cfg),
            host_addrs: Ok(vec![
                "127.0.0.1".parse().unwrap(),
                "192.0.2.10".parse().unwrap(),
            ]),
            anchor: ServiceProbe {
                installed: Some(service::render_wall_plist(layout)),
                loaded: true,
                stuck: None,
            },
            supervisor: ServiceProbe {
                installed: Some(service::render_supervisor_plist(
                    layout,
                    &supervisor_args(faber().uid),
                )),
                loaded: true,
                stuck: None,
            },
            logs: vec![
                (
                    service::WALL_LOG.into(),
                    FileProbe::File {
                        uid: 0,
                        gid: 0,
                        mode: 0o100_640,
                    },
                ),
                (
                    service::SUPERVISOR_LOG.into(),
                    FileProbe::File {
                        uid: vm().uid,
                        gid: 0,
                        mode: 0o100_640,
                    },
                ),
            ],
            golden: None,
            runner_tar: None,
            docker_cli: crate::setup::docker_cli::DockerCliProbe {
                installed: BinProbe::File {
                    sha256: crate::setup::docker_cli::PIN.binary_sha256.into(),
                    uid: 0,
                    gid: 0,
                    mode: 0o100_755,
                    version: None,
                },
                parents: Ok(crate::setup::docker_cli::tests::root_parents()),
                apple_silicon: true,
                plugin_dirs: crate::setup::docker_cli::PLUGIN_DIRS
                    .iter()
                    .map(|d| crate::setup::docker_cli::tests::absent_plugin_dir(d))
                    .collect(),
            },
        }
    }

    fn vm_steps(s: &HostState) -> Vec<Step> {
        plan(s)
            .into_iter()
            .filter(|st| st.section == Section::VmRuntime)
            .collect()
    }

    fn actions(s: &HostState) -> Vec<VmAction> {
        vm_steps(s)
            .into_iter()
            .filter_map(|st| match st.action {
                Some(Action::Vm(a)) => Some(a),
                _ => None,
            })
            .collect()
    }

    fn find<'a>(steps: &'a [Step], subject: &str) -> &'a Step {
        steps
            .iter()
            .find(|st| st.subject == subject)
            .unwrap_or_else(|| panic!("no step for {subject}: {steps:#?}"))
    }

    #[test]
    fn the_network_plan_has_sixteen_slots_and_validates() {
        use crate::setup::linux_image::{build_network, BUILD_NETWORK_ID, BUILD_SLOT};
        let p = network_plan(&config());
        p.validate().unwrap();
        assert_eq!(p.networks.len(), 16);
        for (i, n) in p.networks.iter().take(15).enumerate() {
            let slot = i as u8;
            assert_eq!(n.id, slot + 1);
            assert_ne!(n.id, BUILD_NETWORK_ID);
            assert_eq!(n.subnet.to_string(), format!("192.168.{}.0/24", 240 + slot));
            assert_eq!(n.gateway, Ipv4Addr::new(192, 168, 240 + slot, 1));
            assert_eq!(n.slots.len(), 1);
            assert_eq!(n.slots[0].id, slot);
            assert_eq!(n.slots[0].addr, Ipv4Addr::new(192, 168, 240 + slot, 2));
            let want: &[&str] = if slot == LINUX_SLOT {
                &["docker"]
            } else {
                &["mac"]
            };
            assert_eq!(n.slots[0].profiles, want);
        }
        // The build network, exactly as linux_image gives it, and last.
        assert_eq!(p.networks[15], build_network());
        assert_eq!(p.networks[15].slots[0].id, BUILD_SLOT);
        assert!(p.networks[15].slots[0].profiles.is_empty());
        // The JSON is the launcher's format: it parses back.
        assert_eq!(NetworkPlan::from_json(&plan_bytes(&p)).unwrap(), p);
    }

    /// What setup stores is what the build VM's wall check accepts: its
    /// network in exactly the shape of a slot with no grant, whatever the
    /// grants say.
    #[test]
    fn the_stored_set_holds_the_build_network_as_the_builder_wants_it() {
        use crate::setup::linux_image::{build_network, BUILD_NETWORK_ID};
        let g = Grants::default()
            .with_grant(Grant {
                profile: "docker".into(),
                to: "10.0.0.0/8".parse().unwrap(),
                port: 22,
                proto: Proto::Tcp,
            })
            .unwrap();
        let plan = network_plan(&config());
        let (stored, _) = desired_stored(&plan, Some(&g.to_toml()), &config()).unwrap();
        let have = stored
            .networks
            .iter()
            .find(|n| n.id == BUILD_NETWORK_ID)
            .unwrap();
        let net = build_network();
        assert_eq!((have.gateway, have.subnet), (net.gateway, net.subnet));
        let resolvers = crate::wall::grants::DEFAULT_RESOLVERS
            .iter()
            .copied()
            .collect();
        assert_eq!(
            have.lines,
            crate::wall::render::ungranted_lines(&net, &resolvers, &stored.plan())
        );
    }

    #[test]
    fn the_launcher_path_is_the_layouts() {
        let l = SystemLayout::for_os(Os::MacOs);
        assert_eq!(launcher_path(&l), l.launcher_bin);
        assert_eq!(
            service::supervisor_argv(&l, &supervisor_args(451))[0],
            launcher_path(&l).display().to_string()
        );
    }

    #[test]
    fn host_addresses_in_a_planned_subnet_are_found() {
        let p = network_plan(&config());
        let hits = host_overlaps(
            &p,
            &[
                "192.168.250.7".parse().unwrap(),
                "10.0.0.1".parse().unwrap(),
            ],
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1.to_string(), "192.168.250.0/24");
    }

    #[test]
    fn a_running_vm_s_own_gateway_is_no_overlap() {
        // Setup re-run with the Linux engine up: vmnet has put slot 14's gateway
        // on the engine's bridge. Any other address in a VM subnet still is one.
        let p = network_plan(&config());
        let hits = host_overlaps(
            &p,
            &[
                "192.168.254.1".parse().unwrap(),
                "192.168.254.9".parse().unwrap(),
            ],
        );
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert_eq!(hits[0].0.to_string(), "192.168.254.9");
    }

    #[test]
    fn the_launcher_must_report_this_version() {
        assert_eq!(
            launcher_version_problem("faber-vm 0.4.0+gabc1234\n", "0.4.0+gabc1234"),
            None
        );
        assert!(launcher_version_problem("faber-vm 0.3.9", "0.4.0").is_some());
        let e = launcher_version_problem("\u{1b}[2J", "0.4.0").unwrap();
        assert!(!e.contains('\u{1b}'), "{e:?}");
    }

    /// A fresh host with a launcher beside the binary plans every VM step.
    #[test]
    fn a_fresh_host_plans_the_whole_vm_runtime() {
        let mut s = fresh_mac();
        s.vm_runtime.launcher_source = installed(&s.layout).launcher_source;
        s.vm_runtime.agent_source = installed(&s.layout).agent_source;
        let steps = plan(&s);
        assert_eq!(counts(&steps).2, 0, "{}", render(&s, &steps));
        let got = actions(&s);
        for want in [
            VmAction::EnsureLauncherDir,
            VmAction::InstallLauncher,
            VmAction::InstallAgent,
            VmAction::EnsureVmDir(VmDir::Run),
            VmAction::EnsureVmDir(VmDir::State),
            VmAction::EnsureVmDir(VmDir::Clones),
            VmAction::EnsureVmDir(VmDir::Linux),
            VmAction::EnsureToolsDir,
            VmAction::EnsureImageStore,
            VmAction::WritePlan,
            VmAction::WriteStored,
            VmAction::EnsureVerdictDir,
            VmAction::EnsureLog { supervisor: false },
            VmAction::EnsureLog { supervisor: true },
            VmAction::InstallAnchor { restart: false },
            VmAction::InstallSupervisor { restart: false },
        ] {
            assert!(got.contains(&want), "{want:?} not in {got:?}");
        }
        // The fixture migrates a config: the wall is rendered after it lands.
        let v = vm_steps(&s);
        assert!(find(&v, paths::PLAN)
            .detail
            .contains("once this run's config.toml"));
        // These come before the main service.
        let pos = |f: &dyn Fn(&Step) -> bool| steps.iter().position(f).unwrap();
        assert!(
            pos(&|st| st.section == Section::VmRuntime) > pos(&|st| st.section == Section::Record)
        );
        assert!(
            pos(&|st| st.section == Section::Service)
                > pos(&|st| st.subject == service::SUPERVISOR_PLIST)
        );
    }

    /// The idempotency guarantee, for the VM side too: nothing to do, and
    /// nothing restarted.
    #[test]
    fn a_set_up_host_plans_no_vm_change() {
        let s = set_up_mac();
        assert!(actions(&s).is_empty(), "{}", render(&s, &plan(&s)));
        assert!(vm_steps(&s).iter().all(|st| st.status == Status::Ok));
    }

    /// #260: setup installs the guest agent the tools disks are built from, at
    /// the path the updater replaces, and a re-run (the agent in place, the
    /// same bytes offered) plans nothing.
    #[test]
    fn setup_installs_the_agent_and_a_rerun_plans_nothing() {
        let mut s = set_up_mac();
        s.vm_runtime.agent = BinProbe::Missing;
        assert_eq!(actions(&s), vec![VmAction::InstallAgent]);
        let st = find(
            &vm_steps(&s),
            "/Library/Application Support/faber/bin/faber-agent",
        )
        .clone();
        assert_eq!(st.status, Status::WillCreate);
        assert!(
            st.detail.contains("must report faber-agent"),
            "{}",
            st.detail
        );

        // A different agent in place (an older release's) is replaced.
        s.vm_runtime.agent = installed(&s.layout).agent;
        if let BinProbe::File { sha256, .. } = &mut s.vm_runtime.agent {
            *sha256 = "7".repeat(64);
        }
        assert_eq!(actions(&s), vec![VmAction::InstallAgent]);

        // Installed and offered again: nothing.
        let s = set_up_mac();
        assert!(!actions(&s).contains(&VmAction::InstallAgent));
    }

    /// Setup and the updater agree on where the agent lives: setup installs it
    /// where `UpdatePaths::agent` replaces it and `tools_disk` reads it.
    #[test]
    fn the_agent_path_is_the_updaters() {
        let layout = SystemLayout::for_os(Os::MacOs);
        assert_eq!(
            agent_path(&layout),
            crate::update::UpdatePaths::with_root(layout.state_dir.clone()).agent()
        );
    }

    #[test]
    fn without_an_agent_setup_warns_and_never_takes_a_strangers() {
        let mut s = set_up_mac();
        s.vm_runtime.agent_source = LauncherSource::None;
        s.vm_runtime.agent = BinProbe::Missing;
        let st = find(
            &vm_steps(&s),
            "/Library/Application Support/faber/bin/faber-agent",
        )
        .clone();
        assert_eq!(st.status, Status::Skipped);
        assert!(st.detail.contains("tools disk"), "{}", st.detail);

        // One already there but not `_faber`'s 0755 is refused, not adopted.
        s.vm_runtime.agent = BinProbe::File {
            sha256: AGENT_SHA.into(),
            uid: 0,
            gid: 0,
            mode: 0o100_755,
            version: None,
        };
        let st = find(
            &vm_steps(&s),
            "/Library/Application Support/faber/bin/faber-agent",
        )
        .clone();
        assert_eq!(st.status, Status::Refused);

        // A source faber's own accounts could have replaced is refused.
        let mut s = set_up_mac();
        s.vm_runtime.agent = BinProbe::Missing;
        if let LauncherSource::Found { chain, .. } = &mut s.vm_runtime.agent_source {
            chain.last_mut().unwrap().uid = 451;
        }
        let st = find(
            &vm_steps(&s),
            "/Library/Application Support/faber/bin/faber-agent",
        )
        .clone();
        assert_eq!(st.status, Status::Refused, "{}", st.detail);
    }

    #[test]
    fn the_agent_must_report_this_version_and_a_protocol() {
        let v = "0.4.0+gabc1234";
        assert_eq!(
            agent_version_problem("faber-agent 0.4.0+gabc1234 protocol=2\n", v),
            None
        );
        for bad in [
            "faber-agent 0.4.0+gabc1234",
            "faber-agent 0.3.9+gabc1234 protocol=2",
            "faber-agent 0.4.0+gabc1234 protocol=",
            "faber-agent 0.4.0+gabc1234 protocol=2x",
            "faber-vm 0.4.0+gabc1234 protocol=2",
        ] {
            assert!(agent_version_problem(bad, v).is_some(), "{bad}");
        }
        let e = agent_version_problem("\u{1b}[2J", v).unwrap();
        assert!(!e.contains('\u{1b}'), "{e:?}");
    }

    /// The supervisor's log is created before launchd opens it, owned by the
    /// account the job runs as; a symlink or a stranger's file is refused.
    #[test]
    fn the_daemon_logs_are_created_and_judged() {
        let mut s = set_up_mac();
        s.vm_runtime.logs[1].1 = FileProbe::Missing;
        assert_eq!(actions(&s), vec![VmAction::EnsureLog { supervisor: true }]);
        let all = vm_steps(&s);
        let st = find(&all, service::SUPERVISOR_LOG);
        assert_eq!(st.status, Status::WillCreate);
        assert!(st.detail.contains("_faber_vm:wheel 640"), "{}", st.detail);

        s.vm_runtime.logs[1].1 = FileProbe::NotRegular("a symlink".into());
        assert!(actions(&s).is_empty());
        assert_eq!(
            find(&vm_steps(&s), service::SUPERVISOR_LOG).status,
            Status::Refused
        );

        s.vm_runtime.logs[1].1 = FileProbe::File {
            uid: 999,
            gid: 0,
            mode: 0o100_640,
        };
        assert_eq!(
            find(&vm_steps(&s), service::SUPERVISOR_LOG).status,
            Status::Refused
        );

        s.vm_runtime.logs[0].1 = FileProbe::File {
            uid: 0,
            gid: 0,
            mode: 0o100_644,
        };
        s.vm_runtime.logs[1] = installed(&s.layout).logs[1].clone();
        assert_eq!(actions(&s), vec![VmAction::EnsureLog { supervisor: false }]);
    }

    #[test]
    fn linux_plans_none_of_it() {
        let mut s = set_up_mac();
        s.os = Os::Linux;
        assert!(vm_steps(&s).is_empty());
    }

    /// No launcher anywhere: a warning, and the supervisor is skipped too.
    /// The VM steps refuse nothing; the plan as a whole is refused by the
    /// #223 guard, since such a host would run no jobs, unless
    /// `--before-vm-runtime` says to go on.
    #[test]
    fn no_launcher_skips_the_launcher_and_the_supervisor() {
        let mut s = set_up_mac();
        s.vm_runtime.launcher_source = LauncherSource::None;
        s.vm_runtime.launcher = BinProbe::Missing;
        s.linux_image.launcher = Err("the VM launcher is not installed".into());
        let v = vm_steps(&s);
        let l = find(&v, &s.layout.launcher_bin.display().to_string());
        assert_eq!(l.status, Status::Skipped);
        assert!(l.detail.starts_with("warning:"));
        assert_eq!(find(&v, service::SUPERVISOR_PLIST).status, Status::Skipped);
        assert!(v.iter().all(|st| st.status != Status::Refused));
        assert_eq!(counts(&plan(&s)), (0, 0, 1));
        s.opts.before_vm_runtime = true;
        assert_eq!(counts(&plan(&s)), (0, 0, 0));
        assert!(render(&s, &plan(&s)).contains("! warning"));
        // An installed launcher with no new one given is kept.
        let mut s = set_up_mac();
        s.vm_runtime.launcher_source = LauncherSource::None;
        assert!(actions(&s).is_empty());
    }

    /// A different launcher replaces the installed one and restarts the
    /// supervisor; a refused source is refused; a source faber's accounts
    /// could write is refused.
    #[test]
    fn the_launcher_is_replaced_refused_or_kept() {
        let mut s = set_up_mac();
        if let LauncherSource::Found { sha256, .. } = &mut s.vm_runtime.launcher_source {
            *sha256 = "2".repeat(64);
        }
        let got = actions(&s);
        assert!(got.contains(&VmAction::InstallLauncher));
        assert!(
            got.contains(&VmAction::InstallSupervisor { restart: true }),
            "{got:?}"
        );
        assert!(!got
            .iter()
            .any(|a| matches!(a, VmAction::InstallAnchor { .. })));

        let mut s = set_up_mac();
        s.vm_runtime.launcher_source = LauncherSource::Refused {
            path: "/tmp/faber-vm".into(),
            why: "is a symlink; give the file itself".into(),
        };
        let st = find(&vm_steps(&s), &s.layout.launcher_bin.display().to_string()).clone();
        assert_eq!(st.status, Status::Refused);

        let mut s = set_up_mac();
        if let LauncherSource::Found { chain, .. } = &mut s.vm_runtime.launcher_source {
            chain[1].uid = 451;
        }
        let st = find(&vm_steps(&s), &s.layout.launcher_bin.display().to_string()).clone();
        assert_eq!(st.status, Status::Refused, "{}", st.detail);
        assert!(st.detail.contains("faber's own accounts"));

        let mut s = set_up_mac();
        s.vm_runtime.launcher = BinProbe::File {
            sha256: LAUNCHER_SHA.into(),
            uid: 0,
            gid: 0,
            mode: 0o100_755,
            version: None,
        };
        assert!(actions(&s).contains(&VmAction::InstallLauncher));
    }

    /// The launcher lives outside `D`, which `_faber_vm` cannot traverse, in a
    /// `_faber` 0755 directory under root's: created, fixed, or refused.
    #[test]
    fn the_launcher_directory_is_created_fixed_or_refused() {
        let subject = "/var/db/faber/bin";
        let mut s = fresh_mac();
        s.vm_runtime.launcher_source = installed(&s.layout).launcher_source;
        s.vm_runtime.launcher_parents = Ok(launcher_parents()[..4].to_vec());
        let st = find(&vm_steps(&s), subject).clone();
        assert_eq!(st.status, Status::WillCreate);
        assert!(st.detail.contains("_faber:_faber 755"), "{}", st.detail);
        // It comes before the launcher it holds.
        let v = vm_steps(&s);
        let at = |sub: &str| v.iter().position(|st| st.subject == sub).unwrap();
        assert!(at(subject) < at("/var/db/faber/bin/faber-vm"));

        // Root's (an earlier hand-made one): given to `_faber`.
        let mut s = set_up_mac();
        s.vm_runtime.launcher_dir = dir(Account { uid: 0, gid: 0 }, 0o755);
        assert_eq!(actions(&s), vec![VmAction::EnsureLauncherDir]);
        // Group-writable: fixed to 0755.
        s.vm_runtime.launcher_dir = dir(faber(), 0o775);
        assert_eq!(actions(&s), vec![VmAction::EnsureLauncherDir]);
        // `_faber_vm`'s, or anyone else's: refused, never adopted.
        s.vm_runtime.launcher_dir = dir(vm(), 0o755);
        assert_eq!(find(&vm_steps(&s), subject).status, Status::Refused);
        s.vm_runtime.launcher_dir = DirProbe::NotADir("a symlink".into());
        assert_eq!(find(&vm_steps(&s), subject).status, Status::Refused);

        // A directory above it that is not root's, or that others can write,
        // refuses the plan, with why.
        for (i, uid, mode) in [(4, 451, 0o040_755), (4, 0, 0o040_775), (3, 0, 0o040_777)] {
            let mut s = set_up_mac();
            if let Ok(chain) = &mut s.vm_runtime.launcher_parents {
                chain[i].uid = uid;
                chain[i].mode = mode;
            }
            let st = find(&vm_steps(&s), subject).clone();
            assert_eq!(st.status, Status::Refused, "{}", st.detail);
            assert!(st.detail.contains("must be root's"), "{}", st.detail);
            assert!(counts(&plan(&s)).2 > 0);
        }
        let mut s = set_up_mac();
        s.vm_runtime.launcher_parents = Err("Permission denied".into());
        assert_ne!(find(&vm_steps(&s), subject).status, Status::Ok);
    }

    /// A launcher a release before the move left in `D/bin` is removed, and
    /// the supervisor, which ran it from there, is rewritten onto the new path.
    #[test]
    fn a_launcher_left_in_the_state_dir_is_removed() {
        let mut s = set_up_mac();
        let old = old_launchers(&s.layout);
        assert_eq!(
            old,
            vec![
                PathBuf::from("/Library/Application Support/faber/bin/faber-vm"),
                PathBuf::from("/Library/Application Support/faber/bin/faber-vm.prev"),
            ]
        );
        s.vm_runtime.old_launchers = old.clone();
        assert_eq!(actions(&s), vec![VmAction::RemoveOldLauncher; 2]);
        for p in &old {
            let st = find(&vm_steps(&s), &p.display().to_string()).clone();
            assert_eq!(st.status, Status::WillChange);
            assert!(st.detail.contains("/var/db/faber/bin"), "{}", st.detail);
        }
        // The old supervisor plist named the old path: rewritten, restarted.
        s.vm_runtime.supervisor.installed = Some(
            service::render_supervisor_plist(&s.layout, &supervisor_args(faber().uid)).replace(
                "/var/db/faber/bin/faber-vm",
                "/Library/Application Support/faber/bin/faber-vm",
            ),
        );
        assert!(actions(&s).contains(&VmAction::InstallSupervisor { restart: true }));
        // None of it is under the image store.
        for p in &old {
            assert!(!p.starts_with(IMAGE_STORE));
        }
    }

    #[test]
    fn launcher_parents_must_be_roots_and_unwritable() {
        assert_eq!(launcher_parents_problem(&launcher_parents()), None);
        assert_eq!(launcher_parents_problem(&[]), None);
        let mut c = launcher_parents();
        c[2].mode = 0o120_755; // a symlink
        assert!(launcher_parents_problem(&c).is_some());
        // Root's, but closed to "other": `_faber_vm` could not search it.
        let mut c = launcher_parents();
        c[4].mode = 0o040_750;
        assert!(launcher_parents_problem(&c)
            .unwrap()
            .contains("not searchable"));
    }

    /// A newer daemon copy kept as it is keeps its launcher too.
    #[test]
    fn a_kept_newer_daemon_keeps_its_launcher() {
        let mut s = set_up_mac();
        s.daemon_bin = BinProbe::File {
            sha256: "9".repeat(64),
            uid: 451,
            gid: 451,
            mode: 0o100_755,
            version: Some("9.9.9+gfffffff".into()),
        };
        if let BinProbe::File { sha256, .. } = &mut s.vm_runtime.launcher {
            *sha256 = "8".repeat(64);
        }
        assert!(!actions(&s).contains(&VmAction::InstallLauncher));
        // And its agent: the three are one release.
        if let BinProbe::File { sha256, .. } = &mut s.vm_runtime.agent {
            *sha256 = "8".repeat(64);
        }
        assert!(!actions(&s).contains(&VmAction::InstallAgent));
    }

    #[test]
    fn the_vm_directories_are_created_fixed_or_refused() {
        let mut s = set_up_mac();
        s.vm_runtime.vm_dirs[0].1 = DirProbe::Missing;
        s.vm_runtime.vm_dirs[1].1 = dir(vm(), 0o755);
        let got = actions(&s);
        assert!(got.contains(&VmAction::EnsureVmDir(VmDir::Run)));
        assert!(got.contains(&VmAction::EnsureVmDir(VmDir::State)));
        // A root-owned one could have been moved in from elsewhere: refused,
        // never given away.
        s.vm_runtime.vm_dirs[2].1 = dir(Account { uid: 0, gid: 0 }, 0o700);
        let st = find(&vm_steps(&s), "/var/db/faber-vm/clones").clone();
        assert_eq!(st.status, Status::Refused);
        s.vm_runtime.vm_dirs[3].1 = DirProbe::NotADir("a symlink".into());
        assert_eq!(
            find(&vm_steps(&s), "/var/db/faber-vm/linux").status,
            Status::Refused
        );
        // `run/` is `_faber_vm:_faber` 0710.
        let shown = find(&vm_steps(&set_up_mac()), "/var/db/faber-vm/run")
            .detail
            .clone();
        assert_eq!(shown, "_faber_vm:_faber 710");
    }

    #[test]
    fn a_host_address_in_a_vm_subnet_is_refused() {
        let mut s = set_up_mac();
        s.vm_runtime.host_addrs = Ok(vec!["192.168.243.9".parse().unwrap()]);
        let st = find(&vm_steps(&s), paths::PLAN).clone();
        assert_eq!(st.status, Status::Refused);
        assert!(
            st.detail.contains("192.168.243.9 in 192.168.243.0/24"),
            "{}",
            st.detail
        );
    }

    /// A new grant re-renders the stored rules through `sudo faber grant`'s
    /// function and restarts the checker; the plan is untouched.
    #[test]
    fn a_new_grant_re_stores_the_rules_and_restarts_the_checker() {
        let mut s = set_up_mac();
        let g = Grants::default()
            .with_grant(Grant {
                profile: "mac".into(),
                to: "10.0.0.5/32".parse().unwrap(),
                port: 5432,
                proto: Proto::Tcp,
            })
            .unwrap();
        s.vm_runtime.grants = Ok(Some(g.to_toml()));
        let got = actions(&s);
        assert!(got.contains(&VmAction::WriteStored), "{got:?}");
        assert!(got.contains(&VmAction::InstallAnchor { restart: true }));
        assert!(!got.contains(&VmAction::WritePlan));
        assert!(!got
            .iter()
            .any(|a| matches!(a, VmAction::InstallSupervisor { .. })));
        // What would be stored is exactly `render_wall`'s.
        let plan = network_plan(&config());
        let want = render_wall(&g, &config(), Some(&plan))
            .unwrap()
            .stored
            .unwrap();
        assert_eq!(
            desired_stored(&plan, Some(&g.to_toml()), &config())
                .unwrap()
                .0,
            want
        );
        // A grants file that does not parse is refused.
        s.vm_runtime.grants = Ok(Some("[grants]\n".into()));
        assert_eq!(find(&vm_steps(&s), paths::STORED).status, Status::Refused);
    }

    /// A new profile changes the plan, so the supervisor, which reads it at
    /// start, restarts too.
    #[test]
    fn a_new_profile_rewrites_the_plan_and_restarts_the_supervisor() {
        let mut s = set_up_mac();
        if let Ok(c) = &mut s.vm_runtime.config {
            c.vm_profiles.push("mac-big".into());
            c.profiles.push("mac-big".into());
        }
        let got = actions(&s);
        assert!(got.contains(&VmAction::WritePlan));
        assert!(got.contains(&VmAction::WriteStored));
        assert!(got.contains(&VmAction::InstallSupervisor { restart: true }));
        assert!(got.contains(&VmAction::InstallAnchor { restart: true }));
    }

    /// A new CLI copy restarts the checker, which runs it.
    #[test]
    fn a_new_cli_copy_restarts_the_checker() {
        let mut s = set_up_mac();
        s.cli_bin = BinProbe::Missing;
        let got = actions(&s);
        assert_eq!(got, vec![VmAction::InstallAnchor { restart: true }]);
    }

    #[test]
    fn a_changed_or_stopped_plist_is_rewritten_or_started() {
        let mut s = set_up_mac();
        s.vm_runtime.anchor.installed = Some("old".into());
        s.vm_runtime.supervisor.loaded = false;
        let got = actions(&s);
        assert!(got.contains(&VmAction::InstallAnchor { restart: true }));
        assert!(got.contains(&VmAction::InstallSupervisor { restart: false }));
    }

    /// A checker or supervisor that is loaded but that launchd cannot spawn
    /// is reloaded, so re-running setup heals it (#479): `loaded` alone
    /// read it as running and planned nothing.
    #[test]
    fn a_loaded_job_launchd_cannot_spawn_is_reloaded() {
        let mut s = set_up_mac();
        assert!(actions(&s).is_empty());
        let why = "state spawn scheduled, last exit code 78: EX_CONFIG";
        s.vm_runtime.anchor.stuck = Some(why.into());
        s.vm_runtime.supervisor.stuck = Some(why.into());
        let got = actions(&s);
        assert!(got.contains(&VmAction::InstallAnchor { restart: true }));
        assert!(got.contains(&VmAction::InstallSupervisor { restart: true }));
        let shown = render(&s, &plan(&s));
        assert!(shown.contains("78: EX_CONFIG"), "{shown}");
    }

    #[test]
    fn imports_are_planned_kept_and_refused() {
        use crate::setup::vm_images::{GoldenProbe, StoredBundle, TarProbe, GOLDEN_FILES};
        let files: Vec<(String, String)> = GOLDEN_FILES
            .iter()
            .map(|f| (f.to_string(), "a".repeat(64)))
            .collect();
        let mut s = set_up_mac();
        s.vm_runtime.golden = Some(GoldenProbe {
            path: "/Users/owner/golden".into(),
            name: Ok("golden".into()),
            source: Ok(files.clone()),
            installed: StoredBundle::Missing,
        });
        s.vm_runtime.runner_tar = Some(TarProbe {
            path: "/Users/owner/runner.tar".into(),
            name: Ok("runner.tar".into()),
            source: Ok(("b".repeat(64), 4096)),
            installed: None,
        });
        let got = actions(&s);
        assert!(got.contains(&VmAction::ImportGolden { replace: false }));
        assert!(got.contains(&VmAction::ImportRunnerTar));
        let v = vm_steps(&s);
        let g = find(&v, "/var/db/faber/images/golden");
        assert!(g.detail.contains("image = \"golden\""), "{}", g.detail);
        assert!(g.detail.contains("_faber_vm reads it"), "{}", g.detail);
        assert!(find(&v, "/var/db/faber/images/runner/runner.tar")
            .detail
            .contains("loads it into the Linux VM"));
        // Re-run: both in place, nothing to do.
        if let Some(g) = &mut s.vm_runtime.golden {
            g.installed = StoredBundle::Present(Ok(files));
        }
        if let Some(t) = &mut s.vm_runtime.runner_tar {
            t.installed = Some(Ok("b".repeat(64)));
        }
        assert!(actions(&s).is_empty());
        // A different image of that name is refused without --replace-golden.
        if let Some(g) = &mut s.vm_runtime.golden {
            g.source = Ok(GOLDEN_FILES
                .iter()
                .map(|f| (f.to_string(), "c".repeat(64)))
                .collect());
        }
        assert_eq!(
            find(&vm_steps(&s), "/var/db/faber/images/golden").status,
            Status::Refused
        );
        s.opts.vm.replace_golden = true;
        assert!(actions(&s).contains(&VmAction::ImportGolden { replace: true }));
    }

    #[test]
    fn the_image_store_must_be_roots() {
        let mut s = set_up_mac();
        s.vm_runtime.image_dirs[0].1 = dir(faber(), 0o755);
        assert_eq!(
            find(&vm_steps(&s), "/var/db/faber/images").status,
            Status::Refused
        );
        let mut s = set_up_mac();
        s.vm_runtime.image_dirs[0].1 = DirProbe::Missing;
        assert_eq!(actions(&s), vec![VmAction::EnsureImageStore]);
    }
}
