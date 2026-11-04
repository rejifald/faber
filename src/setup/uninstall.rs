//! `sudo faber uninstall` (ADR-0025): remove what `sudo faber setup` created,
//! driven by the setup record's `artifacts`.
//!
//! The order is fixed, and tested ([`uninstall_plan`]):
//!
//! 1. drain the daemon, then stop the service and remove its definition
//! 2. with `--keep-logs`, copy `audit.jsonl` and `config.toml` to the
//!    operator's home, owned by them
//! 3. on macOS, stop and remove the VM supervisor's and the wall checker's
//!    LaunchDaemons, and flush the wall's pf anchor (ADR-0026)
//! 4. remove the files and directories, the stored GitHub token (ADR-0027)
//!    among them, and the setup record last
//! 5. delete the accounts, then their groups
//!
//! ADR-0051's session account, when there is one: before the credentials and
//! the record, its session is logged out, auto-login and the login window's
//! "Other…" setting are put back, and its home goes; with the accounts, it is
//! deleted first, and the FileVault screen's user list is refreshed if it
//! held a SecureToken ([`super::session`]).
//!
//! **The image store is never touched.** Golden images, runner-image tarballs
//! and the Linux VM's image in [`IMAGE_STORE`] are local-only artifacts
//! (AGENTS.md): nothing could re-pull them. The plan says they are kept, and
//! where, and the operator deletes them by hand if they mean to.
//!
//! **Nor is `_faber_vm`'s `linux/`** ([`kept_linux_dir`]): it holds the Linux
//! VM's disk, and on it the only copy of the local-only runner image
//! (launcher/README.md). Uninstall removes every other entry of `_faber_vm`'s
//! home, never following a symlink, then the home itself only if it is empty
//! ([`UStep::RemoveVmHome`]). Deleting the account afterwards keeps the home
//! either way: macOS deletes the record with `dscl . -delete`, Linux runs
//! `userdel` without `-r`, and neither touches files.
//!
//! Everything else of the VMs goes: the launcher's directory,
//! `/var/db/faber/bin` (outside `_faber`'s state directory, so `_faber_vm` can
//! run it; not in the store), the wall's files and the tools-disk cache, which
//! faber rebuilds. So does the pinned Docker CLI the daemon ran,
//! [`DOCKER_CLI`], and its directory if nothing else is in it.
//!
//! **The Linux engine is stopped, not refused** ([`UStep::StopEngine`]): once
//! faber's service is stopped (so nothing starts the engine again), the
//! root-owned CLI copy runs `faber engine stop` as `_faber`, the only account
//! the supervisor serves, which stops the VM through the supervisor and waits
//! for it, before any LaunchDaemon goes. The VM's disk is not touched.
//!
//! **Any other running VM refuses the uninstall** ([`VM_CLAIMS_REFUSAL`]).
//! `AbandonProcessGroup` lets each VM's launcher process outlive the
//! supervisor, so stopping the supervisor's daemon does not stop them, and
//! removing their state and clones under them would leave VMs nobody can
//! adopt, stop or sweep. Uninstall has no safe way to stop them itself: only
//! the supervisor may signal a VM's child, and only one it can prove is that
//! VM's (launcher/README.md), and a VM whose job is mid-run is the drain's to
//! finish, not root's to kill. So while any `slot-N.claim` marker is in
//! `_faber_vm`'s state directory, the plan is refused before anything is
//! touched, with what to do instead.
//!
//! Files go before accounts because macOS may refuse to delete an account from
//! a terminal hosted inside another app (TCC's `eDSPermissionError`), and a
//! re-run from Terminal.app must still be able to finish: with the record
//! already gone, it finds faber's accounts by name and shape and deletes only
//! those. The run is idempotent: whatever is already gone is skipped.
//!
//! Uninstall runs as root and deletes what the record names, so it only acts
//! on a record the trust rule accepts, and even then only on paths inside the
//! SYSTEM layout and on accounts named and shaped like faber's.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use super::accounts::{self, GroupInfo, UserInfo, ID_RANGE};
use super::exe::{self, PathFacts, Untrusted};
use super::fsx::{self, Dir, SourceRefusal};
use super::plan::{OperatorInfo, RecordProbe};
use super::tools::Runner;
use crate::platform::layout::{
    Os, SystemLayout, DOCKER_CLI, FABER_ACCOUNT, FABER_OPERATOR_GROUP, FABER_VM_ACCOUNT,
    IMAGE_STORE,
};
use crate::platform::record::{Artifact, SetupRecord};
use crate::platform::service::{self, launchd, systemd};

/// Everything the uninstall plan needs to know.
#[derive(Debug, Clone, PartialEq)]
pub struct UninstallState {
    pub os: Os,
    pub layout: SystemLayout,
    pub privileged: bool,
    pub operator: Result<OperatorInfo, String>,
    pub exe_path: PathBuf,
    pub exe_chain: Result<Vec<PathFacts>, String>,
    pub untrusted: Untrusted,
    pub record: RecordProbe,
    /// Which of the paths uninstall might remove exist now.
    pub existing: Vec<PathBuf>,
    pub service_loaded: bool,
    /// faber's accounts and groups, as they exist now.
    pub users: Vec<UserInfo>,
    pub groups: Vec<GroupInfo>,
    pub keep_logs: bool,
    /// `--no-drain`: stop the daemon without asking it to drain first.
    pub no_drain: bool,
    /// What setup moved aside in the operator's home, and in the home of the
    /// account it migrated from, for the closing note.
    pub restores: Vec<Restore>,
    /// macOS: the VM runtime setup installed (ADR-0026).
    pub vm: VmUninstall,
    /// macOS: ADR-0051's session account.
    pub session: super::session::SessionUninstall,
    /// macOS: something is at the menu-bar app's path
    /// ([`super::app::app_path`]). It is the operator's, and left.
    pub menubar_app: bool,
}

/// What uninstall finds of the VM runtime, on macOS.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VmUninstall {
    /// The `slot-N.claim` markers in `_faber_vm`'s state directory: VMs the
    /// supervisor started and has not torn down.
    pub claims: Vec<String>,
    /// Why the state directory could not be read, when it could not.
    pub claims_unknown: Option<String>,
    pub supervisor_plist: bool,
    pub supervisor_loaded: bool,
    pub anchor_plist: bool,
    pub anchor_loaded: bool,
    /// Which of [`vm_paths`] exist.
    pub existing: Vec<PathBuf>,
    /// The image store exists, and is kept.
    pub image_store: bool,
    /// [`kept_linux_dir`] exists, and is kept.
    pub linux_disk: bool,
    /// The pinned Docker CLI, [`DOCKER_CLI`], exists.
    pub docker_cli: bool,
    /// Its directory exists: removed after it, if nothing else is in it.
    pub docker_cli_dir: bool,
}

/// The plan's line for the menu-bar app setup installed: the operator's own,
/// which uninstall leaves.
pub fn menubar_app_left() -> String {
    let app = super::app::app_path();
    format!(
        "{}: the menu-bar app, left: it is yours. To remove it: pkill -x {}; rm -rf \"{}\"",
        app.display(),
        super::app::APP_EXECUTABLE,
        app.display()
    )
}

/// `_faber_vm`'s `linux/`: the Linux VM's bundle, whose disk holds the only
/// copy of the runner image. Uninstall never removes it or anything in it.
pub fn kept_linux_dir(layout: &SystemLayout) -> PathBuf {
    layout.vm_home.join(super::vm_runtime::VmDir::Linux.name())
}

/// The plan's line for [`kept_linux_dir`].
pub fn linux_disk_kept(layout: &SystemLayout) -> String {
    format!(
        "{}: kept: the Linux VM's disk, which holds the runner image; export it (`docker save`) \
         before deleting it by hand",
        kept_linux_dir(layout).display()
    )
}

/// The VM runtime's own files and directories outside the SYSTEM layout's
/// trees, which uninstall removes: the wall's files, the verdict directory,
/// the tools-disk cache, the launcher's directory and the two daemons' logs.
/// Never anything in [`IMAGE_STORE`].
pub fn vm_paths() -> Vec<PathBuf> {
    use crate::platform::layout::{LAUNCHER_DIR, TOOLS_DISKS};
    use crate::wall::paths;
    vec![
        PathBuf::from(paths::WALL_DIR),
        PathBuf::from(paths::VERDICT_DIR),
        PathBuf::from(TOOLS_DISKS),
        PathBuf::from(LAUNCHER_DIR),
        PathBuf::from(service::WALL_LOG),
        PathBuf::from(service::SUPERVISOR_LOG),
    ]
}

/// Why uninstall stops while VMs are claimed (see the module docs).
pub const VM_CLAIMS_REFUSAL: &str = "the VM supervisor still holds VMs, and stopping its daemon \
    would not stop them (AbandonProcessGroup), while removing their state and clones would \
    orphan them. Let them finish first: `faber drain` stops new jobs and waits for running ones, \
    and the supervisor tears each VM down when it exits. If a marker stays with no `faber-vm \
    run` process left (`pgrep -lU _faber_vm faber-vm` prints nothing), it is stale: remove it \
    with `sudo rm`, then re-run";

/// The `slot-N.claim` names among a state directory's entries, sorted.
pub fn claim_markers(names: &[String]) -> Vec<String> {
    let mut out: Vec<String> = names
        .iter()
        .filter(|n| {
            n.strip_prefix("slot-")
                .and_then(|r| r.strip_suffix(".claim"))
                .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
        })
        .cloned()
        .collect();
    out.sort();
    out
}

/// What `sudo faber setup` moved aside in one account's home: its LEGACY
/// directories and its per-user definition, each renamed to
/// `<name>.pre-system-<stamp>`. Putting back the definition alone is not
/// enough: the agent it loads runs a binary, and reads a config, from inside
/// those directories.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restore {
    pub account: String,
    pub uid: u32,
    /// `(backup, original)`: the newest backup of each original, the
    /// directories first and the definition last.
    pub renames: Vec<(PathBuf, PathBuf)>,
    /// The definition, when its backup is among `renames`: what to load once
    /// everything is back.
    pub definition: Option<PathBuf>,
    /// `(backup, original)` where the original exists again: never renamed
    /// over, only mentioned.
    pub blocked: Vec<(PathBuf, PathBuf)>,
}

/// What removing the stored token does not do.
pub const REVOKE_NOTE: &str = "the token itself stays valid on GitHub until you revoke it \
     there, under Settings, Developer settings, Personal access tokens";

/// What `--no-drain` costs, said before it is confirmed.
pub const NO_DRAIN_WARNING: &str = "--no-drain: the daemon is stopped without draining. Every \
     job it is running is cut off: its runner is orphaned on GitHub until it ages out, and its \
     container is left behind for nobody to tear down. Use it only when the daemon cannot drain \
     (it does not answer, or will not finish).";

/// One uninstall step, in the order they run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UStep {
    /// Drain the daemon on loopback, waiting for admitted jobs.
    Drain,
    /// `--no-drain`: stop without draining, after an explicit confirmation.
    SkipDrain,
    StopService,
    /// macOS: stop faber's Linux VM through the supervisor, as `_faber`
    /// (`faber engine stop`), and check its claim marker is gone. Its disk
    /// stays.
    StopEngine,
    RemoveUnit(PathBuf),
    KeepLogs {
        to: PathBuf,
    },
    /// A directory and everything in it.
    RemoveTree(PathBuf),
    /// `_faber_vm`'s home: every entry but `keep`, never following a
    /// symlink, then the home itself only if that left it empty.
    RemoveVmHome {
        home: PathBuf,
        keep: String,
    },
    RemoveFile(PathBuf),
    /// ADR-0027's credential store, and the GitHub token in it. Never in the
    /// record's artifacts: `sudo faber auth set` creates it, not setup.
    RemoveCredentials(PathBuf),
    RemoveRecord(PathBuf),
    /// `/etc/faber`, only if nothing else lives there.
    RemoveEtcDir(PathBuf),
    /// A root-owned file faber installed outside its trees: the Docker CLI.
    /// Removed through descriptors walked from `/` ([`remove_in_root_chain`]),
    /// refused when a directory above it is not root's alone.
    RemoveRootFile(PathBuf),
    /// A directory faber created outside its trees, only if nothing else
    /// lives there: the Docker CLI's. Removed like [`UStep::RemoveRootFile`].
    RemoveDirIfEmpty(PathBuf),
    /// Stop a LaunchDaemon in the `system` domain, by label.
    StopJob(String),
    /// macOS: flush the wall's pf anchor, leaving the operator's main
    /// ruleset alone.
    FlushAnchor,
    DeleteUser(String),
    DeleteGroup(String),
    /// macOS: one of the session account's steps (ADR-0051).
    Session(super::session::SessionUndo),
    /// Something uninstall leaves, and why.
    Leave(String),
}

impl UStep {
    fn render(&self, os: Os) -> String {
        match self {
            UStep::Drain => "drain       the control socket, waiting for admitted jobs".to_string(),
            UStep::SkipDrain => format!("DO NOT DRAIN {NO_DRAIN_WARNING}"),
            UStep::StopService => match os {
                Os::MacOs => format!("stop        {}", service::system_target()),
                Os::Linux => format!("stop        {} (disable --now)", service::UNIT_NAME),
            },
            UStep::StopEngine => format!(
                "stop        faber's Linux VM through the supervisor, as {FABER_ACCOUNT} \
                 (`faber engine stop`); its disk stays"
            ),
            UStep::RemoveUnit(p)
            | UStep::RemoveFile(p)
            | UStep::RemoveRootFile(p)
            | UStep::RemoveRecord(p) => {
                format!("remove      {}", p.display())
            }
            UStep::KeepLogs { to } => format!(
                "copy        audit.jsonl and config.toml to {}",
                to.display()
            ),
            UStep::RemoveTree(p) => format!("remove      {} and everything in it", p.display()),
            UStep::RemoveVmHome { home, keep } => format!(
                "remove      everything in {} but {keep}/, then the directory if it is empty",
                home.display()
            ),
            UStep::RemoveCredentials(p) => format!(
                "remove      {} and the GitHub token in it ({REVOKE_NOTE})",
                p.display()
            ),
            UStep::RemoveEtcDir(p) | UStep::RemoveDirIfEmpty(p) => {
                format!("remove      {} if nothing else is in it", p.display())
            }
            UStep::StopJob(label) => {
                format!("stop        {}", service::system_target_of(label))
            }
            UStep::FlushAnchor => format!(
                "flush       the pf anchor {} (/sbin/pfctl -a {} -F all)",
                crate::wall::ANCHOR,
                crate::wall::ANCHOR
            ),
            UStep::DeleteUser(n) => format!("delete      user {n}"),
            UStep::DeleteGroup(n) => format!("delete      group {n}"),
            UStep::Session(undo) => {
                use super::session::{list_crypto_users, update_preboot, SessionUndo, KCPASSWORD};
                use crate::platform::layout::FABER_MAC_ACCOUNT;
                match undo {
                    SessionUndo::Logout { uid } => format!(
                        "log out     {FABER_MAC_ACCOUNT}'s login session (launchctl bootout \
                         gui/{uid})"
                    ),
                    SessionUndo::AutoLoginOff => format!(
                        "turn off    auto-login as {FABER_MAC_ACCOUNT}, and remove {KCPASSWORD}"
                    ),
                    SessionUndo::RestoreShowOther => {
                        "restore     the login window's \"Other…\" setting from before setup"
                            .to_string()
                    }
                    SessionUndo::UpdatePreboot => format!(
                        "update      the FileVault screen's users ({})",
                        update_preboot().display()
                    ),
                    SessionUndo::CheckCryptoUser { uuid } => format!(
                        "verify      {FABER_MAC_ACCOUNT} ({uuid}) can no longer unlock the disk: \
                         `{}` does not list it",
                        list_crypto_users().display()
                    ),
                }
            }
            UStep::Leave(why) => format!("leave       {why}"),
        }
    }
}

/// Paths uninstall may remove: the SYSTEM layout's trees and files, and
/// definitions named after faber's label or unit. Anything else a record
/// names is left alone, however it got there.
pub fn is_ours(path: &Path, layout: &SystemLayout) -> bool {
    use std::path::Component;
    if !path.is_absolute()
        || path
            .components()
            .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    {
        return false;
    }
    // The local-only images are never uninstall's, whatever names them.
    let store = Path::new(IMAGE_STORE);
    if path.starts_with(store) || path.starts_with(super::vm_images::canonical(store)) {
        return false;
    }
    // Nor the Linux VM's disk, the runner image's only copy.
    if path.starts_with(kept_linux_dir(layout)) {
        return false;
    }
    let exact = [&layout.cli_bin, &layout.service_unit, &layout.setup_record];
    let trees = [
        &layout.state_dir,
        &layout.log_dir,
        &layout.cache_dir,
        &layout.vm_home,
        &layout.etc_dir,
    ];
    if exact.contains(&&path.to_path_buf()) || trees.iter().any(|t| path.starts_with(t)) {
        return true;
    }
    let (Some(parent), Some(name)) = (path.parent(), path.file_name().and_then(|n| n.to_str()))
    else {
        return false;
    };
    let definitions = layout.service_unit.parent();
    Some(parent) == definitions && (name.starts_with(service::LABEL) || name.starts_with("faber"))
}

/// Whether an existing account is one uninstall may delete: named like
/// faber's, with an id in faber's range and, with a record, the id setup
/// recorded.
fn account_is_ours(user: &UserInfo, record: Option<&SetupRecord>) -> Result<(), String> {
    if user.name != FABER_ACCOUNT && user.name != FABER_VM_ACCOUNT {
        return Err(format!("user {}: not one of faber's accounts", user.name));
    }
    if !ID_RANGE.contains(&user.uid) {
        return Err(format!(
            "user {} has uid {}, outside {}-{}: not the account setup creates",
            user.name,
            user.uid,
            ID_RANGE.start(),
            ID_RANGE.end()
        ));
    }
    if let Some(r) = record {
        let recorded = if user.name == FABER_ACCOUNT {
            r.faber_uid()
        } else {
            r.faber_vm_uid()
        };
        if user.uid != recorded {
            return Err(format!(
                "user {} has uid {}, but the record says {recorded}",
                user.name, user.uid
            ));
        }
    }
    Ok(())
}

fn group_is_ours(group: &GroupInfo) -> Result<(), String> {
    if ![FABER_ACCOUNT, FABER_VM_ACCOUNT, FABER_OPERATOR_GROUP].contains(&group.name.as_str()) {
        return Err(format!("group {}: not one of faber's groups", group.name));
    }
    if !ID_RANGE.contains(&group.gid) {
        return Err(format!(
            "group {} has gid {}, outside {}-{}: not the group setup creates",
            group.name,
            group.gid,
            ID_RANGE.start(),
            ID_RANGE.end()
        ));
    }
    Ok(())
}

/// The artifacts to act on without a record: what a setup on this OS
/// creates, less the CLI copy, which an install from before ADR-0025 put at
/// the same path.
fn derived_artifacts(u: &UninstallState) -> Vec<Artifact> {
    super::plan::artifacts(u.os, &u.layout)
        .into_iter()
        .filter(|a| !matches!(a, Artifact::File { path } if *path == u.layout.cli_bin))
        .collect()
}

/// The uninstall plan, in execution order. Pure.
pub fn uninstall_plan(u: &UninstallState, ts: &str) -> Result<Vec<UStep>, String> {
    match &u.exe_chain {
        Ok(chain) => exe::verdict(chain, &u.untrusted).map_err(|r| exe::refusal(&r))?,
        Err(e) => return Err(format!("cannot resolve this binary: {e}")),
    }
    let (artifacts, record) = match &u.record {
        RecordProbe::Untrusted(why) => {
            return Err(format!(
                "the setup record {why}, so uninstall will not act on it; run `sudo faber setup` \
                 to rewrite it, then uninstall"
            ))
        }
        RecordProbe::Trusted(r) => (r.artifacts.clone(), Some(r)),
        RecordProbe::Absent => (derived_artifacts(u), None),
    };
    let layout = &u.layout;
    let exists = |p: &Path| u.existing.iter().any(|e| e == p);
    let mut steps = Vec::new();
    let mut leave = Vec::new();
    let mut engine_runs = false;
    if u.os == Os::MacOs {
        let engine = format!("slot-{}.claim", crate::vm::engine::LINUX_SLOT);
        let others: Vec<&String> = u.vm.claims.iter().filter(|c| **c != engine).collect();
        // The engine is stopped through the supervisor, so only a running
        // supervisor can do it: a marker with none is stale.
        let engine_stoppable = u.vm.supervisor_loaded && u.vm.supervisor_plist;
        if !others.is_empty() || (u.vm.claims.contains(&engine) && !engine_stoppable) {
            return Err(format!(
                "{} VM claim marker(s) in {}: {}. {VM_CLAIMS_REFUSAL}",
                u.vm.claims.len(),
                layout.vm_home.join("state").display(),
                u.vm.claims.join(", ")
            ));
        }
        engine_runs = u.vm.claims.contains(&engine);
        if let (Some(why), true) = (&u.vm.claims_unknown, u.privileged) {
            return Err(format!(
                "cannot read {} to tell whether VMs still run ({why}), so uninstall will not \
                 remove what they run from",
                layout.vm_home.join("state").display()
            ));
        }
    }

    // 1. The service.
    let is_service = |a: &Artifact| match a {
        Artifact::LaunchdJob { label } => label == service::LABEL,
        Artifact::SystemdUnit { name } => name == service::UNIT_NAME,
        _ => false,
    };
    if u.service_loaded && artifacts.iter().any(is_service) {
        steps.push(if u.no_drain {
            UStep::SkipDrain
        } else {
            UStep::Drain
        });
        steps.push(UStep::StopService);
    }
    // With faber stopped nothing starts the engine again. Without a service
    // to stop there is nothing that would, and the step still runs first.
    if engine_runs {
        steps.push(UStep::StopEngine);
    }
    let lists = |p: &Path, dir: bool| {
        artifacts.iter().any(|a| match a {
            Artifact::File { path } => !dir && path == p,
            Artifact::Dir { path } => dir && path == p,
            _ => false,
        })
    };
    if lists(&layout.service_unit, false) && exists(&layout.service_unit) {
        steps.push(UStep::RemoveUnit(layout.service_unit.clone()));
    }

    // 2. What the operator keeps.
    if u.keep_logs {
        let op = u
            .operator
            .as_ref()
            .map_err(|e| format!("--keep-logs needs to know the operator: {e}"))?;
        let audit = layout.log_dir.join("audit.jsonl");
        if exists(&audit) || exists(&layout.config_path()) {
            steps.push(UStep::KeepLogs {
                to: op.home.join(format!("faber-uninstall-{ts}")),
            });
        }
    }

    // 3. The VM runtime's daemons, macOS only: the supervisor first, since
    //    the daemon that used it is stopped, then the wall's checker, then
    //    its anchor.
    if u.os == Os::MacOs {
        for (label, plist, loaded, present) in [
            (
                service::SUPERVISOR_LABEL,
                service::SUPERVISOR_PLIST,
                u.vm.supervisor_loaded,
                u.vm.supervisor_plist,
            ),
            (
                service::WALL_LABEL,
                service::WALL_PLIST,
                u.vm.anchor_loaded,
                u.vm.anchor_plist,
            ),
        ] {
            if loaded {
                steps.push(UStep::StopJob(label.into()));
            }
            if present {
                steps.push(UStep::RemoveUnit(PathBuf::from(plist)));
            }
        }
        let wall_dir = Path::new(crate::wall::paths::WALL_DIR);
        if u.vm.anchor_plist || u.vm.anchor_loaded || u.vm.existing.iter().any(|p| p == wall_dir) {
            steps.push(UStep::FlushAnchor);
        }
    }

    // 4. Files and directories, the record last.
    for a in &artifacts {
        let (path, dir) = match a {
            Artifact::File { path } => (path, false),
            Artifact::Dir { path } => (path, true),
            _ => continue,
        };
        if *path == layout.service_unit || *path == layout.etc_dir || *path == layout.setup_record {
            continue;
        }
        if path.starts_with(kept_linux_dir(layout)) {
            // Said once below, when it exists.
        } else if dir && *path == layout.vm_home {
            if exists(path) {
                steps.push(UStep::RemoveVmHome {
                    home: path.clone(),
                    keep: super::vm_runtime::VmDir::Linux.name().to_string(),
                });
            }
        } else if !is_ours(path, layout) {
            leave.push(format!(
                "{}: outside the SYSTEM layout, so not removed",
                path.display()
            ));
        } else if exists(path) {
            steps.push(if dir {
                UStep::RemoveTree(path.clone())
            } else {
                UStep::RemoveFile(path.clone())
            });
        }
    }
    if record.is_none() && exists(&layout.cli_bin) {
        leave.push(format!(
            "{}: without a setup record it cannot be told apart from an older install",
            layout.cli_bin.display()
        ));
    }
    if u.os == Os::MacOs {
        for path in vm_paths() {
            if u.vm.existing.contains(&path) {
                steps.push(if path.extension().is_some_and(|e| e == "log") {
                    UStep::RemoveFile(path)
                } else {
                    UStep::RemoveTree(path)
                });
            }
        }
        // The pinned Docker CLI the daemon ran (ADR-0026, amended
        // 2026-10-21), then its directory if nothing else is in it.
        if u.vm.docker_cli {
            steps.push(UStep::RemoveRootFile(PathBuf::from(DOCKER_CLI)));
        }
        if u.vm.docker_cli_dir {
            if let Some(dir) = Path::new(DOCKER_CLI).parent() {
                steps.push(UStep::RemoveDirIfEmpty(dir.to_path_buf()));
            }
        }
        if u.vm.linux_disk {
            leave.push(linux_disk_kept(layout));
        }
        if u.menubar_app {
            leave.push(menubar_app_left());
        }
        if u.vm.image_store {
            leave.push(format!(
                "{IMAGE_STORE}: kept. Golden images, runner-image tarballs and the Linux VM's \
                 image are local-only artifacts nothing could re-pull (AGENTS.md); delete them \
                 yourself if you are sure"
            ));
        }
    }
    session_steps(&u.session, &mut steps, &mut leave);
    if exists(&layout.credentials_dir) {
        steps.push(UStep::RemoveCredentials(layout.credentials_dir.clone()));
    }
    if exists(&layout.setup_record) {
        steps.push(UStep::RemoveRecord(layout.setup_record.clone()));
    }
    if lists(&layout.etc_dir, true) && exists(&layout.etc_dir) {
        steps.push(UStep::RemoveEtcDir(layout.etc_dir.clone()));
    }

    // 5. Accounts, then groups, newest first. The session account is the
    // newest of all: setup makes it after the record.
    session_account_steps(&u.session, &mut steps, &mut leave);
    for a in artifacts.iter().rev() {
        if let Artifact::User { name } = a {
            if let Some(user) = u.users.iter().find(|x| &x.name == name) {
                match account_is_ours(user, record) {
                    Ok(()) => steps.push(UStep::DeleteUser(name.clone())),
                    Err(why) => leave.push(why),
                }
            }
        }
    }
    for a in artifacts.iter().rev() {
        if let Artifact::Group { name } = a {
            if let Some(group) = u.groups.iter().find(|x| &x.name == name) {
                match group_is_ours(group) {
                    Ok(()) => steps.push(UStep::DeleteGroup(name.clone())),
                    Err(why) => leave.push(why),
                }
            }
        }
    }

    for a in &artifacts {
        match a {
            Artifact::Other(v) => leave.push(format!(
                "{v}: an entry this release cannot read, so it does not guess what it names"
            )),
            Artifact::LaunchdJob { label } if label != service::LABEL => leave.push(format!(
                "launchd job {label}: not one this release installs"
            )),
            Artifact::SystemdUnit { name } if name != service::UNIT_NAME => leave.push(format!(
                "systemd unit {name}: not one this release installs"
            )),
            _ => {}
        }
    }
    steps.extend(leave.into_iter().map(UStep::Leave));
    Ok(steps)
}

/// ADR-0051's session account, the files' part: its session logged out,
/// auto-login and the login window put back, its home. Before `/etc/faber`
/// goes, since the login window's kept setting is there.
fn session_steps(
    s: &super::session::SessionUninstall,
    steps: &mut Vec<UStep>,
    leave: &mut Vec<String>,
) {
    use super::session::SessionUndo;
    use crate::platform::layout::{FABER_MAC_ACCOUNT, FABER_MAC_HOME};
    let ours = session_account(s);
    if let Some(Ok(uid)) = ours {
        if s.session_up {
            steps.push(UStep::Session(SessionUndo::Logout { uid }));
        }
    }
    if s.autologin_ours {
        steps.push(UStep::Session(SessionUndo::AutoLoginOff));
    }
    if s.show_other_saved {
        steps.push(UStep::Session(SessionUndo::RestoreShowOther));
    }
    if s.ssh_deny {
        steps.push(UStep::RemoveRootFile(super::session::ssh_deny_real()));
    }
    match ours {
        Some(Ok(_)) if s.home => steps.push(UStep::RemoveTree(PathBuf::from(FABER_MAC_HOME))),
        Some(Err(why)) => leave.push(why),
        None if s.home => leave.push(format!(
            "{FABER_MAC_HOME}: no {FABER_MAC_ACCOUNT} account owns it, so uninstall does not \
             guess whose it is"
        )),
        _ => {}
    }
}

/// The session account's uid when it is the one setup makes, why not when it
/// is not, and `None` without one.
fn session_account(s: &super::session::SessionUninstall) -> Option<Result<u32, String>> {
    s.user
        .as_ref()
        .map(|user| super::session::account_is_ours(user, s.admin).map(|()| user.uid))
}

/// ADR-0051's session account, the accounts' part: the account, then the
/// FileVault screen's user list when it held a SecureToken, then the check
/// that the volume no longer lists it as a crypto user. Deleting the record
/// is what removes that (seen on macOS 27.0, 2026-10-25), so uninstall
/// verifies rather than removes, and fails loudly with the command if it is
/// still there.
fn session_account_steps(
    s: &super::session::SessionUninstall,
    steps: &mut Vec<UStep>,
    leave: &mut Vec<String>,
) {
    use super::session::{list_crypto_users, SessionUndo};
    use crate::platform::layout::FABER_MAC_ACCOUNT;
    if let Some(Ok(_)) = session_account(s) {
        steps.push(UStep::DeleteUser(FABER_MAC_ACCOUNT.into()));
        if s.secure_token {
            steps.push(UStep::Session(SessionUndo::UpdatePreboot));
        }
        match &s.generated_uid {
            Some(uuid) => steps.push(UStep::Session(SessionUndo::CheckCryptoUser {
                uuid: uuid.clone(),
            })),
            None => leave.push(format!(
                "{FABER_MAC_ACCOUNT} has no GeneratedUID to check, so whether it can still \
                 unlock the disk is not verified: `sudo {}` should not list it",
                list_crypto_users().display()
            )),
        }
    }
}

/// The uninstall plan as printed.
pub fn render(u: &UninstallState, steps: &[UStep]) -> String {
    let mut out =
        String::from("faber uninstall: remove faber's accounts and service (ADR-0025)\n\n");
    match &u.record {
        RecordProbe::Trusted(_) => out.push_str(&format!(
            "  record    {} (trusted): removing what it lists\n",
            u.layout.setup_record.display()
        )),
        RecordProbe::Absent => out.push_str(
            "  record    none: removing what exists of the SYSTEM layout, and faber's accounts\n",
        ),
        RecordProbe::Untrusted(why) => out.push_str(&format!("  record    untrusted: {why}\n")),
    }
    out.push('\n');
    if steps.iter().all(|s| matches!(s, UStep::Leave(_))) {
        out.push_str("  nothing to remove: faber is not set up on this host\n");
    }
    for step in steps {
        out.push_str(&format!("  {}\n", step.render(u.os)));
    }
    out
}

/// Gather the uninstall state with read-only probes.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn gather(keep_logs: bool, no_drain: bool, runner: &mut dyn Runner) -> Result<UninstallState> {
    use crate::platform::record::{current_euid, load_trusted};

    let os = Os::HOST;
    let layout = crate::platform::system();
    let privileged = current_euid() == 0;
    let operator = super::probe::operator(os, privileged, runner);
    let exe_path = std::env::current_exe()?;
    let exe_path = std::fs::canonicalize(&exe_path).unwrap_or(exe_path);
    let exe_chain = exe::chain_of(&exe_path).map_err(|e| e.to_string());

    let accounts::FaberAccounts {
        untrusted,
        users,
        groups,
    } = accounts::probe_faber_accounts(os, runner)?;

    let record = match load_trusted(&layout.setup_record) {
        Ok(None) => RecordProbe::Absent,
        Ok(Some(r)) => RecordProbe::Trusted(r),
        Err(e) => RecordProbe::Untrusted(e.to_string()),
    };
    let mut candidates: Vec<PathBuf> = match &record {
        RecordProbe::Trusted(r) => r
            .artifacts
            .iter()
            .filter_map(|a| match a {
                Artifact::File { path } | Artifact::Dir { path } => Some(path.clone()),
                _ => None,
            })
            .collect(),
        _ => super::plan::artifacts(os, &layout)
            .into_iter()
            .filter_map(|a| match a {
                Artifact::File { path } | Artifact::Dir { path } => Some(path),
                _ => None,
            })
            .collect(),
    };
    candidates.extend([
        layout.setup_record.clone(),
        layout.credentials_dir.clone(),
        layout.log_dir.join("audit.jsonl"),
        layout.config_path(),
        layout.cli_bin.clone(),
    ]);
    let existing = candidates
        .into_iter()
        .filter(|p| std::fs::symlink_metadata(p).is_ok())
        .collect();
    let service_loaded = match os {
        Os::MacOs => launchd::is_loaded(runner, &service::system_target()),
        Os::Linux => systemd::is_active(runner) || systemd::is_enabled(runner),
    };
    // The operator's home, and the account setup migrated from: the homes
    // whose directories setup renamed aside. Read-only, and only to print
    // commands the account runs itself.
    let mut restores = Vec::new();
    if let Ok(op) = &operator {
        restores.extend(find_restore(os, &op.name, op.uid, &op.home));
    }
    if let RecordProbe::Trusted(r) = &record {
        let from = r.migrated_from.as_ref().map(|m| m.user.as_str());
        if let Some(from) = from.filter(|f| operator.as_ref().map_or(true, |op| op.name != *f)) {
            if let Ok(Some(user)) = accounts::probe_user(os, runner, from) {
                restores.extend(find_restore(os, &user.name, user.uid, &user.home));
            }
        }
    }
    #[cfg(target_os = "macos")]
    let vm = gather_vm(&layout, runner);
    #[cfg(not(target_os = "macos"))]
    let vm = VmUninstall::default();
    #[cfg(target_os = "macos")]
    let session = super::session::probe::gather_uninstall(&layout, runner)?;
    #[cfg(not(target_os = "macos"))]
    let session = super::session::SessionUninstall::default();
    let menubar_app = os == Os::MacOs && std::fs::symlink_metadata(super::app::app_path()).is_ok();
    Ok(UninstallState {
        vm,
        session,
        menubar_app,
        os,
        layout,
        privileged,
        operator,
        exe_path,
        exe_chain,
        untrusted,
        record,
        existing,
        service_loaded,
        users,
        groups,
        keep_logs,
        no_drain,
        restores,
    })
}

/// The claim markers in the state directory `state`, which is opened without
/// following a symlink and listed through its descriptor: a symlink in its
/// place (to an empty directory, say) must not hide the markers. Missing is
/// no claims; a symlink or anything else that is not a directory is an
/// unreadable state directory, which refuses the uninstall.
fn read_claims(state: &Path) -> (Vec<String>, Option<String>) {
    let opened = fsx::split(state).and_then(|(parent, name)| Dir::open(parent)?.subdir(name));
    match opened.and_then(|d| d.names()) {
        Ok(names) => (claim_markers(&names), None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (Vec::new(), None),
        Err(e) => (
            Vec::new(),
            Some(format!(
                "{e}; it must be a real directory, not a symlink or a file. Remove what is at \
                 {} (`sudo rm`) if nothing runs, then re-run",
                state.display()
            )),
        ),
    }
}

/// The VM runtime as it is, read-only, on macOS.
#[cfg(target_os = "macos")]
fn gather_vm(layout: &SystemLayout, runner: &mut dyn Runner) -> VmUninstall {
    let state = layout.vm_home.join("state");
    let (claims, claims_unknown) = read_claims(&state);
    let there = |p: &str| std::fs::symlink_metadata(p).is_ok();
    VmUninstall {
        claims,
        claims_unknown,
        supervisor_plist: there(service::SUPERVISOR_PLIST),
        supervisor_loaded: launchd::is_loaded(
            runner,
            &service::system_target_of(service::SUPERVISOR_LABEL),
        ),
        anchor_plist: there(service::WALL_PLIST),
        anchor_loaded: launchd::is_loaded(runner, &service::system_target_of(service::WALL_LABEL)),
        existing: vm_paths()
            .into_iter()
            .filter(|p| std::fs::symlink_metadata(p).is_ok())
            .collect(),
        image_store: there(IMAGE_STORE),
        linux_disk: std::fs::symlink_metadata(kept_linux_dir(layout)).is_ok(),
        docker_cli: there(DOCKER_CLI),
        docker_cli_dir: Path::new(DOCKER_CLI)
            .parent()
            .is_some_and(|d| std::fs::symlink_metadata(d).is_ok_and(|m| m.is_dir())),
    }
}

/// What `sudo faber setup` moved aside in `home` for the account `uid`: the
/// newest backup beside each LEGACY directory and beside the per-user
/// definition. `None` when there is nothing to put back.
pub fn find_restore(os: Os, account: &str, uid: u32, home: &Path) -> Option<Restore> {
    home.to_str()?;
    let mut r = Restore {
        account: account.to_string(),
        uid,
        renames: Vec::new(),
        definition: None,
        blocked: Vec::new(),
    };
    let dirs = super::plan::legacy_dirs(&crate::platform::legacy(
        &crate::platform::Env::with_home(home),
    ));
    let definition = super::old::definition_path(os, home);
    let originals = dirs
        .into_iter()
        .map(|d| (d, true))
        .chain([(definition, false)]);
    for (original, is_dir) in originals {
        let Some(backup) = newest_backup(&original, uid, is_dir) else {
            continue;
        };
        if std::fs::symlink_metadata(&original).is_ok() {
            r.blocked.push((backup, original));
        } else {
            if !is_dir {
                r.definition = Some(original.clone());
            }
            r.renames.push((backup, original));
        }
    }
    (!r.renames.is_empty() || !r.blocked.is_empty()).then_some(r)
}

/// The newest `<name>.pre-system-<stamp>` beside `original`: a real
/// directory (`is_dir`) or a regular file that `uid` owns, under a name
/// exactly the shape setup writes. Anything else there is not setup's
/// backup, and its name is never printed as a command.
fn newest_backup(original: &Path, uid: u32, is_dir: bool) -> Option<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let parent = original.parent()?;
    let prefix = format!("{}.pre-system-", original.file_name()?.to_str()?);
    let mut found: Vec<String> = std::fs::read_dir(parent)
        .ok()?
        .filter_map(|e| e.ok()?.file_name().into_string().ok())
        .filter(|n| n.strip_prefix(&prefix).is_some_and(is_stamp))
        .filter(|n| {
            std::fs::symlink_metadata(parent.join(n)).is_ok_and(|m| {
                m.uid() == uid
                    && if is_dir {
                        m.file_type().is_dir()
                    } else {
                        m.file_type().is_file()
                    }
            })
        })
        .collect();
    found.sort();
    found.pop().map(|n| parent.join(n))
}

/// `YYYYMMDDTHHMMSSZ`, the stamp setup writes ([`super::stamp`]): it sorts
/// in time order, and holds nothing a shell would read.
fn is_stamp(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 16
        && b[8] == b'T'
        && b[15] == b'Z'
        && b[..8].iter().chain(&b[9..15]).all(u8::is_ascii_digit)
}

/// `path` quoted for a POSIX shell: in single quotes, where nothing expands,
/// with any single quote in it spelled `'\''`.
fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// The closing note after an uninstall: for each account whose per-user
/// faber setup moved aside, the exact commands that put it back, with the
/// names found on disk. The account runs them itself, never with sudo: root
/// is done acting in that home.
pub fn restore_note(os: Os, restores: &[Restore], operator: Option<&str>) -> String {
    let mut out = String::new();
    for r in restores {
        let whose = if operator == Some(r.account.as_str()) {
            "Your".to_string()
        } else {
            format!("{}'s", r.account)
        };
        if r.renames.is_empty() {
            out.push_str(&format!(
                "\n{whose} per-user faber from before setup was kept, moved aside, and its \
                 originals exist again, so nothing is renamed back:\n"
            ));
        } else {
            out.push_str(&format!(
                "\n{whose} per-user faber from before setup was kept, moved aside. To run it \
                 again, run these as {}, not with sudo:\n",
                r.account
            ));
        }
        for (backup, original) in &r.renames {
            out.push_str(&format!(
                "  mv {} {}\n",
                sh_quote(backup),
                sh_quote(original)
            ));
        }
        if let Some(def) = &r.definition {
            match os {
                Os::MacOs => out.push_str(&format!(
                    "  launchctl bootstrap gui/{} {}\n  # (or log out and back in: launchd \
                     loads it at login)\n",
                    r.uid,
                    sh_quote(def)
                )),
                Os::Linux => out.push_str(&format!(
                    "  systemctl --user daemon-reload\n  systemctl --user enable --now {}\n",
                    service::UNIT_NAME
                )),
            }
        }
        for (backup, original) in &r.blocked {
            out.push_str(&format!(
                "  # skipped: {} exists again, so {} stays where it is\n",
                original.display(),
                backup.display()
            ));
        }
    }
    out
}

fn remove_path(path: &Path, tree: bool) -> Result<()> {
    let result = if tree {
        // std's remove_dir_all never follows a symlink inside the tree.
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match result {
        Ok(()) => {
            println!("  removed {}", path.display());
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(anyhow!("removing {}: {e}", path.display())),
    }
}

/// [`UStep::RemoveRootFile`] (`dir` false) and [`UStep::RemoveDirIfEmpty`]
/// (`dir` true): open `path`'s parent by walking from `/` without following a
/// symlink and without creating anything ([`fsx::root_dir_chain_existing`]),
/// refusing when any directory on the way is not root's alone, then
/// `unlinkat` the file, or remove the directory only if it is empty. Nothing
/// there, or a parent missing, is done already. Never a path-based removal:
/// a directory another account could rename would otherwise choose what root
/// deletes.
fn remove_in_root_chain(path: &Path, dir: bool) -> Result<()> {
    let (parent, name) = fsx::split(path)?;
    let parent_dir = match fsx::root_dir_chain_existing(parent) {
        Ok(Some(d)) => d,
        Ok(None) => return Ok(()),
        Err(e) => bail!("not removing {}: {e}", path.display()),
    };
    let removed = if dir {
        parent_dir.rmdir(name)
    } else {
        parent_dir.unlink(name)
    };
    match removed {
        Ok(()) => println!("  removed {}", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) if dir => println!("  kept {}: {e}", path.display()),
        Err(e) => bail!("removing {}: {e}", path.display()),
    }
    Ok(())
}

/// [`UStep::RemoveVmHome`]: every entry of `home` but `keep`, each removed
/// without following a symlink (a symlink is unlinked, a directory removed
/// with `remove_dir_all`, which never follows one inside), then `home` if it
/// is empty. A `home` that is a symlink or not a directory is left, and said.
fn remove_vm_home(home: &Path, keep: &str) -> Result<()> {
    let dir = match Dir::open_nofollow(home) {
        Ok(d) => d,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => {
            println!(
                "  kept {}: {e}; not removing anything in it",
                home.display()
            );
            return Ok(());
        }
    };
    for name in dir
        .names()
        .with_context(|| format!("listing {}", home.display()))?
    {
        if name == keep {
            continue;
        }
        let path = home.join(&name);
        let is_dir = std::fs::symlink_metadata(&path)
            .map(|m| m.file_type().is_dir())
            .unwrap_or(false);
        remove_path(&path, is_dir)?;
    }
    match std::fs::remove_dir(home) {
        Ok(()) => println!("  removed {}", home.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => println!(
            "  kept {}: it still holds {keep}/, the Linux VM's disk and the runner image on it",
            home.display()
        ),
    }
    Ok(())
}

fn delete_account(runner: &mut dyn Runner, cmd: &super::tools::Cmd) -> Result<()> {
    let out = runner.run(cmd)?;
    if out.success {
        println!("  {}", cmd.display());
        return Ok(());
    }
    if accounts::is_tcc_denial(&out.text()) {
        bail!("{}", accounts::TCC_MESSAGE);
    }
    bail!("`{}` failed: {}", cmd.display(), out.text())
}

/// Copy the audit trail and config to the operator, owned by them, before
/// the trees they live in go.
fn keep_logs(u: &UninstallState, to: &Path) -> Result<()> {
    let op = u.operator.as_ref().map_err(|e| anyhow!("{e}"))?;
    let faber = u
        .users
        .iter()
        .find(|x| x.name == FABER_ACCOUNT)
        .map(|x| x.uid)
        .context("_faber no longer exists, so its files cannot be told apart from others")?;
    let (parent, name) = fsx::split(to)?;
    let home = Dir::open(parent).with_context(|| format!("opening {}", parent.display()))?;
    if !home.mkdir(name)? {
        bail!("{} exists already", to.display());
    }
    let out = home.subdir(name)?;
    let owner = fsx::Account {
        uid: op.uid,
        gid: op.gid,
    };
    out.set_owner_mode(owner, 0o700)?;
    for src in [u.layout.log_dir.join("audit.jsonl"), u.layout.config_path()] {
        let (src_parent, src_name) = fsx::split(&src)?;
        let dir = match fsx::open_leaf_dir(src_parent) {
            Ok(dir) => dir,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        match fsx::open_source(&dir, src_name, faber) {
            Ok((mut file, _)) => {
                out.write_atomic(src_name, &mut file, owner, 0o600)?;
                println!("  kept {src_name} in {}", to.display());
            }
            Err(SourceRefusal::Missing) => {}
            Err(why) => bail!("{} {why}; not copying it", src.display()),
        }
    }
    Ok(())
}

/// How long `faber engine stop` may take: the launcher asks the guest to shut
/// down and forces it after 60 s ([`crate::vm::engine::EngineTimeouts::stop`]).
const ENGINE_STOP_LIMIT: Duration = Duration::from_secs(180);

/// Stop faber's Linux VM as `_faber` and check the supervisor removed its
/// claim marker, so the files removed next are not under a running VM.
fn stop_engine(u: &UninstallState, runner: &mut dyn Runner) -> Result<()> {
    let faber = u
        .users
        .iter()
        .find(|x| x.name == FABER_ACCOUNT)
        .map(|x| fsx::Account {
            uid: x.uid,
            gid: x.gid,
        })
        .context("_faber no longer exists, and the supervisor serves no one else")?;
    let args = [std::ffi::OsStr::new("engine"), std::ffi::OsStr::new("stop")];
    let out = runner
        .run_faber_as(
            &u.layout.cli_bin,
            &args,
            faber,
            &u.layout.state_dir,
            ENGINE_STOP_LIMIT,
        )
        .with_context(|| format!("running `faber engine stop` as {FABER_ACCOUNT}"))?;
    if !out.success {
        bail!(
            "`faber engine stop` as {FABER_ACCOUNT} failed; nothing has been removed:\n{}",
            out.sanitized(2000)
        );
    }
    let (claims, unknown) = read_claims(&u.layout.vm_home.join("state"));
    let marker = format!("slot-{}.claim", crate::vm::engine::LINUX_SLOT);
    if claims.contains(&marker) || unknown.is_some() {
        bail!(
            "the supervisor reported faber's Linux VM stopped, but {marker} is still in {}; \
             not removing what it runs from",
            u.layout.vm_home.join("state").display()
        );
    }
    println!("  stopped faber's Linux VM; its disk is kept");
    Ok(())
}

/// Run the plan.
pub async fn execute(
    u: &UninstallState,
    steps: &[UStep],
    drain_timeout: Duration,
    runner: &mut dyn Runner,
) -> Result<()> {
    for step in steps {
        match step {
            UStep::SkipDrain => println!("  not draining: {NO_DRAIN_WARNING}"),
            UStep::Drain => {
                let socket = super::system_socket();
                if super::api_answers(&socket, crate::control::Fallback::Never).await {
                    println!("  draining {}", socket.display());
                    crate::drain::run(&socket, crate::control::Fallback::Never, drain_timeout)
                        .await
                        .context("draining the daemon before stopping it")?;
                }
            }
            UStep::StopService => match u.os {
                Os::MacOs => {
                    let target = service::system_target();
                    launchd::bootout(runner, &target)?;
                    launchd::wait_until_gone(
                        runner,
                        &target,
                        service::STOP_TIMEOUT + Duration::from_secs(60),
                        Duration::from_secs(1),
                    )?;
                    println!("  stopped {target}");
                }
                Os::Linux => {
                    systemd::disable_now(runner)?;
                    println!("  stopped {}", service::UNIT_NAME);
                }
            },
            UStep::StopEngine => stop_engine(u, runner)?,
            UStep::RemoveUnit(p) => {
                remove_path(p, false)?;
                if u.os == Os::Linux {
                    systemd::daemon_reload(runner)?;
                }
            }
            UStep::KeepLogs { to } => keep_logs(u, to)?,
            UStep::RemoveTree(p) => remove_path(p, true)?,
            UStep::RemoveVmHome { home, keep } => remove_vm_home(home, keep)?,
            UStep::RemoveCredentials(p) => {
                remove_path(p, true)?;
                println!("  note: {REVOKE_NOTE}");
            }
            UStep::RemoveFile(p) | UStep::RemoveRecord(p) => remove_path(p, false)?,
            UStep::RemoveRootFile(p) => remove_in_root_chain(p, false)?,
            UStep::RemoveDirIfEmpty(p) => remove_in_root_chain(p, true)?,
            UStep::RemoveEtcDir(p) => match std::fs::remove_dir(p) {
                Ok(()) => println!("  removed {}", p.display()),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => println!("  kept {}: {e}", p.display()),
            },
            UStep::StopJob(label) => {
                let target = service::system_target_of(label);
                launchd::bootout(runner, &target)?;
                launchd::wait_until_gone(
                    runner,
                    &target,
                    Duration::from_secs(60),
                    Duration::from_secs(1),
                )?;
                println!("  stopped {target}");
            }
            UStep::FlushAnchor => {
                let cmd = super::tools::Cmd::new(
                    super::tools::Tool::Pfctl,
                    ["-a", crate::wall::ANCHOR, "-F", "all"],
                );
                match runner.run(&cmd) {
                    Ok(out) if out.success => println!("  flushed {}", crate::wall::ANCHOR),
                    Ok(out) => println!(
                        "  warning: `{}` failed: {}",
                        cmd.display(),
                        out.sanitized(400)
                    ),
                    Err(e) => println!("  warning: `{}`: {e:#}", cmd.display()),
                }
            }
            UStep::DeleteUser(name) => {
                if accounts::probe_user(u.os, runner, name)?.is_some() {
                    delete_account(runner, &accounts::delete_user(u.os, name))?;
                }
            }
            UStep::DeleteGroup(name) => {
                if accounts::probe_group(u.os, runner, name)?.is_some() {
                    delete_account(runner, &accounts::delete_group(u.os, name))?;
                }
            }
            UStep::Session(undo) => {
                #[cfg(target_os = "macos")]
                super::session::apply::undo(undo, &u.layout, runner)?;
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = undo;
                    println!("  skipped (macOS only): {}", step.render(u.os));
                }
            }
            UStep::Leave(why) => println!("  left alone: {why}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::record::{Account, Accounts, Group, Installed, Operator, SCHEMA};
    use crate::setup::tools::{FakeRunner, Output};

    fn layout() -> SystemLayout {
        SystemLayout::for_os(Os::MacOs)
    }

    fn record() -> SetupRecord {
        SetupRecord {
            schema: SCHEMA,
            operator: Operator {
                uid: 501,
                name: "owner".into(),
            },
            accounts: Accounts {
                faber: Account { uid: 451, gid: 451 },
                faber_vm: Account { uid: 452, gid: 452 },
                faber_mac: None,
                operator_group: Some(Group { gid: 453 }),
            },
            installed: Installed {
                version: "0.4.0".into(),
            },
            migrated_from: None,
            artifacts: super::super::plan::artifacts(Os::MacOs, &layout()),
        }
    }

    fn user(name: &str, uid: u32) -> UserInfo {
        UserInfo {
            name: name.into(),
            uid,
            gid: uid,
            home: "/var/empty".into(),
            shell: "/usr/bin/false".into(),
            hidden: true,
            password: Some("*".into()),
            auth_authority: false,
        }
    }

    fn group(name: &str, gid: u32) -> GroupInfo {
        GroupInfo {
            name: name.into(),
            gid,
            members: vec![],
        }
    }

    /// A fully set-up macOS host.
    fn set_up() -> UninstallState {
        let l = layout();
        let mut existing: Vec<PathBuf> = super::super::plan::artifacts(Os::MacOs, &l)
            .into_iter()
            .filter_map(|a| match a {
                Artifact::File { path } | Artifact::Dir { path } => Some(path),
                _ => None,
            })
            .collect();
        existing.extend([
            l.setup_record.clone(),
            l.log_dir.join("audit.jsonl"),
            l.config_path(),
        ]);
        UninstallState {
            os: Os::MacOs,
            privileged: true,
            operator: Ok(OperatorInfo {
                name: "owner".into(),
                uid: 501,
                gid: 20,
                home: "/Users/owner".into(),
            }),
            exe_path: "/usr/local/bin/faber".into(),
            exe_chain: Ok(vec![PathFacts {
                path: "/usr/local/bin/faber".into(),
                uid: 0,
                gid: 0,
                mode: 0o100_755,
            }]),
            untrusted: Untrusted {
                uids: vec![451, 452],
                gids: vec![451, 452, 453],
            },
            record: RecordProbe::Trusted(record()),
            existing,
            service_loaded: true,
            users: vec![user(FABER_ACCOUNT, 451), user(FABER_VM_ACCOUNT, 452)],
            groups: vec![
                group(FABER_ACCOUNT, 451),
                group(FABER_VM_ACCOUNT, 452),
                group(FABER_OPERATOR_GROUP, 453),
            ],
            keep_logs: true,
            no_drain: false,
            restores: vec![],
            vm: VmUninstall::default(),
            session: Default::default(),
            menubar_app: false,
            layout: l,
        }
    }

    /// A macOS host with the VM runtime installed and idle.
    fn set_up_with_vms() -> UninstallState {
        let mut u = set_up();
        u.vm = VmUninstall {
            claims: vec![],
            claims_unknown: None,
            supervisor_plist: true,
            supervisor_loaded: true,
            anchor_plist: true,
            anchor_loaded: true,
            existing: vm_paths(),
            image_store: true,
            linux_disk: true,
            docker_cli: true,
            docker_cli_dir: true,
        };
        u
    }

    /// Every path a step names.
    fn step_paths(steps: &[UStep]) -> Vec<PathBuf> {
        steps
            .iter()
            .filter_map(|s| match s {
                UStep::RemoveUnit(p)
                | UStep::RemoveTree(p)
                | UStep::RemoveFile(p)
                | UStep::RemoveRootFile(p)
                | UStep::RemoveCredentials(p)
                | UStep::RemoveRecord(p)
                | UStep::RemoveEtcDir(p)
                | UStep::RemoveDirIfEmpty(p)
                | UStep::KeepLogs { to: p } => Some(p.clone()),
                _ => None,
            })
            .collect()
    }

    /// The Docker CLI's removal walks from `/` through root's directories
    /// only: under a directory another account owns (any temp directory a test
    /// can write is one), nothing is removed and it says why. A path whose
    /// parent is missing is already gone.
    #[test]
    fn the_docker_cli_is_removed_only_through_a_root_only_chain() {
        let tmp = std::env::temp_dir().join(format!("faber-uninstall-cli-{}", std::process::id()));
        std::fs::create_dir_all(tmp.join("faber")).unwrap();
        let file = tmp.join("faber").join("docker");
        std::fs::write(&file, b"x").unwrap();
        let err = remove_in_root_chain(&file, false).unwrap_err().to_string();
        assert!(err.contains("not removing"), "{err}");
        assert!(
            file.exists(),
            "removed through a chain root does not own alone"
        );
        let err = remove_in_root_chain(&tmp.join("faber"), true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not removing"), "{err}");
        assert!(tmp.join("faber").exists());
        std::fs::remove_dir_all(&tmp).unwrap();
        // A missing parent under root's `/`: nothing there, done.
        remove_in_root_chain(Path::new("/nonexistent-faber-dir/docker"), false).unwrap();
        remove_in_root_chain(Path::new("/nonexistent-faber-dir"), true).unwrap();
    }

    /// The pinned Docker CLI goes, then its directory if nothing else is in it,
    /// both before the record; neither is planned when it is not there.
    #[test]
    fn the_docker_cli_is_removed_and_its_directory_if_empty() {
        let u = set_up_with_vms();
        let steps = uninstall_plan(&u, "T").unwrap();
        let at = |want: &UStep| steps.iter().position(|s| s == want);
        let file = at(&UStep::RemoveRootFile(DOCKER_CLI.into())).expect("the CLI is removed");
        let dir = at(&UStep::RemoveDirIfEmpty("/usr/local/libexec/faber".into()))
            .expect("its directory is removed if empty");
        let record = steps
            .iter()
            .position(|s| matches!(s, UStep::RemoveRecord(_)))
            .unwrap();
        assert!(file < dir && dir < record, "{steps:#?}");
        assert!(render(&u, &steps)
            .contains("remove      /usr/local/libexec/faber if nothing else is in it"));
        // Never `/usr/local/libexec` itself, nor anything as a tree.
        assert!(!step_paths(&steps)
            .iter()
            .any(|p| p == Path::new("/usr/local/libexec")));
        assert!(!steps
            .iter()
            .any(|s| matches!(s, UStep::RemoveTree(p) if p.starts_with("/usr/local"))));

        let mut u = u;
        u.vm.docker_cli = false;
        u.vm.docker_cli_dir = false;
        let steps = uninstall_plan(&u, "T").unwrap();
        assert!(!step_paths(&steps)
            .iter()
            .any(|p| p.starts_with("/usr/local/libexec")));
    }

    /// The VM daemons stop after faber's own, their plists go, the anchor is
    /// flushed, and the wall's files, the verdict directory, the tools-disk
    /// cache and the logs are removed, all before the record.
    #[test]
    fn the_vm_runtime_is_removed_in_order() {
        let u = set_up_with_vms();
        let steps = uninstall_plan(&u, "T").unwrap();
        let at = |want: &UStep| {
            steps
                .iter()
                .position(|s| s == want)
                .unwrap_or_else(|| panic!("{want:?} not in {steps:#?}"))
        };
        let stop = at(&UStep::StopService);
        let sup = at(&UStep::StopJob(service::SUPERVISOR_LABEL.into()));
        let wall = at(&UStep::StopJob(service::WALL_LABEL.into()));
        let flush = at(&UStep::FlushAnchor);
        assert!(stop < sup && sup < wall && wall < flush, "{steps:#?}");
        at(&UStep::RemoveUnit(service::SUPERVISOR_PLIST.into()));
        at(&UStep::RemoveUnit(service::WALL_PLIST.into()));
        let record = steps
            .iter()
            .position(|s| matches!(s, UStep::RemoveRecord(_)))
            .unwrap();
        for p in vm_paths() {
            let i = steps
                .iter()
                .position(|s| matches!(s, UStep::RemoveTree(x) | UStep::RemoveFile(x) if *x == p))
                .unwrap_or_else(|| panic!("{} not removed", p.display()));
            assert!(i < record);
        }
        // The launcher goes with the tree that holds it.
        let trees: Vec<PathBuf> = steps
            .iter()
            .filter_map(|s| match s {
                UStep::RemoveTree(p) => Some(p.clone()),
                _ => None,
            })
            .collect();
        assert!(
            trees.iter().any(|t| u.layout.launcher_bin.starts_with(t)),
            "the launcher is in no removed tree"
        );
        // `_faber_vm`'s home is emptied but for `linux/`, and no tree holds
        // `linux/` (uninstall_never_touches_the_linux_disk).
        let home = at(&UStep::RemoveVmHome {
            home: u.layout.vm_home.clone(),
            keep: "linux".into(),
        });
        assert!(home < record);
        assert!(!trees.iter().any(|t| u.layout.vm_home.starts_with(t)));
        assert!(
            render(&u, &steps).contains("flush       the pf anchor com.apple/000.faber-vm-wall")
        );
    }

    /// The local-only images are never a removal, and the plan says where
    /// they stay.
    #[test]
    fn uninstall_never_touches_the_image_store() {
        let u = set_up_with_vms();
        let steps = uninstall_plan(&u, "T").unwrap();
        let store = Path::new(IMAGE_STORE);
        let canon = super::super::vm_images::canonical(store);
        for p in step_paths(&steps) {
            assert!(
                !p.starts_with(store) && !p.starts_with(&canon),
                "a step targets {}",
                p.display()
            );
            assert!(!store.starts_with(&p), "{} holds the store", p.display());
        }
        assert!(steps.iter().any(
            |s| matches!(s, UStep::Leave(why) if why.starts_with(IMAGE_STORE) && why.contains("kept"))
        ));
        // Even a record naming the store is left alone.
        let mut u = set_up_with_vms();
        if let RecordProbe::Trusted(r) = &mut u.record {
            r.artifacts.push(Artifact::Dir {
                path: PathBuf::from(IMAGE_STORE),
            });
            r.artifacts.push(Artifact::Dir {
                path: PathBuf::from("/private/var/db/faber/images/golden"),
            });
        }
        u.existing.push(PathBuf::from(IMAGE_STORE));
        u.existing
            .push(PathBuf::from("/private/var/db/faber/images/golden"));
        let steps = uninstall_plan(&u, "T").unwrap();
        for p in step_paths(&steps) {
            assert!(
                !p.starts_with(store) && !p.starts_with(&canon),
                "{}",
                p.display()
            );
        }
    }

    /// `_faber_vm`'s `linux/` holds the runner image's only copy: no step
    /// removes it or anything in it, or a directory above it, even when a
    /// record names it, and the plan says where it stays.
    #[test]
    fn uninstall_never_touches_the_linux_disk() {
        let mut u = set_up_with_vms();
        let kept = kept_linux_dir(&u.layout);
        if let RecordProbe::Trusted(r) = &mut u.record {
            for p in [
                kept.clone(),
                kept.join("linux"),
                kept.join("linux/disk.raw"),
            ] {
                r.artifacts.push(Artifact::Dir { path: p.clone() });
                u.existing.push(p);
            }
        }
        let steps = uninstall_plan(&u, "T").unwrap();
        for p in step_paths(&steps) {
            assert!(!p.starts_with(&kept), "a step targets {}", p.display());
            assert!(
                !kept.starts_with(&p),
                "{} holds {}",
                p.display(),
                kept.display()
            );
        }
        let homes: Vec<&UStep> = steps
            .iter()
            .filter(|s| matches!(s, UStep::RemoveVmHome { .. }))
            .collect();
        assert_eq!(
            homes,
            [&UStep::RemoveVmHome {
                home: u.layout.vm_home.clone(),
                keep: "linux".into()
            }]
        );
        assert!(!is_ours(&kept, &u.layout));
        assert!(!is_ours(&kept.join("linux/disk.raw"), &u.layout));
        let line = linux_disk_kept(&u.layout);
        assert!(line.starts_with("/var/db/faber-vm/linux: kept"), "{line}");
        assert!(line.contains("docker save"), "{line}");
        assert_eq!(
            steps
                .iter()
                .filter(|s| **s == UStep::Leave(line.clone()))
                .count(),
            1
        );
        let text = render(&u, &steps);
        assert!(
            text.contains("but linux/, then the directory if it is empty"),
            "{text}"
        );
    }

    /// Removing the home keeps `linux/` with everything in it, removes the
    /// rest without following a symlink, and keeps the home that still holds
    /// it; a home with nothing to keep goes too.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_vm_home_is_emptied_around_the_linux_disk() {
        use crate::setup::fsx::tests::Scratch;
        let s = Scratch::new("uninstall-vm-home");
        let home = s.path("home");
        for d in [
            "home/linux/linux",
            "home/run",
            "home/state",
            "home/clones/vm-0",
            "outside",
        ] {
            std::fs::create_dir_all(s.path(d)).unwrap();
        }
        std::fs::write(s.path("home/linux/linux/disk.raw"), b"image").unwrap();
        std::fs::write(s.path("home/state/slot-0.lock"), b"").unwrap();
        std::fs::write(s.path("outside/precious"), b"").unwrap();
        std::os::unix::fs::symlink(s.path("outside"), s.path("home/link")).unwrap();
        remove_vm_home(&home, "linux").unwrap();
        let mut left: Vec<String> = std::fs::read_dir(&home)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        left.sort();
        assert_eq!(left, ["linux"]);
        assert_eq!(
            std::fs::read(s.path("home/linux/linux/disk.raw")).unwrap(),
            b"image"
        );
        assert!(s.path("outside/precious").exists());
        // Nothing to keep: the home goes as well.
        std::fs::remove_dir_all(s.path("home/linux")).unwrap();
        std::fs::create_dir(s.path("home/run")).unwrap();
        remove_vm_home(&home, "linux").unwrap();
        assert!(!home.exists());
        // Missing is done.
        remove_vm_home(&home, "linux").unwrap();
    }

    /// The engine's claim no longer refuses the uninstall: the engine is
    /// stopped as `_faber` after faber's service, before the LaunchDaemons.
    #[test]
    fn the_engine_claim_is_stopped_not_refused() {
        let mut u = set_up_with_vms();
        u.vm.claims = vec!["slot-14.claim".into()];
        let steps = uninstall_plan(&u, "T").unwrap();
        let at = |want: &UStep| steps.iter().position(|s| s == want).unwrap();
        let stop = at(&UStep::StopService);
        let engine = at(&UStep::StopEngine);
        let sup = at(&UStep::StopJob(service::SUPERVISOR_LABEL.into()));
        assert!(stop < engine && engine < sup, "{steps:#?}");
        // No claim, no step.
        u.vm.claims = vec![];
        assert!(!uninstall_plan(&u, "T")
            .unwrap()
            .contains(&UStep::StopEngine));
        // Another VM still refuses, engine or not.
        u.vm.claims = vec!["slot-0.claim".into(), "slot-14.claim".into()];
        assert!(uninstall_plan(&u, "T").is_err());
        // The engine's marker with no supervisor to ask is stale.
        u.vm.claims = vec!["slot-14.claim".into()];
        u.vm.supervisor_loaded = false;
        assert!(uninstall_plan(&u, "T").unwrap_err().contains("stale"));
    }

    /// The step runs the root-owned CLI copy as `_faber`, with `engine stop`
    /// and nothing else; a failure stops the uninstall before anything goes.
    #[tokio::test]
    async fn the_engine_stop_runs_as_faber() {
        use crate::setup::fsx::tests::Scratch;
        // The check after the stop reads `vm_home/state`; a scratch one keeps
        // the test off a host whose own Linux VM is running.
        let s = Scratch::new("uninstall-engine-stop");
        let mut u = set_up_with_vms();
        u.layout.vm_home = s.0.clone();
        let mut fake = FakeRunner::default();
        execute(&u, &[UStep::StopEngine], Duration::ZERO, &mut fake)
            .await
            .unwrap();
        assert_eq!(
            fake.ran_as,
            vec![(
                u.layout.cli_bin.clone(),
                vec!["engine".to_string(), "stop".to_string()],
                451
            )]
        );
        assert!(fake.ran.is_empty(), "{:?}", fake.ran);

        let mut fake = FakeRunner {
            ran_as_answer: Some(Output {
                success: false,
                code: Some(1),
                stderr: "the VM supervisor did not answer".into(),
                ..Output::default()
            }),
            ..FakeRunner::default()
        };
        let err = execute(&u, &[UStep::StopEngine], Duration::ZERO, &mut fake)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("nothing has been removed"), "{err}");
        assert!(err.contains("did not answer"), "{err}");
    }

    /// A stop the supervisor reports done while the engine's claim marker is
    /// still there refuses the uninstall: the VM may still run from its files.
    #[tokio::test]
    async fn a_claim_left_after_the_engine_stop_refuses() {
        use crate::setup::fsx::tests::Scratch;
        let s = Scratch::new("uninstall-engine-claim");
        let marker = format!("slot-{}.claim", crate::vm::engine::LINUX_SLOT);
        std::fs::create_dir(s.path("state")).unwrap();
        std::fs::write(s.path("state").join(&marker), b"").unwrap();
        let mut u = set_up_with_vms();
        u.layout.vm_home = s.0.clone();
        let err = execute(
            &u,
            &[UStep::StopEngine],
            Duration::ZERO,
            &mut FakeRunner::default(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains(&marker), "{err}");
        assert!(err.contains("not removing"), "{err}");
    }

    /// The Linux VM's disk is never a step: nothing in the plan for an
    /// engine uninstall removes `linux/`.
    #[test]
    fn stopping_the_engine_never_plans_its_disk() {
        let mut u = set_up_with_vms();
        u.vm.claims = vec!["slot-14.claim".into()];
        let steps = uninstall_plan(&u, "T").unwrap();
        let kept = kept_linux_dir(&u.layout);
        for p in step_paths(&steps) {
            assert!(!p.starts_with(&kept) && !kept.starts_with(&p), "{p:?}");
        }
    }

    /// Claimed VMs refuse the whole uninstall, before anything is touched;
    /// an unreadable state directory refuses it as root.
    #[test]
    fn claimed_vms_refuse_the_uninstall() {
        let mut u = set_up_with_vms();
        u.vm.claims = vec!["slot-0.claim".into(), "slot-14.claim".into()];
        let e = uninstall_plan(&u, "T").unwrap_err();
        assert!(e.contains("slot-0.claim, slot-14.claim"), "{e}");
        assert!(e.contains("AbandonProcessGroup"), "{e}");
        let mut u = set_up_with_vms();
        u.vm.claims_unknown = Some("permission denied".into());
        assert!(uninstall_plan(&u, "T").is_err());
        u.privileged = false;
        assert!(uninstall_plan(&u, "T").is_ok());
        // Linux has no VMs: nothing about them is planned.
        let mut u = set_up_with_vms();
        u.os = Os::Linux;
        u.vm.claims = vec!["slot-0.claim".into()];
        let steps = uninstall_plan(&u, "T").unwrap();
        assert!(!steps.contains(&UStep::FlushAnchor));
    }

    /// A symlink in place of `state` cannot hide claim markers: it is refused,
    /// not read through. A real directory is listed; a missing one is empty.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_symlinked_state_directory_is_refused() {
        use crate::setup::fsx::tests::Scratch;
        let s = Scratch::new("uninstall-state");
        std::fs::create_dir(s.path("real")).unwrap();
        std::fs::write(s.path("real/slot-3.claim"), b"").unwrap();
        std::fs::write(s.path("real/other"), b"").unwrap();
        std::fs::create_dir(s.path("empty")).unwrap();
        std::os::unix::fs::symlink(s.path("empty"), s.path("state")).unwrap();
        let (claims, unknown) = read_claims(&s.path("state"));
        assert!(claims.is_empty());
        assert!(unknown.is_some_and(|w| w.contains("not a symlink")));
        assert_eq!(
            read_claims(&s.path("real")),
            (vec!["slot-3.claim".to_string()], None)
        );
        assert_eq!(read_claims(&s.path("nope")), (Vec::new(), None));
        std::fs::write(s.path("file"), b"").unwrap();
        assert!(read_claims(&s.path("file")).1.is_some());
    }

    #[test]
    fn only_slot_claim_markers_count() {
        let names: Vec<String> = [
            "slot-3.claim",
            "slot-0.claim",
            "slot-0.lock",
            "slot-0.sock",
            "slot-.claim",
            "slot-x.claim",
            "other",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(claim_markers(&names), ["slot-0.claim", "slot-3.claim"]);
    }

    fn position(steps: &[UStep], pred: impl Fn(&UStep) -> bool) -> Vec<usize> {
        steps
            .iter()
            .enumerate()
            .filter(|(_, s)| pred(s))
            .map(|(i, _)| i)
            .collect()
    }

    /// Drain, stop, unit; keep-logs; files; the record last among files;
    /// /etc/faber; users; groups. The order ADR-0025 fixes, and the reason a
    /// TCC refusal on the accounts leaves nothing else behind.
    #[test]
    fn uninstall_runs_in_the_documented_order() {
        let u = set_up();
        let steps = uninstall_plan(&u, "20260928T120000Z").unwrap();
        assert_eq!(steps[0], UStep::Drain);
        assert_eq!(steps[1], UStep::StopService);
        assert_eq!(steps[2], UStep::RemoveUnit(u.layout.service_unit.clone()));
        assert_eq!(
            steps[3],
            UStep::KeepLogs {
                to: "/Users/owner/faber-uninstall-20260928T120000Z".into()
            }
        );
        let files = position(&steps, |s| {
            matches!(
                s,
                UStep::RemoveFile(_)
                    | UStep::RemoveTree(_)
                    | UStep::RemoveUnit(_)
                    | UStep::RemoveVmHome { .. }
            )
        });
        let record = position(&steps, |s| matches!(s, UStep::RemoveRecord(_)));
        let etc = position(&steps, |s| matches!(s, UStep::RemoveEtcDir(_)));
        let users = position(&steps, |s| matches!(s, UStep::DeleteUser(_)));
        let groups = position(&steps, |s| matches!(s, UStep::DeleteGroup(_)));
        assert_eq!(record.len(), 1);
        assert!(files.iter().all(|f| *f < record[0]), "{steps:#?}");
        assert!(etc[0] > record[0]);
        assert!(users.iter().all(|x| *x > etc[0]));
        assert!(
            groups.iter().all(|g| users.iter().all(|x| g > x)),
            "{steps:#?}"
        );
        assert_eq!(
            steps
                .iter()
                .filter(|s| matches!(s, UStep::RemoveTree(_)))
                .count(),
            3,
            "state, logs and cache"
        );
        // The VM home is emptied around the Linux VM's disk, never removed whole.
        assert!(steps.contains(&UStep::RemoveVmHome {
            home: u.layout.vm_home.clone(),
            keep: "linux".into()
        }));
        assert!(steps.contains(&UStep::RemoveFile(u.layout.cli_bin.clone())));
        assert_eq!(
            &steps[users[0]..],
            &[
                UStep::DeleteUser(FABER_VM_ACCOUNT.into()),
                UStep::DeleteUser(FABER_ACCOUNT.into()),
                UStep::DeleteGroup(FABER_OPERATOR_GROUP.into()),
                UStep::DeleteGroup(FABER_VM_ACCOUNT.into()),
                UStep::DeleteGroup(FABER_ACCOUNT.into()),
            ]
        );
    }

    /// ADR-0027: the stored token goes with faber, before the record and
    /// `/etc/faber`, and the plan says it stays valid on GitHub until revoked.
    #[test]
    fn the_stored_token_is_removed_with_a_reminder_to_revoke_it() {
        let mut u = set_up();
        let steps = uninstall_plan(&u, "t").unwrap();
        assert!(!steps
            .iter()
            .any(|s| matches!(s, UStep::RemoveCredentials(_))));

        u.existing.push(u.layout.credentials_dir.clone());
        let steps = uninstall_plan(&u, "t").unwrap();
        let cred = position(&steps, |s| {
            *s == UStep::RemoveCredentials("/etc/faber/credentials".into())
        });
        let record = position(&steps, |s| matches!(s, UStep::RemoveRecord(_)));
        let etc = position(&steps, |s| matches!(s, UStep::RemoveEtcDir(_)));
        assert_eq!(cred.len(), 1, "{steps:#?}");
        assert!(cred[0] < record[0] && record[0] < etc[0], "{steps:#?}");
        let shown = render(&u, &steps);
        assert!(
            shown.contains("/etc/faber/credentials and the GitHub token in it"),
            "{shown}"
        );
        assert!(shown.contains("until you revoke it"), "{shown}");
    }

    /// Fix for a review finding: uninstall no longer depends on the daemon
    /// agreeing to drain. `--no-drain` skips it, in its place in the order, and
    /// says loudly what that costs; the confirmation `--yes` cannot give is
    /// asked for in `uninstall()`.
    #[test]
    fn no_drain_skips_the_drain_and_says_what_it_costs() {
        let mut u = set_up();
        u.no_drain = true;
        let steps = uninstall_plan(&u, "t").unwrap();
        assert_eq!(steps[0], UStep::SkipDrain);
        assert_eq!(steps[1], UStep::StopService);
        assert!(!steps.contains(&UStep::Drain));
        let shown = render(&u, &steps);
        assert!(shown.contains("orphaned"), "{shown}");
        assert!(shown.contains("DO NOT DRAIN"), "{shown}");
    }

    /// A re-run after a TCC refusal: the record and files are gone, the
    /// accounts remain, and only they are deleted.
    #[test]
    fn a_rerun_after_tcc_deletes_only_the_accounts() {
        let mut u = set_up();
        u.record = RecordProbe::Absent;
        u.existing.clear();
        u.service_loaded = false;
        u.keep_logs = false;
        let steps = uninstall_plan(&u, "t").unwrap();
        assert_eq!(
            steps,
            vec![
                UStep::DeleteUser(FABER_VM_ACCOUNT.into()),
                UStep::DeleteUser(FABER_ACCOUNT.into()),
                UStep::DeleteGroup(FABER_OPERATOR_GROUP.into()),
                UStep::DeleteGroup(FABER_VM_ACCOUNT.into()),
                UStep::DeleteGroup(FABER_ACCOUNT.into()),
            ]
        );
    }

    #[test]
    fn a_host_with_nothing_left_plans_nothing() {
        let mut u = set_up();
        u.record = RecordProbe::Absent;
        u.existing.clear();
        u.service_loaded = false;
        u.users.clear();
        u.groups.clear();
        let steps = uninstall_plan(&u, "t").unwrap();
        assert!(steps.is_empty(), "{steps:?}");
        assert!(render(&u, &steps).contains("nothing to remove"));
    }

    /// Without a record, the CLI copy is left: an install from before
    /// ADR-0025 put a binary at the same path.
    #[test]
    fn without_a_record_the_cli_copy_stays() {
        let mut u = set_up();
        u.record = RecordProbe::Absent;
        let steps = uninstall_plan(&u, "t").unwrap();
        assert!(!steps.contains(&UStep::RemoveFile(u.layout.cli_bin.clone())));
        assert!(steps
            .iter()
            .any(|s| matches!(s, UStep::Leave(why) if why.contains("older install"))));
    }

    #[test]
    fn an_untrusted_record_or_binary_refuses() {
        let mut u = set_up();
        u.record = RecordProbe::Untrusted("is owned by uid 451, not root".into());
        assert!(uninstall_plan(&u, "t").unwrap_err().contains("uid 451"));

        let mut u = set_up();
        if let Ok(chain) = &mut u.exe_chain {
            chain[0].uid = 451;
        }
        assert!(uninstall_plan(&u, "t").unwrap_err().contains("ADR-0025"));
    }

    /// A record names what to remove, but never anything outside the layout,
    /// never an entry this release cannot read, and never an account of
    /// another shape.
    #[test]
    fn what_a_record_names_is_checked_before_it_is_removed() {
        let mut u = set_up();
        let mut r = record();
        r.artifacts.push(Artifact::Dir { path: "/".into() });
        r.artifacts.push(Artifact::File {
            path: "/etc/../etc/passwd".into(),
        });
        r.artifacts.push(Artifact::Other(
            serde_json::json!({"kind": "vm", "name": "x"}),
        ));
        u.existing
            .extend([PathBuf::from("/"), PathBuf::from("/etc/../etc/passwd")]);
        u.record = RecordProbe::Trusted(r);
        u.users[0].uid = 501;
        let steps = uninstall_plan(&u, "t").unwrap();
        assert!(!steps.contains(&UStep::RemoveTree("/".into())));
        assert!(!steps
            .iter()
            .any(|s| matches!(s, UStep::RemoveFile(p) if p.ends_with("passwd"))));
        assert!(!steps.contains(&UStep::DeleteUser(FABER_ACCOUNT.into())));
        let left: Vec<&String> = steps
            .iter()
            .filter_map(|s| match s {
                UStep::Leave(why) => Some(why),
                _ => None,
            })
            .collect();
        assert_eq!(left.len(), 4, "{left:#?}");
    }

    #[test]
    fn is_ours_accepts_only_the_layout() {
        let l = layout();
        for ok in [
            "/Library/Application Support/faber",
            "/Library/Application Support/faber/bin/faber",
            "/Library/Logs/faber",
            "/Library/Caches/faber",
            "/var/db/faber-vm",
            "/etc/faber",
            "/etc/faber/setup.json",
            "/usr/local/bin/faber",
            "/Library/LaunchDaemons/io.github.rejifald.faber.plist",
            "/Library/LaunchDaemons/io.github.rejifald.faber.vm.plist",
        ] {
            assert!(is_ours(Path::new(ok), &l), "{ok}");
        }
        for bad in [
            "/",
            "/Library",
            "/Library/LaunchDaemons/com.example.other.plist",
            "/usr/local/bin/other",
            "relative/faber",
            "/Library/Logs/faber/../../x",
        ] {
            assert!(!is_ours(Path::new(bad), &l), "{bad}");
        }
    }

    /// The TCC refusal is recognised and explained; any other failure is
    /// reported as it is.
    #[test]
    fn a_tcc_refusal_names_terminal_app() {
        let cmd = accounts::delete_user(Os::MacOs, FABER_ACCOUNT);
        let mut fake = FakeRunner {
            answers: vec![(
                cmd.display(),
                Output {
                    success: false,
                    code: Some(40),
                    stdout: "<main> delete status: eDSPermissionError\n".into(),
                    ..Output::default()
                },
            )],
            ..FakeRunner::default()
        };
        let err = delete_account(&mut fake, &cmd).unwrap_err().to_string();
        assert!(err.contains("Terminal.app"), "{err}");
        assert!(err.contains("eDSPermissionError"), "{err}");
    }

    /// `original` with `suffix` added to its file name.
    fn beside(original: &Path, suffix: &str) -> PathBuf {
        let mut name = original.file_name().unwrap().to_os_string();
        name.push(suffix);
        original.with_file_name(name)
    }

    /// Fix for a review finding: setup renamed the LEGACY directories aside
    /// as well as the definition, and the old agent runs its binary and
    /// reads its config from inside them. So the note renames every backup
    /// it found back, with the names on disk, then loads the definition.
    #[test]
    fn the_restore_note_renames_back_every_backup_it_found() {
        use crate::setup::fsx::tests::{me, Scratch};
        use std::os::unix::fs::symlink;

        let s = Scratch::new("restore");
        let uid = me(&s).uid;
        let os = Os::HOST;
        // A home with a space in it, as `Application Support` has anyway.
        let home = s.path("home dir");
        let dirs = super::super::plan::legacy_dirs(&crate::platform::legacy(
            &crate::platform::Env::with_home(&home),
        ));
        let def = super::super::old::definition_path(os, &home);
        const T: &str = ".pre-system-20260928T120000Z";
        for d in &dirs {
            std::fs::create_dir_all(d.parent().unwrap()).unwrap();
            std::fs::create_dir(beside(d, T)).unwrap();
        }
        std::fs::create_dir_all(def.parent().unwrap()).unwrap();
        std::fs::write(beside(&def, T), b"definition").unwrap();
        // An older run's backup, a name setup never writes, a symlink and a
        // file where a directory belongs: none of them is offered.
        std::fs::create_dir(beside(&dirs[0], ".pre-system-20250101T000000Z")).unwrap();
        std::fs::create_dir(beside(&dirs[0], ".pre-system-$(touch x)")).unwrap();
        symlink(&s.0, beside(&dirs[0], ".pre-system-20991231T235959Z")).unwrap();
        std::fs::write(beside(&dirs[0], ".pre-system-20991231T235958Z"), b"").unwrap();

        let r = find_restore(os, "owner", uid, &home).expect("backups found");
        let mut want: Vec<(PathBuf, PathBuf)> =
            dirs.iter().map(|d| (beside(d, T), d.clone())).collect();
        want.push((beside(&def, T), def.clone()));
        assert_eq!(r.renames, want);
        assert_eq!(r.definition.as_ref(), Some(&def));
        assert!(r.blocked.is_empty());

        let note = restore_note(os, std::slice::from_ref(&r), Some("owner"));
        for (backup, original) in &want {
            let line = format!("  mv '{}' '{}'\n", backup.display(), original.display());
            assert!(note.contains(&line), "{line}not in:\n{note}");
        }
        let load = match os {
            Os::MacOs => format!("  launchctl bootstrap gui/{uid} '{}'\n", def.display()),
            Os::Linux => "  systemctl --user enable --now faber.service\n".to_string(),
        };
        assert!(note.contains(&load), "{note}");
        assert!(note.contains("run these as owner, not with sudo"), "{note}");
        assert!(!note.contains('<'), "no placeholder:\n{note}");
        for absent in [
            ".pre-system-20250101T000000Z",
            ".pre-system-$(touch x)",
            ".pre-system-20991231T235959Z",
            ".pre-system-20991231T235958Z",
        ] {
            assert!(!note.contains(absent), "{absent:?} in:\n{note}");
        }

        // An original that exists again is never renamed over.
        std::fs::create_dir(&dirs[0]).unwrap();
        let r = find_restore(os, "owner", uid, &home).unwrap();
        assert_eq!(r.blocked, vec![(beside(&dirs[0], T), dirs[0].clone())]);
        assert!(!r.renames.iter().any(|(_, o)| o == &dirs[0]));
        let note = restore_note(os, &[r], Some("owner"));
        assert!(
            !note.contains(&format!("'{}'\n", dirs[0].display())),
            "{note}"
        );
        assert!(note.contains("# skipped"), "{note}");

        // Another account's commands are addressed to it.
        let r = find_restore(os, "ci", uid, &home).unwrap();
        let note = restore_note(os, &[r], Some("owner"));
        assert!(note.contains("ci's per-user faber"), "{note}");
        assert!(note.contains("run these as ci, not with sudo"), "{note}");

        // A home with no backups has nothing to restore.
        assert_eq!(find_restore(os, "owner", uid, &s.path("elsewhere")), None);
    }

    #[test]
    fn a_path_is_quoted_so_no_shell_expands_it() {
        assert_eq!(sh_quote(Path::new("/a b/c")), "'/a b/c'");
        assert_eq!(sh_quote(Path::new("/it's")), "'/it'\\''s'");
        assert_eq!(sh_quote(Path::new("/$(x)`y`")), "'/$(x)`y`'");
        assert!(is_stamp("20260928T120000Z"));
        assert!(!is_stamp("20260928T120000"));
        assert!(!is_stamp("2026092xT120000Z"));
        assert!(!is_stamp("$(touch x)000000"));
    }

    fn session_user(uid: u32) -> UserInfo {
        UserInfo {
            name: crate::platform::layout::FABER_MAC_ACCOUNT.into(),
            uid,
            gid: 20,
            home: crate::platform::layout::FABER_MAC_HOME.into(),
            shell: "/sbin/nologin".into(),
            hidden: true,
            password: Some("********".into()),
            auth_authority: true,
        }
    }

    /// ADR-0051's session account, in every mode's leftovers: the session
    /// logged out first, auto-login and "Other…" put back before `/etc/faber`
    /// goes, the home with the files, the account first among the accounts,
    /// and the FileVault list refreshed after it.
    #[test]
    fn the_session_account_is_removed_and_leaves_nothing() {
        use crate::platform::layout::{FABER_MAC_ACCOUNT, FABER_MAC_HOME};
        use crate::setup::session::{SessionUndo, SessionUninstall};
        let mut u = set_up();
        u.session = SessionUninstall {
            user: Some(session_user(600)),
            admin: false,
            session_up: true,
            autologin_ours: true,
            show_other_saved: true,
            home: true,
            secure_token: true,
            generated_uid: Some("0A1B2C3D-4E5F-6789-ABCD-EF0123456789".into()),
            ssh_deny: true,
        };
        let steps = uninstall_plan(&u, "20260928T120000Z").unwrap();
        let at = |want: &UStep| {
            steps
                .iter()
                .position(|s| s == want)
                .unwrap_or_else(|| panic!("{want:?} missing: {steps:#?}"))
        };
        let logout = at(&UStep::Session(SessionUndo::Logout { uid: 600 }));
        let autologin = at(&UStep::Session(SessionUndo::AutoLoginOff));
        let other = at(&UStep::Session(SessionUndo::RestoreShowOther));
        let home = at(&UStep::RemoveTree(FABER_MAC_HOME.into()));
        let user = at(&UStep::DeleteUser(FABER_MAC_ACCOUNT.into()));
        let preboot = at(&UStep::Session(SessionUndo::UpdatePreboot));
        let crypto = at(&UStep::Session(SessionUndo::CheckCryptoUser {
            uuid: "0A1B2C3D-4E5F-6789-ABCD-EF0123456789".into(),
        }));
        let ssh = at(&UStep::RemoveRootFile(
            crate::setup::session::ssh_deny_real(),
        ));
        let etc = at(&UStep::RemoveEtcDir(u.layout.etc_dir.clone()));
        let faber = at(&UStep::DeleteUser(FABER_ACCOUNT.into()));
        assert!(logout < autologin && autologin < other && other < home);
        assert!(other < etc, "the kept setting lives in /etc/faber");
        assert!(home < etc && etc < user, "files before accounts");
        assert!(ssh < user, "the SSH drop-in goes with the files");
        assert!(user < preboot && user < faber);
        assert!(
            preboot < crypto,
            "the crypto user is checked once the account and its Preboot entry are gone"
        );
        let shown = render(&u, &steps);
        assert!(
            shown.contains("verify      _faber_mac (0A1B2C3D-4E5F-6789-ABCD-EF0123456789)"),
            "{shown}"
        );
        assert!(shown.contains("diskutil apfs listCryptoUsers /"), "{shown}");

        // Without a GeneratedUID nothing can be checked, and it says so.
        u.session.generated_uid = None;
        let steps = uninstall_plan(&u, "20260928T120000Z").unwrap();
        assert!(!steps
            .iter()
            .any(|s| matches!(s, UStep::Session(SessionUndo::CheckCryptoUser { .. }))));
        assert!(steps
            .iter()
            .any(|s| matches!(s, UStep::Leave(w) if w.contains("listCryptoUsers"))));
        u.session.generated_uid = Some("0A1B2C3D-4E5F-6789-ABCD-EF0123456789".into());

        // A user by that name that setup did not make is left, and so is its
        // home.
        u.session.user = Some(session_user(599));
        let steps = uninstall_plan(&u, "20260928T120000Z").unwrap();
        assert!(!steps.contains(&UStep::DeleteUser(FABER_MAC_ACCOUNT.into())));
        assert!(!steps.contains(&UStep::RemoveTree(FABER_MAC_HOME.into())));
        assert!(steps
            .iter()
            .any(|s| matches!(s, UStep::Leave(w) if w.contains("outside 600-699"))));
        u.session.user = Some(session_user(600));
        u.session.admin = true;
        let steps = uninstall_plan(&u, "20260928T120000Z").unwrap();
        assert!(!steps.contains(&UStep::DeleteUser(FABER_MAC_ACCOUNT.into())));

        // Another account's auto-login is not faber's: left as it is.
        u.session.admin = false;
        u.session.autologin_ours = false;
        let steps = uninstall_plan(&u, "20260928T120000Z").unwrap();
        assert!(!steps.contains(&UStep::Session(SessionUndo::AutoLoginOff)));
        assert!(steps.contains(&UStep::DeleteUser(FABER_MAC_ACCOUNT.into())));

        // Without one, nothing of it is planned.
        u.session = SessionUninstall::default();
        let steps = uninstall_plan(&u, "20260928T120000Z").unwrap();
        assert!(!steps
            .iter()
            .any(|s| matches!(s, UStep::Session(_))
                || *s == UStep::DeleteUser(FABER_MAC_ACCOUNT.into())));
    }

    /// The menu-bar app is the operator's: uninstall leaves it, and says how
    /// to remove it; it touches nothing at its path.
    #[test]
    fn the_menubar_app_is_left_and_named() {
        let mut u = set_up();
        let app = super::super::app::app_path();
        let said = |steps: &[UStep]| {
            steps
                .iter()
                .any(|s| matches!(s, UStep::Leave(w) if w.contains("the menu-bar app, left")))
        };
        assert!(!said(&uninstall_plan(&u, "T").unwrap()));
        u.menubar_app = true;
        let steps = uninstall_plan(&u, "T").unwrap();
        assert!(said(&steps), "{steps:?}");
        assert!(menubar_app_left().contains("rm -rf \"/Applications/faber.app\""));
        for p in step_paths(&steps) {
            assert!(!p.starts_with(&app), "a step targets {}", p.display());
        }
    }
}
