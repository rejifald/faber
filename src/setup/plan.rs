//! What `sudo faber setup` would do to this host, as a pure function of what
//! the host is: [`plan`] maps a [`HostState`] to a list of [`Step`]s, each
//! `ok`, `will create`, `will change`, `refused` or `skipped` (a warning).
//!
//! The plan is printed before anything changes, and it is the only input the
//! executor acts on: every change is a [`Step`] carrying an [`Action`]. A host
//! that is already set up plans nothing but `ok` steps, so a re-run changes
//! nothing and restarts nothing. That idempotency is tested here, against a
//! fabricated `HostState`, rather than against a real machine.
//!
//! Probing (reading the directory service, stat-ing paths, asking launchd) is
//! [`super::probe`]'s job; executing is [`super::apply`]'s. Neither decides
//! anything this module doesn't.

use std::path::{Path, PathBuf};

use crate::platform::layout::{
    Os, Paths, SystemLayout, FABER_ACCOUNT, FABER_OPERATOR_GROUP, FABER_VM_ACCOUNT,
};
use crate::platform::record::{
    Account, Accounts, Artifact, Group, Installed, MigratedFrom, Operator, SetupRecord, SCHEMA,
};
use crate::platform::service;

use super::accounts::{self, GroupInfo, UsedIds, UserInfo};
use super::exe::{self, PathFacts, Untrusted};
use super::fsx;
use super::tools::Cmd;

/// The control panel's port. The LaunchDaemon and the unit pass no `--port`,
/// so the SYSTEM daemon always listens here.
pub const DEFAULT_PORT: u16 = 8787;

/// A step's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Already as setup would leave it.
    Ok,
    WillCreate,
    WillChange,
    /// Setup cannot proceed until a person fixes this.
    Refused,
    /// Needed, and not done by this run: a warning, and setup goes on.
    Skipped,
}

impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Status::Ok => "ok",
            Status::WillCreate => "will create",
            Status::WillChange => "will change",
            Status::Refused => "refused",
            Status::Skipped => "skipped",
        }
    }
}

/// Where a step prints in the plan. The order is the order setup's design
/// lists them in, not the order they execute in (see [`super::apply`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    Preflight,
    Accounts,
    Directories,
    Binaries,
    OldInstances,
    Migration,
    LogFiles,
    Credential,
    Record,
    /// ADR-0026's launcher, wall and images ([`super::vm_runtime`]).
    VmRuntime,
    Service,
    Legacy,
    KeepAwake,
    /// The menu-bar app's first install ([`super::app`]).
    App,
    /// ADR-0051's macOS VM session account ([`super::session`]).
    Session,
    LinuxImage,
}

impl Status {
    /// The kind it prints as (ADR-0050): `skipped` is a warning, which is
    /// what its detail already says.
    pub fn kind(self) -> crate::out::Kind {
        use crate::out::Kind;
        match self {
            Status::Ok => Kind::Ok,
            Status::WillCreate => Kind::Create,
            Status::WillChange => Kind::Change,
            Status::Refused => Kind::Refused,
            Status::Skipped => Kind::Warning,
        }
    }
}

impl Section {
    /// What this section's `steps` do to the machine, as a person would say
    /// it: the plan's "at a glance" (ADR-0050). Written by the plan, not the
    /// renderer, so it is tested with the steps. Empty when the section
    /// changes nothing worth a sentence; a warning is never left out.
    pub fn glance(self, s: &HostState, steps: &[&Step]) -> Vec<(crate::out::Kind, String)> {
        use crate::out::Kind;
        let changing = |st: &&&Step| matches!(st.status, Status::WillCreate | Status::WillChange);
        let doing = |f: fn(&Action) -> bool| {
            steps
                .iter()
                .filter(changing)
                .filter(move |st| st.action.as_ref().is_some_and(f))
        };
        let warnings: Vec<&&Step> = steps
            .iter()
            .filter(|st| st.status == Status::Skipped)
            .collect();
        let you = |name: &str| match &s.operator {
            Ok(op) if op.name == name => "your".to_string(),
            _ => format!("{name}'s"),
        };
        let mut out = Vec::new();
        match self {
            Section::Accounts => {
                let users: Vec<&str> = doing(|a| matches!(a, Action::Run(_)))
                    .filter(|st| st.status == Status::WillCreate && st.subject.starts_with("user "))
                    .filter_map(|st| st.shown.item.as_deref()?.strip_prefix("user "))
                    .collect();
                if !users.is_empty() {
                    let count = match users.len() {
                        1 => "a".to_string(),
                        2 => "two".to_string(),
                        n => n.to_string(),
                    };
                    out.push((
                        Kind::Create,
                        format!(
                            "{count} {}role account{}, {}",
                            if s.os == Os::MacOs { "hidden " } else { "" },
                            if users.len() == 1 { "" } else { "s" },
                            users.join(" and ")
                        ),
                    ));
                }
                let changes = |subject: String| {
                    steps
                        .iter()
                        .any(|st| st.status == Status::WillChange && st.subject == subject)
                };
                if let Ok(op) = &s.operator {
                    if changes(joins_subject(&op.name)) {
                        out.push((
                            Kind::Change,
                            format!(
                                "you join group {FABER_OPERATOR_GROUP}, to read config, logs and \
                                 the audit trail"
                            ),
                        ));
                    }
                    if changes(leaves_subject(&op.name)) {
                        out.push((
                            Kind::Change,
                            format!(
                                "you leave group {FABER_ACCOUNT}, so only faber reads its GitHub \
                                 token"
                            ),
                        ));
                    }
                }
                let fixed: Vec<&str> = steps
                    .iter()
                    .filter(|st| st.status == Status::WillChange)
                    .filter_map(|st| st.shown.item.as_deref())
                    .collect();
                let mut fixed = fixed;
                fixed.dedup();
                if !fixed.is_empty() {
                    out.push((
                        Kind::Change,
                        format!(
                            "{} {} brought into shape",
                            fixed.join(" and "),
                            if fixed.len() == 1 { "is" } else { "are" }
                        ),
                    ));
                }
                if steps.iter().any(|st| {
                    st.status == Status::WillChange && st.subject.ends_with("in group docker")
                }) {
                    out.push((
                        Kind::Change,
                        format!(
                            "{FABER_ACCOUNT} joins group docker, root-equivalent until ADR-0026"
                        ),
                    ));
                }
            }
            Section::Binaries => {
                if doing(|a| matches!(a, Action::InstallDaemon | Action::InstallCli))
                    .any(|st| st.status == Status::WillChange)
                {
                    out.push((
                        Kind::Change,
                        format!(
                            "faber's binaries are replaced with this one ({})",
                            s.version
                        ),
                    ));
                }
            }
            Section::OldInstances => {
                for st in steps.iter().filter(changing) {
                    if let Some(Action::StopOld { account }) = &st.action {
                        out.push((
                            Kind::Change,
                            format!("{} per-user faber is drained and stopped", you(account)),
                        ));
                    }
                }
            }
            Section::Migration => {
                let mut files = Vec::new();
                let mut from = None;
                let mut kind = Kind::Create;
                for st in steps.iter().filter(changing) {
                    if let Some(Action::Migrate(item)) = &st.action {
                        files.push(
                            item.dest
                                .file_name()
                                .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
                        );
                        from = Some(item.account.clone());
                        if st.status == Status::WillChange {
                            kind = Kind::Change;
                        }
                    }
                }
                if let Some(from) = from {
                    out.push((
                        kind,
                        format!(
                            "{} copied from {} per-user install; the originals stay",
                            files.join(" and "),
                            you(&from)
                        ),
                    ));
                }
                if doing(|a| matches!(a, Action::SeedConfig)).next().is_some() {
                    out.push((Kind::Create, "a starter config".to_string()));
                }
            }
            Section::Service => {
                for st in doing(|a| matches!(a, Action::InstallService { .. })) {
                    out.push(if st.status == Status::WillCreate {
                        (Kind::Create, "faber as a system service".to_string())
                    } else {
                        (
                            Kind::Change,
                            "the faber service is drained and restarted".to_string(),
                        )
                    });
                }
            }
            Section::VmRuntime => {
                use super::vm_runtime::VmAction as V;
                if doing(|a| matches!(a, Action::Vm(V::InstallAnchor { .. })))
                    .any(|st| st.status == Status::WillCreate)
                {
                    out.push((Kind::Create, "the VM wall as a system service".to_string()));
                }
                if doing(|a| matches!(a, Action::Vm(V::InstallSupervisor { .. })))
                    .any(|st| st.status == Status::WillCreate)
                {
                    out.push((
                        Kind::Create,
                        "the VM supervisor as a system service".to_string(),
                    ));
                }
                let named = |names: &[&str]| {
                    warnings.iter().any(|st| {
                        Path::new(&st.subject)
                            .file_name()
                            .is_some_and(|n| names.iter().any(|want| n == *want))
                    })
                };
                let launcher = named(&super::vm_runtime::LAUNCHER_NAMES);
                let agent = named(&super::vm_runtime::AGENT_NAMES);
                if launcher {
                    out.push((
                        Kind::Warning,
                        format!(
                            "VM jobs cannot run yet: no VM launcher{} beside this binary",
                            if agent { " or guest agent" } else { "" }
                        ),
                    ));
                } else if agent {
                    out.push((
                        Kind::Warning,
                        "macOS VM jobs cannot build their tools disk yet: no guest agent \
                         beside the launcher"
                            .to_string(),
                    ));
                }
            }
            Section::Legacy => {
                let n = doing(|a| matches!(a, Action::MoveLegacyAside { .. })).count();
                if n > 0 {
                    out.push((
                        Kind::Change,
                        format!(
                            "{n} old per-user faber folder{} renamed aside once the new daemon \
                             answers",
                            if n == 1 { " is" } else { "s are" }
                        ),
                    ));
                }
            }
            Section::KeepAwake => {
                if doing(|a| matches!(a, Action::KeepAwake)).next().is_some() {
                    out.push((
                        Kind::Change,
                        "system sleep and Power Nap are turned off".to_string(),
                    ));
                }
            }
            Section::App => {
                out.extend(super::app::glance(steps));
            }
            Section::Session => {
                if let Some((creates, text)) = super::session::glance(steps) {
                    out.push((if creates { Kind::Create } else { Kind::Change }, text));
                }
            }
            Section::LinuxImage => {
                if let Some(st) = steps.iter().find(changing) {
                    out.push((
                        st.status.kind(),
                        "the Linux VM's image is built, last (up to 30 minutes)".to_string(),
                    ));
                }
            }
            Section::Credential => {
                if steps
                    .iter()
                    .any(|st| matches!(st.status, Status::WillCreate | Status::WillChange))
                {
                    out.push((
                        Kind::Change,
                        "the GitHub token file is given to root".to_string(),
                    ));
                }
            }
            Section::Preflight | Section::Directories | Section::LogFiles | Section::Record => {}
        }
        // A warning no sentence above accounts for still gets a line: the
        // summary never leaves one out.
        if !warnings.is_empty() && !out.iter().any(|(k, _)| *k == Kind::Warning) {
            out.push((
                Kind::Warning,
                format!(
                    "{}: {} warning{}, listed below",
                    self.title(),
                    warnings.len(),
                    if warnings.len() == 1 { "" } else { "s" }
                ),
            ));
        }
        out
    }

    pub fn title(self) -> &'static str {
        match self {
            Section::Preflight => "preflight",
            Section::Accounts => "accounts",
            Section::Directories => "directories",
            Section::Binaries => "binaries",
            Section::OldInstances => "per-user instances",
            Section::Migration => "config and state",
            Section::LogFiles => "log files",
            Section::Credential => "GitHub token",
            Section::Record => "setup record",
            Section::VmRuntime => "VMs (ADR-0026)",
            Section::Service => "service",
            Section::Legacy => "your old per-user files",
            Section::KeepAwake => "keep awake",
            Section::App => "menu-bar app",
            Section::Session => "macOS VM session (ADR-0051)",
            Section::LinuxImage => "Linux VM image",
        }
    }
}

/// One step of the plan, and the change it stands for, if any.
#[derive(Debug, Clone, PartialEq)]
pub struct Step {
    pub section: Section,
    pub subject: String,
    pub status: Status,
    /// Everything the plan says about the step: what `--verbose` prints.
    pub detail: String,
    pub action: Option<Action>,
    /// How the step is printed ([`super::view`]); it changes nothing setup
    /// does.
    pub shown: Shown,
}

/// How a step is printed (ADR-0050): which item it belongs to and what the
/// default view says about it. Written here, by the plan, so the renderer
/// never decides what a step means.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Shown {
    /// Steps of a section that share a key print as one item, named by the
    /// key: a role account is one line, not four.
    pub item: Option<String>,
    /// The item's name, when it is not the subject.
    pub name: Option<String>,
    /// What the default view says, when it is shorter than the detail;
    /// empty says nothing. A refusal or a warning ignores it: those are
    /// never shortened.
    pub brief: Option<String>,
    /// An `ok` step that describes something setup will still do: listed
    /// like a change, not folded into its section's count.
    pub listed: bool,
}

/// A change the executor makes. Every one is idempotent: running it against a
/// host that already has what it produces changes nothing.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// An account command: create a group or user, hide a user, add a member
    /// or take one out.
    Run(Cmd),
    /// macOS: leave a new role account with no password, then verify it.
    LockPassword { name: String },
    /// Create a directory, or fix its own owner and mode. Never recursive.
    EnsureDir(DirSpec),
    /// Create a file empty, or fix its owner and mode.
    EnsureFile(FileSpec),
    /// Copy this binary to the root-owned CLI path.
    InstallCli,
    /// Copy this binary to the daemon's own path.
    InstallDaemon,
    /// Drain and stop an account's per-user faber, and move its definition
    /// aside.
    StopOld { account: String },
    /// Copy one file of an old instance's state.
    Migrate(MigrationItem),
    /// Write the starter config.
    SeedConfig,
    /// Write `/etc/faber/setup.json`.
    WriteRecord,
    /// Install the service definition and start it. With `restart`, the
    /// running SYSTEM daemon is drained and stopped first, before anything it
    /// runs from is swapped.
    InstallService { restart: bool },
    /// `pmset -a sleep 0` and `pmset -a powernap 0`.
    KeepAwake,
    /// One of the VM steps ([`super::vm_runtime`]).
    Vm(super::vm_runtime::VmAction),
    /// Build the Linux VM's image in a throwaway VM and land it root-owned
    /// ([`super::linux_image`]). Runs last, once the daemon answers.
    BuildLinuxImage,
    /// Install the menu-bar app from the release's archive, as the operator's
    /// ([`super::app`]). Last in [`super::apply::apply`], once the daemon
    /// answers; a failure is reported and rolls nothing back.
    InstallApp,
    /// One of the session account's steps ([`super::session`]), run after the
    /// service answers and before the Linux image.
    Session(super::session::SessionAction),
    /// Give the stored GitHub token file, still in the store's earlier
    /// `_faber`-owned form, to `root:_faber` at mode 0640 (ADR-0027), through
    /// [`crate::credential::Store::reown_legacy`].
    ReownCredential,
    /// Once the new daemon answers: rename an account's LEGACY directory to
    /// `<name>.pre-system-<time>`, so nothing faber reads stays in that home
    /// and the originals are kept as the backup.
    MoveLegacyAside {
        account: String,
        uid: u32,
        path: PathBuf,
    },
}

/// Who owns a SYSTEM directory or file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    Root,
    FaberVm,
    /// `_faber`, with the operator group: the state and cache trees, the log
    /// tree on Linux, and the files in them the operator reads. The operator
    /// reads them through that group and can write none of them; `_faber`,
    /// which owns them, is in it too.
    FaberOperatorGroup,
    /// Root, with the operator group: the macOS log directory. `_faber`
    /// appends to the files setup put there, reaching them through the group,
    /// and can create, rename or replace none of them.
    RootOperatorGroup,
    /// `_faber_vm`, with `_faber`'s group: on macOS, `_faber_vm`'s home and its
    /// `run/`, both 0710, so `_faber` can reach the VM supervisor's socket
    /// (and the Linux VM's `docker.sock`) and list neither directory.
    FaberVmFaberGroup,
}

/// The ids a SYSTEM owner resolves to: both role accounts and the operator
/// group, as this run plans them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ids {
    pub faber: Account,
    pub vm: Account,
    /// [`FABER_OPERATOR_GROUP`]'s gid.
    pub operator_gid: u32,
}

impl Ids {
    /// `_faber`, with the operator group: who owns what `_faber` keeps for
    /// the operator to read ([`Owner::FaberOperatorGroup`]).
    pub fn faber_operator(self) -> Account {
        Account {
            uid: self.faber.uid,
            gid: self.operator_gid,
        }
    }
}

/// One directory of the SYSTEM layout, with the owner and mode setup gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirSpec {
    pub path: PathBuf,
    pub owner: Owner,
    pub mode: u32,
    /// Inside the state directory, which `_faber` owns: created through a
    /// descriptor for it rather than by path.
    pub in_state_dir: bool,
    /// The running daemon reaches it only through its group: the macOS log
    /// directory, which is root's. So setup changes it only once the daemon
    /// is stopped, never under one that started before it joined that group.
    pub after_stop: bool,
}

/// The SYSTEM directories, parents before children.
///
/// On macOS the log directory is root's: launchd may open the plist's
/// `StandardOutPath` and `StandardErrorPath` as root, and in a directory
/// `_faber` could write, `_faber` could first replace either with a symlink to
/// any file on the host. Linux logs to the journal instead, and systemd gives
/// `LogsDirectory` to the service's user and group.
///
/// The state, cache and log trees have the operator group, which systemd
/// also gives them on Linux (the unit's `Group=`), so the two agree.
pub fn dir_specs(os: Os, layout: &SystemLayout) -> Vec<DirSpec> {
    let top = |path: &Path, owner, mode| DirSpec {
        path: path.to_path_buf(),
        owner,
        mode,
        in_state_dir: false,
        after_stop: false,
    };
    let logs = match os {
        Os::MacOs => DirSpec {
            after_stop: true,
            ..top(&layout.log_dir, Owner::RootOperatorGroup, 0o750)
        },
        Os::Linux => top(&layout.log_dir, Owner::FaberOperatorGroup, 0o750),
    };
    // macOS: `_faber` must reach `run/supervisor.sock` inside `_faber_vm`'s
    // home, and is never in `_faber_vm`'s group, so the home gives `_faber`'s
    // group search and nothing else. Linux runs no VMs and keeps it closed.
    let vm_home = match os {
        Os::MacOs => top(&layout.vm_home, Owner::FaberVmFaberGroup, 0o710),
        Os::Linux => top(&layout.vm_home, Owner::FaberVm, 0o700),
    };
    let inner = |path: &Path, mode| DirSpec {
        path: path.to_path_buf(),
        owner: Owner::FaberOperatorGroup,
        mode,
        in_state_dir: true,
        after_stop: false,
    };
    vec![
        top(&layout.etc_dir, Owner::Root, 0o755),
        top(&layout.state_dir, Owner::FaberOperatorGroup, 0o750),
        inner(&layout.bin_dir, 0o755),
        inner(&layout.registry_auth_dir, 0o700),
        logs,
        top(&layout.cache_dir, Owner::FaberOperatorGroup, 0o750),
        vm_home,
    ]
}

/// A file setup gives its owner and mode: one it creates empty, so that it
/// exists, as the right owner's, before anything else could put something in
/// its place, or one it only fixes when it is there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSpec {
    pub path: PathBuf,
    pub owner: Owner,
    pub mode: u32,
    /// Created empty when it is missing. Otherwise a missing file is left
    /// missing: the daemon, or a migration, makes it.
    pub create: bool,
    /// In a directory `_faber` can write: only `_faber`'s own file there is
    /// taken over, since `_faber` could have linked one of root's in.
    pub in_faber_dir: bool,
}

/// The files setup gives their owner and mode.
///
/// - macOS: the log files, in the root-owned log directory, `_faber`'s to
///   write, created empty. `audit.jsonl` too: the daemon opens it with create
///   and append, which works on a file that already exists and needs no right
///   to the directory.
/// - Both: each state file a migration copies ([`FileKind`]), when it is there,
///   so that the operator reads it through the operator group. A host set up
///   before that group has them in `_faber`'s.
pub fn file_specs(os: Os, layout: &SystemLayout) -> Vec<FileSpec> {
    let mut specs: Vec<FileSpec> = match os {
        Os::MacOs => ["stdout.log", "stderr.log", "audit.jsonl"]
            .into_iter()
            .map(|name| FileSpec {
                path: layout.log_dir.join(name),
                owner: Owner::FaberOperatorGroup,
                mode: 0o640,
                create: true,
                in_faber_dir: false,
            })
            .collect(),
        Os::Linux => Vec::new(),
    };
    for kind in FileKind::ALL {
        let path = kind.dest(layout);
        if !specs.iter().any(|f| f.path == path) {
            specs.push(FileSpec {
                path,
                owner: Owner::FaberOperatorGroup,
                mode: 0o640,
                create: false,
                in_faber_dir: true,
            });
        }
    }
    specs
}

/// The uid and gid a SYSTEM directory's owner resolves to.
pub fn owner_account(owner: Owner, ids: Ids) -> Account {
    match owner {
        Owner::Root => Account { uid: 0, gid: 0 },
        Owner::FaberVm => ids.vm,
        Owner::FaberOperatorGroup => ids.faber_operator(),
        Owner::RootOperatorGroup => Account {
            uid: 0,
            gid: ids.operator_gid,
        },
        Owner::FaberVmFaberGroup => Account {
            uid: ids.vm.uid,
            gid: ids.faber.gid,
        },
    }
}

/// Who may already own a directory or file setup finds in place: root, or the
/// account it is meant for. Anything else was planted, and is refused rather
/// than adopted. The macOS log directory also takes `_faber`, which owned it
/// before it became root's.
pub fn prior_owners(owner: Owner, ids: Ids) -> Vec<u32> {
    match owner {
        Owner::Root => vec![0],
        Owner::FaberOperatorGroup | Owner::RootOperatorGroup => vec![0, ids.faber.uid],
        Owner::FaberVm | Owner::FaberVmFaberGroup => vec![0, ids.vm.uid],
    }
}

fn owner_name(os: Os, owner: Owner) -> String {
    match owner {
        Owner::Root => match os {
            Os::MacOs => "root:wheel".into(),
            Os::Linux => "root:root".into(),
        },
        Owner::FaberVm => format!("{FABER_VM_ACCOUNT}:{FABER_VM_ACCOUNT}"),
        Owner::FaberOperatorGroup => format!("{FABER_ACCOUNT}:{FABER_OPERATOR_GROUP}"),
        Owner::RootOperatorGroup => format!("root:{FABER_OPERATOR_GROUP}"),
        Owner::FaberVmFaberGroup => format!("{FABER_VM_ACCOUNT}:{FABER_ACCOUNT}"),
    }
}

/// A state file an old per-user instance kept, which setup copies.
///
/// Deliberately not copied: `runner-variant/` (faber rebuilds it), the host
/// lock, caches, the updater's `staged/`, `marker` and `clean-exit`, and the
/// `faber.prev` / `config.prev` rollback pair, which belong to the old binary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FileKind {
    Config,
    /// Pins and holds.
    Overrides,
    ImageCache,
    /// Releases that crash-looped here and must not be offered again.
    Quarantine,
    /// `replay_outcomes` seeds learned caps from it.
    Audit,
}

impl FileKind {
    pub const ALL: [FileKind; 5] = [
        FileKind::Config,
        FileKind::Overrides,
        FileKind::ImageCache,
        FileKind::Quarantine,
        FileKind::Audit,
    ];

    /// Where the old instance kept it: its LEGACY layout, or for the config an
    /// explicit `--config` it ran with.
    pub fn source(self, legacy: &Paths, config_override: Option<&Path>) -> Option<PathBuf> {
        match self {
            FileKind::Config => config_override
                .map(Path::to_path_buf)
                .or_else(|| legacy.config_path.clone()),
            FileKind::Overrides => legacy.overrides_path.clone(),
            FileKind::ImageCache => legacy
                .state_dir
                .as_ref()
                .map(|d| d.join("image-cache.json")),
            FileKind::Quarantine => legacy
                .state_dir
                .as_ref()
                .map(|d| d.join("update/quarantine.json")),
            FileKind::Audit => legacy.log_dir.as_ref().map(|d| d.join("audit.jsonl")),
        }
    }

    /// Where the SYSTEM daemon reads it.
    pub fn dest(self, layout: &SystemLayout) -> PathBuf {
        match self {
            FileKind::Config => layout.config_path(),
            FileKind::Overrides => layout.overrides_path(),
            FileKind::ImageCache => layout.state_dir.join("image-cache.json"),
            FileKind::Quarantine => layout.state_dir.join("update/quarantine.json"),
            FileKind::Audit => layout.log_dir.join("audit.jsonl"),
        }
    }
}

/// One file to copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationItem {
    pub kind: FileKind,
    /// The account whose per-user instance this came from.
    pub account: String,
    pub source: PathBuf,
    /// The uid that must own `source`: that account's.
    pub owner_uid: u32,
    pub dest: PathBuf,
    /// `--remigrate`: move an existing destination aside first.
    pub replace: bool,
}

/// What a directory is today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DirProbe {
    Missing,
    Dir {
        uid: u32,
        gid: u32,
        mode: u32,
    },
    /// A symlink or a file where a directory belongs.
    NotADir(String),
    Unknown(String),
}

/// What a file setup creates is today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileProbe {
    Missing,
    File {
        uid: u32,
        gid: u32,
        mode: u32,
    },
    /// A symlink or anything else where a regular file belongs.
    NotRegular(String),
    Unknown(String),
}

/// What a binary path holds today.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinProbe {
    Missing,
    File {
        sha256: String,
        uid: u32,
        gid: u32,
        mode: u32,
        /// The version its marker claims ([`super::marker`]), read from its
        /// bytes, never by running it: a hint `_faber` could have written.
        version: Option<String>,
    },
    NotRegular,
    Unknown(String),
}

/// What an old instance's state file is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SrcProbe {
    Missing,
    Regular {
        len: u64,
    },
    /// A symlink, not a regular file, or not the account's: never copied.
    Refused(String),
    Unknown(String),
}

/// An old per-user faber: a LaunchAgent or `systemd --user` unit, loaded or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OldInstance {
    /// The plist or unit file, when there is one.
    pub definition: Option<PathBuf>,
    /// Why the definition cannot be moved aside safely.
    pub definition_problem: Option<String>,
    /// Loaded / active; `None` when this run cannot tell without sudo.
    pub loaded: Option<bool>,
    /// The control-panel port it was started with.
    pub port: u16,
    /// An explicit `--config` it was started with.
    pub config_override: Option<PathBuf>,
}

/// An account that ran faber per-user, or the operator, whose LEGACY state is
/// a migration candidate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyAccount {
    pub name: String,
    pub uid: u32,
    pub home: PathBuf,
    pub instance: Option<OldInstance>,
    pub files: Vec<(FileKind, PathBuf, SrcProbe)>,
    /// The operator's own identity that setup finds but does not copy.
    pub identity: Vec<String>,
    /// The LEGACY directories that exist in this home.
    pub dirs: Vec<LegacyDir>,
}

/// A LEGACY directory in an account's home: config and state, logs, cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyDir {
    pub path: PathBuf,
    /// Its owner, when it is a real directory; `None` for anything else (a
    /// symlink), which setup leaves alone.
    pub owner: Option<u32>,
}

/// The LEGACY directories an account's per-user faber used, from its
/// [`Paths`], each once: on macOS config and state share one.
pub fn legacy_dirs(paths: &Paths) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let candidates = [
        paths
            .config_path
            .as_ref()
            .and_then(|c| c.parent().map(Path::to_path_buf)),
        paths.state_dir.clone(),
        paths.log_dir.clone(),
        paths.cache_dir.clone(),
    ];
    for dir in candidates.into_iter().flatten() {
        if !dirs.contains(&dir) {
            dirs.push(dir);
        }
    }
    dirs
}

impl LegacyAccount {
    fn has_state(&self) -> bool {
        self.files
            .iter()
            .any(|(_, _, p)| !matches!(p, SrcProbe::Missing))
    }

    fn has_config(&self) -> bool {
        self.files
            .iter()
            .any(|(k, _, p)| *k == FileKind::Config && !matches!(p, SrcProbe::Missing))
    }
}

/// The setup record as it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordProbe {
    Absent,
    Trusted(SetupRecord),
    /// There is a record, and faber will not act on it.
    Untrusted(String),
}

/// The installed service definition, and whether the manager runs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceProbe {
    pub installed: Option<String>,
    pub loaded: bool,
    /// Loaded, but launchd is not running it: its `state` and `last exit
    /// code` as `launchctl print` gave them. A KeepAlive job launchd cannot
    /// spawn stays loaded and retries forever (#479). Probed for the VM
    /// LaunchDaemons only.
    pub stuck: Option<String>,
}

/// The person running setup: from `SUDO_UID`/`SUDO_USER`, looked up in the
/// directory service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OperatorInfo {
    pub name: String,
    pub uid: u32,
    /// The primary group.
    pub gid: u32,
    pub home: PathBuf,
}

/// The flags that change the plan.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PlanOpts {
    pub remigrate: bool,
    /// Replace the daemon copy whenever it differs from this binary, whatever
    /// its marker says: the repair path.
    pub replace_daemon: bool,
    pub keep_awake: bool,
    /// Migrate from this account instead of the one running sudo.
    pub migrate_from: Option<String>,
    /// Migrate nothing, and start from the starter config.
    pub fresh: bool,
    /// Leave the LEGACY directories where they are after a healthy start.
    pub keep_legacy: bool,
    /// The launcher, golden image and runner tarball to install.
    pub vm: super::vm_runtime::VmOpts,
    /// `--before-vm-runtime`: set up a macOS host even though it will have no
    /// runnable VM runtime, and so run no jobs ([`vm_runtime_gaps`], #223).
    pub before_vm_runtime: bool,
}

/// Everything [`plan`] needs to know about the host. Gathered by read-only
/// probes; fabricated in tests.
#[derive(Debug, Clone, PartialEq)]
pub struct HostState {
    pub os: Os,
    pub layout: SystemLayout,
    /// This binary's `FABER_VERSION`.
    pub version: String,
    /// Running as root.
    pub privileged: bool,
    pub operator: Result<OperatorInfo, String>,
    /// The running binary, resolved.
    pub exe_path: PathBuf,
    pub exe_sha256: Option<String>,
    /// The binary and its ancestors, for the root invariant.
    pub exe_chain: Result<Vec<PathFacts>, String>,
    /// The ancestors of the CLI copy's path that exist, nearest first, read
    /// without following symlinks.
    pub cli_chain: Vec<PathFacts>,
    /// The migration source's config, checked with this binary's
    /// `check-config` before anything is drained: `None` when no config is
    /// migrated, or it could not be checked without sudo.
    pub source_check: Option<Result<(), String>>,
    pub ids: UsedIds,
    pub faber_user: Option<UserInfo>,
    pub faber_group: Option<GroupInfo>,
    pub vm_user: Option<UserInfo>,
    pub vm_group: Option<GroupInfo>,
    /// [`FABER_OPERATOR_GROUP`], when it exists.
    pub operator_group: Option<GroupInfo>,
    /// Every group `_faber` belongs to (`id -G`), when it exists.
    pub faber_groups: Vec<u32>,
    pub vm_groups: Vec<u32>,
    /// Members of `admin` (macOS).
    pub admins: Vec<String>,
    /// The `docker` group (Linux).
    pub docker_group: Option<GroupInfo>,
    pub dirs: Vec<(DirSpec, DirProbe)>,
    pub files: Vec<(FileSpec, FileProbe)>,
    pub cli_bin: BinProbe,
    pub daemon_bin: BinProbe,
    pub legacy: Vec<LegacyAccount>,
    /// Destinations that already exist.
    pub dest_exists: Vec<FileKind>,
    pub record: RecordProbe,
    pub service: ServiceProbe,
    /// macOS: system sleep and Power Nap already off. `None` when unknown.
    pub keep_awake: Option<bool>,
    /// Every token-named entry in the credential store, read without
    /// following a symlink ([`crate::credential::token_files`]).
    pub credential_files: Vec<(PathBuf, crate::credential::FileFacts)>,
    /// Linux: root can reach a user's service manager (systemd 248+).
    pub user_manager: bool,
    /// macOS: the launcher, the wall's files, the VM daemons and the images.
    pub vm_runtime: super::vm_runtime::VmRuntimeProbe,
    /// macOS: the Linux VM's image and the launcher that builds it.
    pub linux_image: super::linux_image::LinuxImageProbe,
    /// macOS: the menu-bar app, installed and to install.
    pub app: super::app::AppProbe,
    pub opts: PlanOpts,
}

/// The ids both accounts will have.
pub fn planned_ids(s: &HostState) -> Result<(Account, Account), String> {
    let faber = accounts::plan_ids(s.faber_user.as_ref(), s.faber_group.as_ref(), &s.ids, &[])?;
    let vm = accounts::plan_ids(
        s.vm_user.as_ref(),
        s.vm_group.as_ref(),
        &s.ids,
        &[faber.uid, faber.gid],
    )?;
    Ok((faber, vm))
}

/// The operator group's gid, given both accounts' planned ids: its own, or
/// the lowest free one neither account takes.
fn operator_gid_beside(s: &HostState, faber: Account, vm: Account) -> Result<u32, String> {
    accounts::plan_gid(
        s.operator_group.as_ref(),
        &s.ids,
        &[faber.uid, faber.gid, vm.uid, vm.gid],
        FABER_OPERATOR_GROUP,
    )
}

/// Every id this run plans: both accounts' and the operator group's.
pub fn planned(s: &HostState) -> Result<Ids, String> {
    let (faber, vm) = planned_ids(s)?;
    Ok(Ids {
        faber,
        vm,
        operator_gid: operator_gid_beside(s, faber, vm)?,
    })
}

/// Every id root must not trust for the binary it runs: both accounts', as
/// they exist or are about to, and every group they are in. That includes the
/// operator group, which `_faber` joins (and on Linux runs as), and on Linux
/// `docker`, which setup adds `_faber` to.
pub fn untrusted(s: &HostState, ids: Option<(Account, Account)>) -> Untrusted {
    let mut u = Untrusted::default();
    for user in [&s.faber_user, &s.vm_user].into_iter().flatten() {
        u.uids.push(user.uid);
        u.gids.push(user.gid);
    }
    for group in [&s.faber_group, &s.vm_group, &s.operator_group]
        .into_iter()
        .flatten()
    {
        u.gids.push(group.gid);
    }
    u.gids.extend(&s.faber_groups);
    u.gids.extend(&s.vm_groups);
    if let Some((faber, vm)) = ids {
        u.uids.extend([faber.uid, vm.uid]);
        u.gids.extend([faber.gid, vm.gid]);
        u.gids.extend(operator_gid_beside(s, faber, vm));
    }
    if let Some(docker) = &s.docker_group {
        u.gids.push(docker.gid);
    }
    u.uids.sort_unstable();
    u.uids.dedup();
    u.gids.sort_unstable();
    u.gids.dedup();
    u
}

/// Whose per-user state setup migrates: the account that ran sudo, or the
/// one `--migrate-from` names. Never anyone else's, whatever else setup finds:
/// copying another account's config into the daemon would let that account
/// decide what the daemon runs. Other accounts' per-user fabers are still
/// stopped, since two daemons cannot share a host.
pub fn migration_source(s: &HostState) -> Result<Option<&LegacyAccount>, String> {
    if s.opts.fresh {
        return Ok(None);
    }
    let name = match (&s.opts.migrate_from, &s.operator) {
        (Some(name), _) => name.as_str(),
        (None, Ok(op)) => op.name.as_str(),
        (None, Err(_)) => return Ok(None),
    };
    match s.legacy.iter().find(|a| a.name == name) {
        Some(a) => Ok(Some(a)),
        None if s.opts.migrate_from.is_some() => Err(format!(
            "--migrate-from {name}: no account of that name with a home directory"
        )),
        None => Ok(None),
    }
}

/// The artifacts a setup on `os` creates, in creation order: what uninstall
/// removes.
pub fn artifacts(os: Os, layout: &SystemLayout) -> Vec<Artifact> {
    let dir = |p: &Path| Artifact::Dir {
        path: p.to_path_buf(),
    };
    let mut v = vec![
        Artifact::Group {
            name: FABER_ACCOUNT.into(),
        },
        Artifact::Group {
            name: FABER_VM_ACCOUNT.into(),
        },
        Artifact::Group {
            name: FABER_OPERATOR_GROUP.into(),
        },
        Artifact::User {
            name: FABER_ACCOUNT.into(),
        },
        Artifact::User {
            name: FABER_VM_ACCOUNT.into(),
        },
        dir(&layout.etc_dir),
        dir(&layout.state_dir),
        dir(&layout.log_dir),
        dir(&layout.cache_dir),
        dir(&layout.vm_home),
        Artifact::File {
            path: layout.cli_bin.clone(),
        },
        Artifact::File {
            path: layout.service_unit.clone(),
        },
    ];
    v.push(match os {
        Os::MacOs => Artifact::LaunchdJob {
            label: service::LABEL.into(),
        },
        Os::Linux => Artifact::SystemdUnit {
            name: service::UNIT_NAME.into(),
        },
    });
    v
}

/// The record this run writes. Entries a previous record carries that this
/// release does not create (a later release's, an unknown kind) are kept, so
/// uninstall still finds them.
pub fn record_for(
    s: &HostState,
    operator: &OperatorInfo,
    ids: Ids,
    migrated_from: Option<MigratedFrom>,
) -> SetupRecord {
    let Ids { faber, vm, .. } = ids;
    let mut artifacts = artifacts(s.os, &s.layout);
    if let RecordProbe::Trusted(previous) = &s.record {
        for a in &previous.artifacts {
            if !artifacts.contains(a) {
                artifacts.push(a.clone());
            }
        }
    }
    SetupRecord {
        schema: SCHEMA,
        operator: Operator {
            uid: operator.uid,
            name: operator.name.clone(),
        },
        accounts: Accounts {
            faber,
            faber_vm: vm,
            // The session account is the session sub-plan's to record, after it
            // has made the account (`session::apply`): this run keeps what the
            // previous record said, so a re-run plans no rewrite for it.
            faber_mac: match &s.record {
                RecordProbe::Trusted(previous) => previous.accounts.faber_mac.clone(),
                _ => None,
            },
            operator_group: Some(Group {
                gid: ids.operator_gid,
            }),
        },
        installed: Installed {
            version: s.version.clone(),
        },
        migrated_from,
        artifacts,
    }
}

pub(super) fn step(
    section: Section,
    status: Status,
    subject: impl Into<String>,
    detail: impl Into<String>,
) -> Step {
    Step {
        section,
        subject: subject.into(),
        status,
        detail: detail.into(),
        action: None,
        shown: Shown::default(),
    }
}

impl Step {
    pub(super) fn doing(mut self, action: Action) -> Self {
        self.action = Some(action);
        self
    }

    /// Print this step as part of the item `key`.
    pub(super) fn item(mut self, key: impl Into<String>) -> Self {
        self.shown.item = Some(key.into());
        self
    }

    /// Name this step's item `name` rather than its subject.
    pub(super) fn named(mut self, name: impl Into<String>) -> Self {
        self.shown.name = Some(name.into());
        self
    }

    /// What the default view says about this step.
    pub(super) fn brief(mut self, brief: impl Into<String>) -> Self {
        self.shown.brief = Some(brief.into());
        self
    }

    /// An `ok` step that setup still acts on: listed, not folded.
    pub(super) fn listed(mut self) -> Self {
        self.shown.listed = true;
        self
    }

    /// What the default view says about this step: its own brief, or one
    /// derived from its action (a command's detail is the command, which
    /// `--verbose` prints; a directory's owner and mode are `--verbose`'s).
    /// A refusal or a warning is its whole detail.
    pub fn brief_text(&self) -> String {
        if matches!(self.status, Status::Refused | Status::Skipped) {
            return self.detail.clone();
        }
        if let Some(brief) = &self.shown.brief {
            return brief.clone();
        }
        if !matches!(self.status, Status::WillCreate | Status::WillChange) {
            return self.detail.clone();
        }
        use super::vm_runtime::VmAction as V;
        match &self.action {
            Some(Action::Run(cmd)) => self.after_command(cmd).to_string(),
            Some(Action::LockPassword { .. }) => "no password".to_string(),
            Some(
                Action::EnsureDir(_)
                | Action::EnsureFile(_)
                | Action::Vm(
                    V::EnsureLauncherDir
                    | V::EnsureVmDir(_)
                    | V::EnsureToolsDir
                    | V::EnsureImageStore
                    | V::EnsureVerdictDir
                    | V::EnsureLog { .. },
                ),
            ) => {
                // A log or file setup creates empty says no more by default.
                let rest = without_mode(&self.detail);
                if rest == "empty" {
                    String::new()
                } else {
                    rest
                }
            }
            Some(Action::Migrate(item)) => format!(
                "{}copied from {}'s per-user install; the original stays",
                if item.replace {
                    "the existing copy moved aside, then "
                } else {
                    ""
                },
                item.account
            ),
            Some(Action::InstallDaemon) if self.status == Status::WillCreate => {
                "the daemon's own copy of this binary".to_string()
            }
            Some(Action::WriteRecord) => "written last; it makes this host SYSTEM".to_string(),
            Some(Action::InstallService { restart: true }) => {
                "the faber service: drained, stopped and started again on this binary".to_string()
            }
            Some(Action::InstallService { restart: false }) => {
                "the faber service; ready once its control socket answers".to_string()
            }
            Some(Action::Vm(V::InstallAnchor { .. })) => {
                "the VM wall service, run by root".to_string()
            }
            Some(Action::Vm(V::InstallSupervisor { .. })) => {
                "the VM supervisor service, run as _faber_vm".to_string()
            }
            Some(Action::Vm(V::WritePlan | V::WriteStored)) => self
                .detail
                .split_once(", rendered once")
                .map_or_else(|| self.detail.clone(), |(head, _)| head.to_string()),
            Some(Action::MoveLegacyAside { .. }) => {
                "renamed aside once the new daemon answers (--keep-legacy leaves it)".to_string()
            }
            Some(Action::KeepAwake) => "off: a sleeping Mac suspends every job".to_string(),
            _ => self.detail.clone(),
        }
    }

    /// The detail after the command it starts with: empty when the detail is
    /// only the command, which `--verbose` prints as a command.
    pub fn after_command(&self, cmd: &Cmd) -> &str {
        let shown = cmd.display();
        match self.detail.strip_prefix(shown.as_str()) {
            Some("") => "",
            Some(rest) => rest.strip_prefix(": ").unwrap_or(&self.detail),
            None => &self.detail,
        }
    }
}

/// A directory's or file's detail without its owner and mode, which
/// `--verbose` shows: `_faber:_faber 755: the supervisor reads them` is `the
/// supervisor reads them`, and `empty, root:wheel 640` is `empty`.
fn without_mode(detail: &str) -> String {
    let is_mode = |part: &str| {
        let mut words = part.split(' ');
        match (words.next(), words.next(), words.next()) {
            (Some(owner), Some(mode), None) => {
                (owner.contains(':') || owner == "root-owned")
                    && mode.len() >= 3
                    && mode.chars().all(|c| ('0'..='7').contains(&c))
            }
            _ => false,
        }
    };
    if is_mode(detail) {
        return String::new();
    }
    let first = [", ", ": "]
        .iter()
        .filter_map(|sep| detail.find(sep).map(|at| (at, sep.len())))
        .min();
    if let Some((at, len)) = first {
        if is_mode(&detail[..at]) {
            let rest = &detail[at + len..];
            return rest.strip_prefix("and ").unwrap_or(rest).to_string();
        }
    }
    match detail.rsplit_once(", ") {
        Some((head, tail)) if is_mode(tail) => head.to_string(),
        _ => detail.to_string(),
    }
}

/// An unknown state is a refusal when running as root, where it should have
/// been readable, and a guess in an unprivileged preview.
pub(super) fn unknown(s: &HostState, section: Section, subject: String, why: &str) -> Step {
    if s.privileged {
        step(section, Status::Refused, subject, why)
    } else {
        step(
            section,
            Status::WillChange,
            subject,
            "checked when run with sudo",
        )
    }
}

/// The plan. Pure: see the module doc.
pub fn plan(s: &HostState) -> Vec<Step> {
    let mut steps = Vec::new();
    let ids = planned(s);
    let accounts = ids.as_ref().ok().map(|i| (i.faber, i.vm));
    preflight(s, accounts, &mut steps);
    account_steps(s, &ids, &mut steps);
    // With no usable ids every account step above is already refused; the
    // rest of the plan still prints, owned by a placeholder no step executes.
    let placeholder = Account {
        uid: u32::MAX,
        gid: u32::MAX,
    };
    let owners = ids.clone().unwrap_or(Ids {
        faber: placeholder,
        vm: placeholder,
        operator_gid: u32::MAX,
    });
    dir_steps(s, owners, &mut steps);
    binary_steps(s, owners, &mut steps);
    let source = migration_source(s);
    old_instance_steps(s, &mut steps);
    migration_steps(s, &source, &mut steps);
    file_steps(s, owners, &mut steps);
    if let Ok(ids) = &ids {
        credential_steps(s, ids.faber, &mut steps);
    }
    if let (Ok(op), Ok(ids)) = (&s.operator, &ids) {
        record_step(s, op, *ids, &mut steps);
    }
    super::vm_runtime::plan_steps(s, ids.as_ref().ok().copied(), &mut steps);
    service_step(s, &mut steps);
    legacy_steps(s, &mut steps);
    keep_awake_step(s, &mut steps);
    steps.extend(super::app::plan_step(s, accounts));
    linux_image_step(s, &mut steps);
    vm_runtime_guard(s, &mut steps);
    steps
}

/// Why this run would leave a macOS host with no runnable VM runtime, and so
/// running no jobs at all (#223): under `_faber` the daemon cannot use the
/// operator's Docker Desktop, and there is no native backend (ADR-0026,
/// ADR-0052), so every job runs in a VM faber owns (ADR-0026) or nowhere.
///
/// Runnable means what setup itself puts in place, judged from `steps`, the
/// plan for this run: the VM launcher, installed by this run or already
/// installed and trusted, which the VM supervisor runs and the `macos-vm`
/// backend's VMs boot through; and the Linux VM's image, current already or
/// built by this run, which every `docker` profile's jobs run in. A golden
/// macOS image is not part of it: each `macos-vm` profile names its own, and
/// without one only that profile's jobs wait. Empty on Linux, which runs no
/// VMs, and when nothing is missing.
pub fn vm_runtime_gaps(s: &HostState, steps: &[Step]) -> Vec<String> {
    use super::vm_runtime::{VmAction, LAUNCHER_NAMES};
    if s.os != Os::MacOs {
        return Vec::new();
    }
    let mut gaps = Vec::new();
    let installs_launcher = steps
        .iter()
        .any(|st| st.action == Some(Action::Vm(VmAction::InstallLauncher)));
    if !installs_launcher {
        if let Err(why) = &s.linux_image.launcher {
            gaps.push(format!(
                "no VM launcher: none is beside this binary (as {}), none was given with \
                 --launcher, and {} is not usable ({why})",
                LAUNCHER_NAMES.join(" or "),
                s.layout.launcher_bin.display()
            ));
        }
    }
    let image = steps.iter().find(|st| st.section == Section::LinuxImage);
    if !image.is_some_and(|st| {
        matches!(
            st.status,
            Status::Ok | Status::WillCreate | Status::WillChange
        )
    }) {
        gaps.push(
            "the Linux VM's image is not built, and this run cannot build it, so no Docker \
             job can run"
                .into(),
        );
    }
    gaps
}

/// The step that refuses a macOS setup that would leave the host running no
/// jobs ([`vm_runtime_gaps`]), unless `--before-vm-runtime` says to go on, in
/// which case it says what will stop working. Placed after the preflight.
fn vm_runtime_guard(s: &HostState, steps: &mut Vec<Step>) {
    let gaps = vm_runtime_gaps(s, steps);
    if gaps.is_empty() {
        return;
    }
    let listed = gaps.join("; ");
    let st = if s.opts.before_vm_runtime {
        step(
            Section::Preflight,
            Status::Skipped,
            "VM runtime",
            format!(
                "warning: --before-vm-runtime: going on without one ({listed}). Once this \
                 host runs as {FABER_ACCOUNT} it runs NO jobs: Docker Desktop belongs to your \
                 login session and {FABER_ACCOUNT} cannot use it, there is no native \
                 backend, and there is no VM to run them in. Undo it with \
                 `sudo faber uninstall`"
            ),
        )
    } else {
        step(
            Section::Preflight,
            Status::Refused,
            "VM runtime",
            format!(
                "this binary cannot give the host a runnable VM runtime ({listed}). Under \
                 {FABER_ACCOUNT} the host would run no jobs: it cannot use Docker Desktop, and \
                 there is no native backend. Update this host by swapping \
                 the binary instead (docs/manual-update.md), or give setup the launcher \
                 (--launcher). --before-vm-runtime sets up anyway, for testing"
            ),
        )
    };
    let at = steps
        .iter()
        .position(|st| st.section != Section::Preflight)
        .unwrap_or(steps.len());
    steps.insert(at, st);
}

fn preflight(s: &HostState, ids: Option<(Account, Account)>, steps: &mut Vec<Step>) {
    let sec = Section::Preflight;
    steps.push(match &s.operator {
        Ok(op) => step(
            sec,
            Status::Ok,
            format!("operator {} (uid {})", op.name, op.uid),
            format!(
                "reads config, logs and the audit trail through group {FABER_OPERATOR_GROUP}, \
                 and never the GitHub token"
            ),
        ),
        Err(e) => step(sec, Status::Refused, "operator", e.clone()),
    });
    steps.push(match &s.exe_chain {
        Ok(chain) => match exe::verdict(chain, &untrusted(s, ids)) {
            Ok(()) => step(
                sec,
                Status::Ok,
                format!("this binary, {}", s.exe_path.display()),
                match exe::writable_by(chain) {
                    None => "nothing faber's own accounts can write".to_string(),
                    Some(writer) => format!(
                        "this binary is writable by {}; after this install, prefer `sudo {}`",
                        writer_name(s, writer),
                        s.layout.cli_bin.display()
                    ),
                },
            ),
            Err(reason) => step(sec, Status::Refused, "this binary", exe::refusal(&reason)),
        },
        Err(e) => step(
            sec,
            Status::Refused,
            "this binary",
            format!("cannot resolve it: {e}"),
        ),
    });
}

/// Who [`exe::writable_by`] found, as the plan names them.
fn writer_name(s: &HostState, writer: exe::Writer) -> String {
    match writer {
        exe::Writer::Account(uid) => match &s.operator {
            Ok(op) if op.uid == uid => op.name.clone(),
            _ => format!("uid {uid}"),
        },
        exe::Writer::Group(gid) => format!("the members of gid {gid}"),
    }
}

/// The operator's step that joins the operator group, by subject.
fn joins_subject(operator: &str) -> String {
    format!("{operator} in group {FABER_OPERATOR_GROUP}")
}

/// The operator's step that leaves `_faber`'s group, by subject.
fn leaves_subject(operator: &str) -> String {
    format!("{operator} out of group {FABER_ACCOUNT}")
}

/// Whether `steps` take the operator out of `_faber`'s group: a host set up
/// before the operator group, whose stored token every process the operator
/// ran could read.
pub fn leaves_faber(s: &HostState, steps: &[Step]) -> bool {
    s.operator.as_ref().is_ok_and(|op| {
        steps
            .iter()
            .any(|st| st.status == Status::WillChange && st.subject == leaves_subject(&op.name))
    })
}

fn account_steps(s: &HostState, ids: &Result<Ids, String>, steps: &mut Vec<Step>) {
    let sec = Section::Accounts;
    let Ids {
        faber,
        vm,
        operator_gid,
    } = match ids {
        Ok(ids) => *ids,
        Err(e) => {
            steps.push(step(sec, Status::Refused, "account ids", e.clone()));
            return;
        }
    };
    let operator = s.operator.as_ref().ok().map(|op| op.name.as_str());
    let accounts = [
        (FABER_ACCOUNT, &s.faber_user, &s.faber_group, faber),
        (FABER_VM_ACCOUNT, &s.vm_user, &s.vm_group, vm),
    ];
    let groups = [
        (FABER_ACCOUNT, &s.faber_group, faber.gid),
        (FABER_VM_ACCOUNT, &s.vm_group, vm.gid),
        (FABER_OPERATOR_GROUP, &s.operator_group, operator_gid),
    ];
    for (name, group, gid) in groups {
        steps.push(match group {
            None => {
                let cmd = accounts::create_group(s.os, name, gid);
                step(
                    sec,
                    Status::WillCreate,
                    format!("group {name} (gid {gid})"),
                    cmd.display(),
                )
                .doing(Action::Run(cmd))
                .named(format!("group {name}"))
                .brief(format!("gid {gid}"))
            }
            Some(g) => {
                let problems = accounts::group_problems(s.os, g, operator);
                if problems.is_empty() {
                    step(sec, Status::Ok, format!("group {name} (gid {})", g.gid), "")
                        .named(format!("group {name}"))
                        .brief(format!("gid {}", g.gid))
                } else {
                    step(
                        sec,
                        Status::Refused,
                        format!("group {name}"),
                        problems.join("; "),
                    )
                }
            }
        });
    }
    for (name, user, group, id) in &accounts {
        let home = accounts::home(name, &s.layout);
        match user {
            None => {
                let cmd = accounts::create_user(s.os, name, id.uid, id.gid, &home);
                steps.push(
                    step(
                        sec,
                        Status::WillCreate,
                        format!("user {name} (uid {}, gid {})", id.uid, id.gid),
                        cmd.display(),
                    )
                    .doing(Action::Run(cmd))
                    .item(format!("user {name}"))
                    .brief(format!("uid {}", id.uid)),
                );
                if s.os == Os::MacOs {
                    let [set, delete] = accounts::lock_password(name);
                    steps.push(
                        step(
                            sec,
                            Status::WillCreate,
                            format!("{name} with no password"),
                            format!(
                                "{}; {} if it has one; then checked, as for _www",
                                set.display(),
                                delete.display()
                            ),
                        )
                        .doing(Action::LockPassword {
                            name: name.to_string(),
                        })
                        .item(format!("user {name}")),
                    );
                    let hide = accounts::hide_user(name);
                    steps.push(
                        step(
                            sec,
                            Status::WillCreate,
                            format!("{name} hidden"),
                            hide.display(),
                        )
                        .doing(Action::Run(hide))
                        .item(format!("user {name}"))
                        .brief("hidden"),
                    );
                    // `sysadminctl -roleAccount` ignores `-home` on macOS 27.
                    let set_home = accounts::set_home(name, &home);
                    steps.push(
                        step(
                            sec,
                            Status::WillCreate,
                            format!("{name}'s home {}", home.display()),
                            set_home.display(),
                        )
                        .doing(Action::Run(set_home))
                        .item(format!("user {name}"))
                        .brief(""),
                    );
                }
            }
            Some(u) => {
                let problems = accounts::user_problems(s.os, u, &home, group.as_ref(), &s.admins);
                if !problems.is_empty() {
                    steps.push(
                        step(
                            sec,
                            Status::Refused,
                            format!("user {name}"),
                            format!(
                                "{}; setup does not adopt an account of another shape",
                                problems.join("; ")
                            ),
                        )
                        .item(format!("user {name}")),
                    );
                    continue;
                }
                steps.push(
                    step(
                        sec,
                        Status::Ok,
                        format!("user {name} (uid {}, gid {})", u.uid, u.gid),
                        "",
                    )
                    .item(format!("user {name}"))
                    .brief(format!("uid {}", u.uid)),
                );
                if s.os == Os::MacOs && !u.hidden {
                    let hide = accounts::hide_user(name);
                    steps.push(
                        step(
                            sec,
                            Status::WillChange,
                            format!("{name} hidden"),
                            hide.display(),
                        )
                        .doing(Action::Run(hide))
                        .item(format!("user {name}"))
                        .brief("hidden"),
                    );
                }
                if accounts::home_needs_setting(s.os, u, &home) {
                    let set_home = accounts::set_home(name, &home);
                    steps.push(
                        step(
                            sec,
                            Status::WillChange,
                            format!("{name}'s home {}", home.display()),
                            format!(
                                "{}: sysadminctl -roleAccount left it at {}",
                                set_home.display(),
                                accounts::ROLE_ACCOUNT_HOME
                            ),
                        )
                        .doing(Action::Run(set_home))
                        .item(format!("user {name}"))
                        .brief(format!(
                            "home {}, which sysadminctl -roleAccount left at {}",
                            home.display(),
                            accounts::ROLE_ACCOUNT_HOME
                        )),
                    );
                }
            }
        }
    }
    // The operator group: the operator reads through it, and `_faber` shares
    // through it what it writes for the operator. Joined before the operator
    // leaves `_faber`'s group, so no session started after this run is
    // without read access.
    let in_operator_group = |user: &str| {
        s.operator_group
            .as_ref()
            .is_some_and(|g| g.has_member(user))
    };
    let named = format!("group {FABER_OPERATOR_GROUP}");
    if let Ok(op) = &s.operator {
        let subject = joins_subject(&op.name);
        steps.push(if in_operator_group(&op.name) {
            step(sec, Status::Ok, subject, "")
                .named(&named)
                .brief(format!("has you ({}) as a member", op.name))
        } else {
            let cmd = accounts::add_member(s.os, FABER_OPERATOR_GROUP, &op.name);
            step(sec, Status::WillChange, subject, cmd.display())
                .doing(Action::Run(cmd))
                .named(&named)
                .brief(format!("adds you ({})", op.name))
        });
    }
    let subject = format!("{FABER_ACCOUNT} in group {FABER_OPERATOR_GROUP}");
    steps.push(if in_operator_group(FABER_ACCOUNT) {
        step(sec, Status::Ok, subject, "")
            .named(&named)
            .brief(format!("has {FABER_ACCOUNT} as a member"))
    } else {
        let cmd = accounts::add_member(s.os, FABER_OPERATOR_GROUP, FABER_ACCOUNT);
        step(
            sec,
            Status::WillChange,
            subject,
            format!(
                "{}: the daemon shares what it writes for you through this group, from its next \
                 start",
                cmd.display()
            ),
        )
        .doing(Action::Run(cmd))
        .named(&named)
        .brief(format!(
            "adds {FABER_ACCOUNT}, which shares what it writes for you"
        ))
    });
    // `_faber`'s group reads the token, and a setup before the operator group
    // put the operator in it.
    if let Ok(op) = &s.operator {
        if s.faber_group
            .as_ref()
            .is_some_and(|g| g.has_member(&op.name))
        {
            let cmd = accounts::remove_member(s.os, FABER_ACCOUNT, &op.name);
            steps.push(
                step(
                    sec,
                    Status::WillChange,
                    leaves_subject(&op.name),
                    format!(
                        "{}: {FABER_ACCOUNT}'s group reads the GitHub token, so every process you \
                         ran could read it. Sessions started before this keep the group: log out \
                         of them, then replace the token",
                        cmd.display()
                    ),
                )
                .doing(Action::Run(cmd))
                .named(format!("group {FABER_ACCOUNT}"))
                .brief(format!(
                    "takes you ({}) out: only faber reads its GitHub token",
                    op.name
                )),
            );
        }
    }
    if s.os == Os::Linux {
        let subject = format!("{FABER_ACCOUNT} in group docker");
        steps.push(match &s.docker_group {
            Some(g) if g.has_member(FABER_ACCOUNT) => step(
                sec,
                Status::Ok,
                subject,
                "root-equivalent until ADR-0026 puts job code behind a wall",
            )
            // A security property: shown, never folded (ADR-0050).
            .listed(),
            Some(_) => {
                let cmd = accounts::add_member(s.os, "docker", FABER_ACCOUNT);
                step(
                    sec,
                    Status::WillChange,
                    subject,
                    format!(
                        "{}: Docker access, which is root-equivalent on this host until ADR-0026 \
                         puts job code behind a wall",
                        cmd.display()
                    ),
                )
                .doing(Action::Run(cmd))
            }
            None => step(
                sec,
                Status::Ok,
                "no docker group on this host",
                "Docker profiles cannot run until Docker Engine is installed",
            )
            .listed(),
        });
    }
}

fn dir_steps(s: &HostState, ids: Ids, steps: &mut Vec<Step>) {
    let sec = Section::Directories;
    for (spec, probe) in &s.dirs {
        let owner = owner_account(spec.owner, ids);
        let want = format!("{} {:o}", owner_name(s.os, spec.owner), spec.mode);
        let subject = spec.path.display().to_string();
        steps.push(match probe {
            DirProbe::Missing => {
                step(sec, Status::WillCreate, subject, want).doing(Action::EnsureDir(spec.clone()))
            }
            DirProbe::Dir {
                uid: u,
                gid: g,
                mode,
            } => {
                // Inside `_faber`'s state directory only `_faber`'s own is
                // taken over: `_faber` could have moved one of root's there
                // (#228, as [`super::fsx::ensure_dir_beneath`] judges it).
                let prior = if spec.in_state_dir {
                    vec![owner.uid]
                } else {
                    prior_owners(spec.owner, ids)
                };
                if !prior.contains(u) {
                    step(
                        sec,
                        Status::Refused,
                        subject,
                        format!(
                            "owned by uid {u}, neither root nor {}: something else created it. \
                             If nothing in it is yours, remove it (`sudo rm -rf \"{}\"`) and \
                             re-run",
                            owner_name(s.os, spec.owner),
                            spec.path.display()
                        ),
                    )
                } else if (*u, *g, mode & 0o7777) == (owner.uid, owner.gid, spec.mode) {
                    step(sec, Status::Ok, subject, want)
                } else {
                    step(
                        sec,
                        Status::WillChange,
                        subject,
                        format!(
                            "{u}:{g} {:o} -> {want} (this directory only, never its contents)",
                            mode & 0o7777
                        ),
                    )
                    .doing(Action::EnsureDir(spec.clone()))
                }
            }
            DirProbe::NotADir(what) => step(
                sec,
                Status::Refused,
                subject,
                format!("{what} where a directory belongs; move it aside and re-run"),
            ),
            DirProbe::Unknown(e) => unknown(s, sec, subject, e),
        });
    }
}

fn binary_steps(s: &HostState, ids: Ids, steps: &mut Vec<Step>) {
    let faber = ids.faber;
    let sec = Section::Binaries;
    let Some(ours) = s.exe_sha256.as_deref() else {
        steps.push(step(
            sec,
            Status::Refused,
            "this binary",
            "cannot read it to copy it",
        ));
        return;
    };
    let version = &s.version;

    let cli = s.layout.cli_bin.display().to_string();
    let chain_problem = s.cli_chain.iter().find_map(|f| {
        fsx::root_dir_problem(&f.path, f.mode & 0o170_000 == 0o040_000, f.uid, f.mode)
    });
    steps.push(match &s.cli_bin {
        _ if chain_problem.is_some() => step(
            sec,
            Status::Refused,
            cli,
            format!(
                "{}: any process running as that account could replace the root copy that \
                 `sudo faber` runs, as an operator-owned /usr/local/bin from some older Homebrew \
                 setups would allow. Make every directory above it root-owned and writable only \
                 by root, then re-run",
                chain_problem.unwrap_or_default()
            ),
        ),
        BinProbe::Missing => step(
            sec,
            Status::WillCreate,
            cli,
            format!("this binary ({version}), root-owned 0755: what `sudo faber` runs"),
        )
        .doing(Action::InstallCli),
        BinProbe::File {
            sha256,
            uid,
            gid,
            mode,
            ..
        } if sha256 == ours && (*uid, *gid, mode & 0o7777) == (0, 0, 0o755) => {
            step(sec, Status::Ok, cli, format!("this binary ({version})"))
        }
        BinProbe::File { .. } => step(
            sec,
            Status::WillChange,
            cli,
            format!("replaced with this binary ({version}), root-owned 0755"),
        )
        .doing(Action::InstallCli),
        BinProbe::NotRegular => step(
            sec,
            Status::Refused,
            cli,
            "not a regular file; move it aside and re-run",
        ),
        BinProbe::Unknown(e) => unknown(s, sec, cli, e),
    });

    let daemon = s.layout.daemon_bin.display().to_string();
    // `_faber`'s, 0755: setup writes it with its directory's group, the
    // operator group, and the updater's copy takes whichever group the
    // directory or the daemon gives it. Neither group can write it.
    let owned = |uid: &u32, gid: &u32, mode: &u32| {
        *uid == faber.uid && [faber.gid, ids.operator_gid].contains(gid) && mode & 0o7777 == 0o755
    };
    steps.push(match &s.daemon_bin {
        BinProbe::Missing => step(
            sec,
            Status::WillCreate,
            daemon,
            format!("this binary ({version}), owned by {FABER_ACCOUNT}: the copy the daemon runs and updates"),
        )
        .doing(Action::InstallDaemon),
        BinProbe::File {
            sha256,
            uid,
            gid,
            mode,
            ..
        } if sha256 == ours => {
            if owned(uid, gid, mode) {
                step(sec, Status::Ok, daemon, format!("this binary ({version})"))
            } else {
                step(sec, Status::WillChange, daemon, "rewritten with the right owner and mode")
                    .doing(Action::InstallDaemon)
            }
        }
        BinProbe::File { version: theirs, .. } => {
            // The marker is `_faber`'s to write, so it is a hint that decides
            // one thing only: keep a newer copy rather than replace it.
            let theirs = theirs.as_deref().map(super::marker::version_chars);
            match theirs {
                _ if s.opts.replace_daemon => step(
                    sec,
                    Status::WillChange,
                    daemon,
                    format!("replaced with this binary ({version}) (--replace-daemon)"),
                )
                .doing(Action::InstallDaemon),
                Some(v) if crate::update::is_newer(&v, version) => step(
                    sec,
                    Status::Ok,
                    daemon,
                    format!(
                        "kept: its version marker says {v}, newer than this binary ({version}). \
                         The marker is unverified; --replace-daemon replaces the copy anyway"
                    ),
                ),
                Some(v) => step(
                    sec,
                    Status::WillChange,
                    daemon,
                    format!("{v} replaced with this binary ({version})"),
                )
                .doing(Action::InstallDaemon),
                None => step(
                    sec,
                    Status::WillChange,
                    daemon,
                    format!(
                        "has no version marker (a release older than this one, or not faber): \
                         replaced with this binary ({version})"
                    ),
                )
                .doing(Action::InstallDaemon),
            }
        }
        BinProbe::NotRegular if s.opts.replace_daemon => step(
            sec,
            Status::WillChange,
            daemon,
            "not a regular file: moved aside and replaced (--replace-daemon)",
        )
        .doing(Action::InstallDaemon),
        BinProbe::NotRegular => step(
            sec,
            Status::Refused,
            daemon,
            "not a regular file; --replace-daemon moves it aside and replaces it",
        ),
        BinProbe::Unknown(e) if s.opts.replace_daemon && s.privileged => step(
            sec,
            Status::WillChange,
            daemon,
            format!("unreadable ({e}): moved aside and replaced (--replace-daemon)"),
        )
        .doing(Action::InstallDaemon),
        BinProbe::Unknown(e) => unknown(
            s,
            sec,
            daemon,
            &format!("{e}; --replace-daemon replaces it anyway"),
        ),
    });
}

fn old_instance_steps(s: &HostState, steps: &mut Vec<Step>) {
    let sec = Section::OldInstances;
    let mut any = false;
    for acct in &s.legacy {
        let Some(inst) = &acct.instance else {
            continue;
        };
        any = true;
        let subject = format!("{}'s per-user faber", acct.name);
        if let Some(problem) = &inst.definition_problem {
            let def = inst
                .definition
                .as_ref()
                .map_or_else(String::new, |d| d.display().to_string());
            steps.push(step(
                sec,
                Status::Refused,
                subject,
                format!("{def} {problem}; move it aside yourself, then re-run"),
            ));
            continue;
        }
        if s.os == Os::Linux && !s.user_manager && inst.loaded == Some(true) {
            steps.push(step(
                sec,
                Status::Refused,
                subject,
                format!(
                    "this systemd cannot stop another user's unit; as {}, run `faber drain --port \
                     {}` and `systemctl --user disable --now {}`, then re-run",
                    acct.name,
                    inst.port,
                    service::UNIT_NAME
                ),
            ));
            continue;
        }
        let stop = match s.os {
            Os::MacOs => format!("stop {}", service::gui_target(acct.uid)),
            Os::Linux => format!("disable --now its {}", service::UNIT_NAME),
        };
        let aside = inst.definition.as_ref().map_or_else(String::new, |d| {
            format!(
                ", move {} aside to {}.pre-system-<time>",
                d.display(),
                d.display()
            )
        });
        let state = match inst.loaded {
            Some(true) => "running",
            Some(false) => "not running",
            None => "state checked with sudo",
        };
        let mut st = step(
            sec,
            Status::WillChange,
            subject,
            format!(
                "{state}: drain its control socket or 127.0.0.1:{} (waits for admitted jobs), {stop}{aside}",
                inst.port
            ),
        )
        .doing(Action::StopOld {
            account: acct.name.clone(),
        });
        // Named by its file on disk, as every item is (ADR-0050).
        st = match &inst.definition {
            Some(def) => st.named(def.display().to_string()).brief(format!(
                "{}'s per-user faber, {state}: drained (waits for admitted jobs), stopped, and \
                 this file moved aside",
                acct.name
            )),
            None => st.brief(format!(
                "{state}: drained (waits for admitted jobs) and stopped"
            )),
        };
        steps.push(st);
    }
    if !any {
        steps.push(step(sec, Status::Ok, "none found", ""));
    }
}

fn migration_steps(
    s: &HostState,
    source: &Result<Option<&LegacyAccount>, String>,
    steps: &mut Vec<Step>,
) {
    let sec = Section::Migration;
    let source = match source {
        Ok(source) => *source,
        Err(e) => {
            steps.push(step(sec, Status::Refused, "migration source", e.clone()));
            return;
        }
    };
    let mut config_from_source = false;
    let mut writes_config = false;
    if let Some(acct) = source {
        for (kind, src, probe) in &acct.files {
            let dest = kind.dest(&s.layout);
            let subject = dest.display().to_string();
            let exists = s.dest_exists.contains(kind);
            let item = |replace| MigrationItem {
                kind: *kind,
                account: acct.name.clone(),
                source: src.clone(),
                owner_uid: acct.uid,
                dest: dest.clone(),
                replace,
            };
            if *kind == FileKind::Config && !matches!(probe, SrcProbe::Missing) {
                config_from_source = true;
            }
            match probe {
                SrcProbe::Missing => {}
                SrcProbe::Regular { len } => {
                    if exists && !s.opts.remigrate {
                        // One item with the step that gives it its group,
                        // if it needs one ([`file_steps`]).
                        steps.push(
                            step(
                                sec,
                                Status::Ok,
                                subject.clone(),
                                format!(
                                    "kept: it exists already (--remigrate copies {} again)",
                                    src.display()
                                ),
                            )
                            .item(subject),
                        );
                    } else {
                        if *kind == FileKind::Config {
                            writes_config = true;
                        }
                        let (status, what) = if exists {
                            (
                                Status::WillChange,
                                format!(
                                    "existing copy moved aside to {}.pre-remigrate-<time>, then ",
                                    kind.dest(&s.layout)
                                        .file_name()
                                        .map_or_else(String::new, |n| n
                                            .to_string_lossy()
                                            .into_owned())
                                ),
                            )
                        } else {
                            (Status::WillCreate, String::new())
                        };
                        steps.push(
                            step(
                                sec,
                                status,
                                subject,
                                format!(
                                    "{what}copied from {} ({len} bytes; the original stays, its \
                                     sha256 goes in the record)",
                                    src.display()
                                ),
                            )
                            .doing(Action::Migrate(item(exists))),
                        );
                    }
                }
                SrcProbe::Refused(why) => steps.push(step(
                    sec,
                    Status::Refused,
                    subject,
                    format!("{} {why}", src.display()),
                )),
                SrcProbe::Unknown(e) => {
                    let mut st = unknown(s, sec, subject, &format!("{}: {e}", src.display()));
                    if st.status != Status::Refused {
                        st.detail = format!("copied from {} (read with sudo)", src.display());
                    }
                    steps.push(st);
                }
            }
        }
        for flag in &acct.identity {
            steps.push(step(sec, Status::Ok, "not copied", flag.clone()).listed());
        }
    }
    let source_name = source.map(|a| a.name.as_str());
    // The starter config would be seeded: nothing of the source's to migrate,
    // and no config in place yet. If another account's per-user faber has a
    // config, seeding past it silently is not an answer: ask which.
    let would_seed = !config_from_source && !s.dest_exists.contains(&FileKind::Config);
    let mut refused_for = Vec::new();
    // With no operator there is no source to compare against: preflight has
    // already refused, and naming the operator's own account here as
    // "another" would only confuse.
    let source_known = s.operator.is_ok() || s.opts.migrate_from.is_some() || s.opts.fresh;
    for other in s
        .legacy
        .iter()
        .filter(|a| source_known && Some(a.name.as_str()) != source_name && a.has_state())
    {
        if would_seed && !s.opts.fresh && other.has_config() {
            refused_for.push(other.name.clone());
            steps.push(step(
                sec,
                Status::Refused,
                format!("{}'s per-user config", other.name),
                format!(
                    "found faber config for {0}: pass --migrate-from {0} to migrate it, or \
                     --fresh to start from the starter config",
                    other.name
                ),
            ));
        } else if s.opts.fresh {
            // Not copied, but not necessarily left alone: the operator's
            // directories are still renamed aside (`legacy_steps`).
            steps.push(
                step(
                    sec,
                    Status::Ok,
                    format!("not copied: {}'s per-user state", other.name),
                    if moves_aside(s, other) {
                        "--fresh: not copied. Renamed aside once the new daemon answers, as listed \
                     under \"your old per-user files\", and kept there as the backup"
                    } else {
                        "--fresh: not copied, and left where it is"
                    },
                )
                .listed(),
            );
        } else {
            steps.push(
                step(
                    sec,
                    Status::Ok,
                    format!("not copied: {}'s per-user state", other.name),
                    format!(
                    "setup migrates only from the account that ran sudo; pass --migrate-from {} \
                     to copy it instead",
                    other.name
                ),
                )
                .listed(),
            );
        }
    }
    let config = s.layout.config_path().display().to_string();
    let migrates_config = steps.iter().any(
        |st| matches!(&st.action, Some(Action::Migrate(item)) if item.kind == FileKind::Config),
    );
    if migrates_config {
        let checker = match &s.operator {
            Ok(op) if s.privileged => op.name.clone(),
            _ => "you".to_string(),
        };
        steps.push(match &s.source_check {
            Some(Ok(())) => step(
                sec,
                Status::Ok,
                "source config check",
                format!(
                    "the config to migrate passes this binary's check-config, run as {checker} \
                     before anything is drained or stopped"
                ),
            ),
            Some(Err(why)) => step(
                sec,
                Status::Refused,
                "source config check",
                format!(
                    "the config to migrate fails this binary's check-config (run as {checker}, \
                     before anything was drained or stopped). Fix it where it is, then re-run:\n{why}"
                ),
            ),
            None if s.privileged => step(
                sec,
                Status::Ok,
                "source config check",
                "not run: only the check as _faber after the copy applies",
            ),
            None => step(
                sec,
                Status::Ok,
                "source config check",
                "checked when run with sudo, before anything is drained or stopped",
            ),
        });
    }
    if !config_from_source && refused_for.is_empty() {
        if s.dest_exists.contains(&FileKind::Config) {
            steps.push(step(sec, Status::Ok, config, "kept"));
        } else {
            writes_config = true;
            steps.push(
                step(
                    sec,
                    Status::WillCreate,
                    config,
                    "the starter config (deploy/config.starter.toml); autoupdate is asked \
                     separately and defaults to off",
                )
                .doing(Action::SeedConfig),
            );
        }
    }
    if writes_config {
        steps.push(
            step(
                sec,
                Status::Ok,
                "config check",
                format!(
                    "{} check-config runs on it as {FABER_ACCOUNT} before the service starts; a \
                     failure stops setup and restores what it stopped",
                    s.layout.cli_bin.display()
                ),
            )
            .listed()
            .brief(format!(
                "run as {FABER_ACCOUNT} before the service starts; a failure stops setup and \
                 restores what it stopped"
            )),
        );
    }
}

/// The files setup gives their owner and mode ([`file_specs`]): a log file
/// under "log files", a state file under "config and state". One this run
/// copies or seeds is written with the right owner already, and has no step.
fn file_steps(s: &HostState, ids: Ids, steps: &mut Vec<Step>) {
    let written: Vec<PathBuf> = steps
        .iter()
        .filter_map(|st| match &st.action {
            Some(Action::Migrate(item)) => Some(item.dest.clone()),
            Some(Action::SeedConfig) => Some(s.layout.config_path()),
            _ => None,
        })
        .collect();
    for (spec, probe) in &s.files {
        if written.contains(&spec.path) {
            continue;
        }
        let sec = if spec.create {
            Section::LogFiles
        } else {
            Section::Migration
        };
        let owner = owner_account(spec.owner, ids);
        let prior = if spec.in_faber_dir {
            vec![ids.faber.uid]
        } else {
            prior_owners(spec.owner, ids)
        };
        let want = format!("{} {:o}", owner_name(s.os, spec.owner), spec.mode);
        let subject = spec.path.display().to_string();
        let remove = format!("`sudo rm \"{}\"`", spec.path.display());
        let keyed = |st: Step| {
            if spec.create {
                st
            } else {
                let key = st.subject.clone();
                st.item(key)
            }
        };
        steps.push(keyed(match probe {
            FileProbe::Missing if !spec.create => continue,
            FileProbe::Missing => step(sec, Status::WillCreate, subject, format!("empty, {want}"))
                .doing(Action::EnsureFile(spec.clone())),
            FileProbe::File {
                uid: u,
                gid: g,
                mode,
            } => {
                if !prior.contains(u) {
                    step(
                        sec,
                        Status::Refused,
                        subject,
                        format!(
                            "owned by uid {u}, neither root nor {}: something else created it. \
                             Remove it ({remove}) and re-run",
                            owner_name(s.os, spec.owner)
                        ),
                    )
                } else if (*u, *g, mode & 0o7777) == (owner.uid, owner.gid, spec.mode) {
                    step(sec, Status::Ok, subject, want)
                } else {
                    let st = step(
                        sec,
                        Status::WillChange,
                        subject,
                        format!("{u}:{g} {:o} -> {want}", mode & 0o7777),
                    )
                    .doing(Action::EnsureFile(spec.clone()));
                    // Every file here is the operator's to read: a group
                    // change is the one a host set up before that group needs.
                    if *g == owner.gid {
                        st
                    } else {
                        st.brief(format!(
                            "given group {FABER_OPERATOR_GROUP}, which you read it through"
                        ))
                    }
                }
            }
            FileProbe::NotRegular(what) => step(
                sec,
                Status::Refused,
                subject,
                format!(
                    "{what} where {} belongs; remove it ({remove}) and re-run",
                    if spec.create {
                        "a log file"
                    } else {
                        "faber's file"
                    }
                ),
            ),
            FileProbe::Unknown(e) => unknown(s, sec, subject, e),
        }));
    }
}

fn record_step(s: &HostState, op: &OperatorInfo, ids: Ids, steps: &mut Vec<Step>) {
    let sec = Section::Record;
    let subject = s.layout.setup_record.display().to_string();
    let migrates = steps
        .iter()
        .any(|st| matches!(st.action, Some(Action::Migrate(_))));
    let previous_migration = match &s.record {
        RecordProbe::Trusted(r) => r.migrated_from.clone(),
        _ => None,
    };
    let want = record_for(s, op, ids, previous_migration);
    steps.push(match &s.record {
        RecordProbe::Absent => step(
            sec,
            Status::WillCreate,
            subject,
            "root-owned 0644, written last before the service starts: it is what makes this \
             host SYSTEM",
        )
        .doing(Action::WriteRecord),
        RecordProbe::Untrusted(why) => step(
            sec,
            Status::WillChange,
            subject,
            format!("untrusted today ({why}); rewritten root-owned"),
        )
        .doing(Action::WriteRecord),
        RecordProbe::Trusted(r) if *r == want && !migrates => step(sec, Status::Ok, subject, ""),
        RecordProbe::Trusted(_) => step(
            sec,
            Status::WillChange,
            subject,
            "rewritten with this run's accounts, version and artifacts",
        )
        .doing(Action::WriteRecord),
    });
}

/// The session account the session sub-plan wants the record to name
/// ([`super::session::SessionAction::Record`]): `Some(None)` to drop it,
/// `None` when the record already says what it should.
pub fn session_record(steps: &[Step]) -> Option<Option<crate::platform::record::SessionAccount>> {
    steps.iter().find_map(|st| match &st.action {
        Some(Action::Session(super::session::SessionAction::Record(a)))
            if matches!(st.status, Status::WillCreate | Status::WillChange) =>
        {
            Some(a.clone())
        }
        _ => None,
    })
}

/// Fold the session sub-plan's record change into the main plan, which it is
/// planned after: the record is rewritten before the daemon starts, and a
/// running daemon is drained and restarted, since it reads the record only at
/// start. Without such a change, nothing moves.
pub fn fold_session_record(steps: &mut [Step]) {
    if session_record(steps).is_none() {
        return;
    }
    for st in steps.iter_mut() {
        match st.section {
            Section::Record if st.status == Status::Ok => {
                st.status = Status::WillChange;
                st.detail = "rewritten: it names the macOS VM session account".to_string();
                st.action = Some(Action::WriteRecord);
            }
            // The one service step: the daemon's own.
            Section::Service if st.status == Status::Ok => {
                st.status = Status::WillChange;
                st.detail = "the running daemon is drained (control socket) and stopped, then \
                             started again, so it reads the record's session account; ready once \
                             the control socket answers"
                    .to_string();
                st.action = Some(Action::InstallService { restart: true });
            }
            _ => {}
        }
    }
}

fn service_step(s: &HostState, steps: &mut Vec<Step>) {
    let sec = Section::Service;
    let rendered = service::render(s.os, &s.layout);
    let subject = s.layout.service_unit.display().to_string();
    let swaps_binary = steps
        .iter()
        .any(|st| st.action == Some(Action::InstallDaemon));
    // A running daemon writes its state from memory: a copy made under it
    // (`--remigrate`) would be overwritten by its next save, and a new config
    // is read only at start. So state setup rewrites means a restart too.
    let rewrites_state = steps.iter().any(|st| {
        matches!(
            st.action,
            Some(Action::Migrate(_)) | Some(Action::SeedConfig)
        )
    });
    // A process keeps the groups it started with: the daemon holds the
    // operator group only once it starts again after joining it.
    let joins = Action::Run(accounts::add_member(
        s.os,
        FABER_OPERATOR_GROUP,
        FABER_ACCOUNT,
    ));
    let regroups = steps.iter().any(|st| st.action.as_ref() == Some(&joins));
    let changed = s.service.installed.as_deref() != Some(rendered.as_str());
    let start = match s.os {
        Os::MacOs => format!(
            "plutil -lint, then launchctl enable, bootstrap and kickstart {}",
            service::system_target()
        ),
        Os::Linux => format!(
            "systemctl daemon-reload, then systemctl enable --now {}",
            service::UNIT_NAME
        ),
    };
    let ready = "ready once the control socket answers".to_string();
    steps.push(match (&s.service.installed, s.service.loaded) {
        (None, _) => step(
            sec,
            Status::WillCreate,
            subject,
            format!("{start}; {ready}"),
        )
        .doing(Action::InstallService { restart: false }),
        (Some(_), true) if !changed && !swaps_binary && !rewrites_state && !regroups => step(
            sec,
            Status::Ok,
            subject,
            format!("running as {FABER_ACCOUNT}"),
        ),
        (Some(_), true) if !changed && !swaps_binary && !rewrites_state => step(
            sec,
            Status::WillChange,
            subject,
            format!(
                "the running daemon is drained (control socket) and stopped, then {start}, so \
                 it starts in group {FABER_OPERATOR_GROUP}; {ready}"
            ),
        )
        .doing(Action::InstallService { restart: true })
        .brief(format!(
            "the faber service: drained, stopped and started again, in group \
             {FABER_OPERATOR_GROUP}"
        )),
        (Some(_), true) => step(
            sec,
            Status::WillChange,
            subject,
            format!(
                "the running daemon is drained (control socket) and stopped, its {} \
                 swapped, then {start}; {ready}",
                if changed {
                    "definition"
                } else if swaps_binary {
                    "binary"
                } else {
                    "state"
                }
            ),
        )
        .doing(Action::InstallService { restart: true }),
        (Some(_), false) => step(
            sec,
            Status::WillChange,
            subject,
            format!("installed but not running: {start}; {ready}"),
        )
        .doing(Action::InstallService { restart: false }),
    });
}

/// The accounts whose LEGACY directories [`legacy_steps`] renames aside: the
/// operator's, and those of the account `--migrate-from` names.
fn legacy_movers(s: &HostState) -> Vec<&str> {
    let mut names: Vec<&str> = Vec::new();
    if let Ok(op) = &s.operator {
        names.push(op.name.as_str());
    }
    if let Some(from) = &s.opts.migrate_from {
        if !names.contains(&from.as_str()) {
            names.push(from.as_str());
        }
    }
    names
}

/// Whether [`legacy_steps`] renames any of `acct`'s directories aside: one of
/// [`legacy_movers`], no `--keep-legacy`, and a real directory it owns.
fn moves_aside(s: &HostState, acct: &LegacyAccount) -> bool {
    !s.opts.keep_legacy
        && legacy_movers(s).contains(&acct.name.as_str())
        && acct.dirs.iter().any(|d| d.owner == Some(acct.uid))
}

/// The LEGACY directories of the account that ran sudo, and of the one
/// `--migrate-from` names: renamed aside once the new daemon answers.
fn legacy_steps(s: &HostState, steps: &mut Vec<Step>) {
    let sec = Section::Legacy;
    let names = legacy_movers(s);
    for acct in s.legacy.iter().filter(|a| names.contains(&a.name.as_str())) {
        for dir in &acct.dirs {
            let subject = dir.path.display().to_string();
            steps.push(match dir.owner {
                Some(uid) if uid == acct.uid && s.opts.keep_legacy => {
                    step(sec, Status::Ok, subject, "kept where it is (--keep-legacy)")
                }
                Some(uid) if uid == acct.uid => step(
                    sec,
                    Status::WillChange,
                    subject,
                    "renamed aside to <name>.pre-system-<time> once the new daemon answers: the \
                     backup stays, and nothing faber reads stays in this home (--keep-legacy \
                     leaves it)",
                )
                .doing(Action::MoveLegacyAside {
                    account: acct.name.clone(),
                    uid: acct.uid,
                    path: dir.path.clone(),
                }),
                Some(uid) => step(
                    sec,
                    Status::Ok,
                    subject,
                    format!("left alone: owned by uid {uid}, not {}", acct.name),
                ),
                None => step(sec, Status::Ok, subject, "left alone: not a real directory"),
            });
        }
    }
}

/// The stored GitHub token, if there is one (ADR-0027). A file still in the
/// store's earlier form, owned by `_faber`, is given to root, so faber's own
/// account can no longer rewrite its token. A file in any other state is left
/// alone and only described: the daemon refuses it already, and `sudo faber
/// auth set` is what replaces it. A host with no store has no step.
fn credential_steps(s: &HostState, faber: Account, steps: &mut Vec<Step>) {
    use crate::credential::{ownership, Ownership};
    let sec = Section::Credential;
    for (path, facts) in &s.credential_files {
        let subject = path.display().to_string();
        steps.push(match ownership(path, *facts, fsx::ROOT.uid, faber) {
            Ownership::Current => step(sec, Status::Ok, subject, "root:_faber 0640"),
            Ownership::Legacy => step(
                sec,
                Status::WillChange,
                subject,
                "owned by _faber, so faber's own account could rewrite its token: give it \
                 to root:_faber, mode 0640",
            )
            .doing(Action::ReownCredential),
            Ownership::Refused(why) => step(
                sec,
                Status::Ok,
                subject,
                format!("left alone: {why}; faber will not use it until `sudo faber auth set`"),
            )
            .listed(),
        });
    }
}

fn keep_awake_step(s: &HostState, steps: &mut Vec<Step>) {
    if s.os != Os::MacOs {
        return;
    }
    let sec = Section::KeepAwake;
    steps.push(if !s.opts.keep_awake {
        step(
            sec,
            Status::Ok,
            "system sleep",
            "skipped (--skip-keep-awake)",
        )
    } else if s.keep_awake == Some(true) {
        step(sec, Status::Ok, "system sleep and Power Nap", "off")
    } else {
        step(
            sec,
            Status::WillChange,
            "system sleep and Power Nap",
            "pmset -a sleep 0; pmset -a powernap 0 (a sleeping Mac suspends every job)",
        )
        .named("sleep and Power Nap")
        .doing(Action::KeepAwake)
    });
}

/// The Linux VM's image (ADR-0026): built when it is missing or this release
/// pins another, skipped with a warning while the launcher is not installed.
/// macOS only: Linux hosts run no VMs.
fn linux_image_step(s: &HostState, steps: &mut Vec<Step>) {
    use super::linux_image::{plan_for, LinuxImagePlan, BASE_BUILD};
    // A launcher this run installs (`super::vm_runtime`) is there, and as
    // trusted, by the time the build runs last: the build need not wait for a
    // second run of setup.
    let mut probe = s.linux_image.clone();
    if steps
        .iter()
        .any(|st| st.action == Some(Action::Vm(super::vm_runtime::VmAction::InstallLauncher)))
    {
        probe.launcher = Ok(());
    }
    let Some(p) = plan_for(s.os, &probe) else {
        return;
    };
    let sec = Section::LinuxImage;
    let subject = format!("{}/linux/disk.raw", crate::platform::layout::IMAGE_STORE);
    steps.push(match p {
        LinuxImagePlan::Current => step(
            sec,
            Status::Ok,
            subject,
            format!("Debian {BASE_BUILD}, provisioned by this release"),
        ),
        LinuxImagePlan::Build { reason, fresh } => step(
            sec,
            if fresh {
                Status::WillCreate
            } else {
                Status::WillChange
            },
            subject,
            format!(
                "{reason}: download Debian 13 genericcloud arm64 {BASE_BUILD}, check its \
                 SHA-512 against the pin and Debian's SHA512SUMS, provision it in a throwaway \
                 VM as {FABER_VM_ACCOUNT} (Docker Engine, the vsock relay, Cloudflare's \
                 resolvers, no way in), and land it root:wheel 0444. Runs last, after the \
                 daemon answers; up to 30 minutes"
            ),
        )
        .doing(Action::BuildLinuxImage),
        LinuxImagePlan::Blocked { why } => step(
            sec,
            Status::Skipped,
            subject,
            format!("warning: {why}. Setup goes on; re-run it once the launcher is installed"),
        ),
    });
}

/// How many steps create, change, or refuse.
pub fn counts(steps: &[Step]) -> (usize, usize, usize) {
    let n = |want| steps.iter().filter(|s| s.status == want).count();
    (
        n(Status::WillCreate),
        n(Status::WillChange),
        n(Status::Refused),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::platform::legacy;
    use crate::platform::Env;
    use crate::setup::exe::PathFacts;

    /// The plan as a pipe would print it with `--verbose`, which holds every
    /// step's detail and command: for assertions and their messages.
    pub(crate) fn render(s: &HostState, steps: &[Step]) -> String {
        crate::setup::view::render(
            s,
            steps,
            crate::out::Style::PLAIN.verbose(true),
            crate::setup::view::End::Prompt,
        )
    }

    pub(crate) const SHA: &str = "0000000000000000000000000000000000000000000000000000000000000abc";

    fn faber_ids() -> Account {
        Account { uid: 451, gid: 451 }
    }

    fn vm_ids() -> Account {
        Account { uid: 452, gid: 452 }
    }

    /// The operator group's gid on a set-up host.
    const OPERATOR_GID: u32 = 453;

    fn ids() -> Ids {
        Ids {
            faber: faber_ids(),
            vm: vm_ids(),
            operator_gid: OPERATOR_GID,
        }
    }

    fn operator() -> OperatorInfo {
        OperatorInfo {
            name: "owner".into(),
            uid: 501,
            gid: 20,
            home: "/Users/owner".into(),
        }
    }

    fn trusted_chain() -> Vec<PathFacts> {
        vec![
            PathFacts {
                path: "/usr/local/bin/faber".into(),
                uid: 0,
                gid: 0,
                mode: 0o100_755,
            },
            PathFacts {
                path: "/usr/local/bin".into(),
                uid: 0,
                gid: 0,
                mode: 0o040_755,
            },
            PathFacts {
                path: "/".into(),
                uid: 0,
                gid: 0,
                mode: 0o040_755,
            },
        ]
    }

    /// `home`'s LEGACY paths on `os`, written out: `platform::legacy` answers
    /// for the OS the tests run on, and a fixture's plan must render the same on
    /// every host (its golden view is checked on macOS and Linux alike).
    fn legacy_paths(os: Os, home: &Path) -> Paths {
        let (config, state, logs, cache) = match os {
            Os::MacOs => (
                "Library/Application Support/faber",
                "Library/Application Support/faber",
                "Library/Logs/faber",
                "Library/Caches/faber",
            ),
            Os::Linux => (
                ".config/faber",
                ".local/state/faber",
                ".local/state/faber",
                ".cache/faber",
            ),
        };
        let mut p = legacy(&Env::with_home(home));
        p.config_path = Some(home.join(config).join("config.toml"));
        p.overrides_path = Some(home.join(config).join("overrides.json"));
        p.state_dir = Some(home.join(state));
        p.log_dir = Some(home.join(logs));
        p.cache_dir = Some(home.join(cache));
        p
    }

    /// `name`'s LEGACY files on `os`, with a config and an audit log.
    fn legacy_files(os: Os, home: &Path) -> Vec<(FileKind, PathBuf, SrcProbe)> {
        let paths = legacy_paths(os, home);
        FileKind::ALL
            .iter()
            .map(|k| {
                let probe = match k {
                    FileKind::Config => SrcProbe::Regular { len: 120 },
                    FileKind::Audit => SrcProbe::Regular { len: 4096 },
                    _ => SrcProbe::Missing,
                };
                (*k, k.source(&paths, None).unwrap(), probe)
            })
            .collect()
    }

    /// `home`'s LEGACY directories, as a per-user faber leaves them: real
    /// directories its account owns.
    fn legacy_home_dirs(os: Os, home: &Path, uid: u32) -> Vec<LegacyDir> {
        legacy_dirs(&legacy_paths(os, home))
            .into_iter()
            .map(|path| LegacyDir {
                path,
                owner: Some(uid),
            })
            .collect()
    }

    /// `/usr/local/bin` and up, root-owned 0755.
    fn root_cli_chain() -> Vec<PathFacts> {
        ["/usr/local/bin", "/usr/local", "/usr", "/"]
            .into_iter()
            .map(|p| PathFacts {
                path: p.into(),
                uid: 0,
                gid: 0,
                mode: 0o040_755,
            })
            .collect()
    }

    fn running_instance(home: &Path) -> OldInstance {
        OldInstance {
            definition: Some(home.join(format!("Library/LaunchAgents/{}.plist", service::LABEL))),
            definition_problem: None,
            loaded: Some(true),
            port: 8787,
            config_override: None,
        }
    }

    /// A macOS host that has never run setup: the operator runs a per-user
    /// faber with a config and an audit log.
    pub(crate) fn fresh_mac() -> HostState {
        let layout = SystemLayout::for_os(Os::MacOs);
        let op = operator();
        HostState {
            os: Os::MacOs,
            dirs: dir_specs(Os::MacOs, &layout)
                .into_iter()
                .map(|d| (d, DirProbe::Missing))
                .collect(),
            files: file_specs(Os::MacOs, &layout)
                .into_iter()
                .map(|f| (f, FileProbe::Missing))
                .collect(),
            layout,
            version: "0.4.0+gabc1234".into(),
            privileged: true,
            operator: Ok(op.clone()),
            exe_path: "/usr/local/bin/faber".into(),
            exe_sha256: Some(SHA.into()),
            exe_chain: Ok(trusted_chain()),
            cli_chain: root_cli_chain(),
            source_check: Some(Ok(())),
            ids: UsedIds {
                uids: [0, 1, 441, 480, 501].into_iter().collect(),
                gids: [0, 12, 20, 80, 441].into_iter().collect(),
            },
            faber_user: None,
            faber_group: None,
            vm_user: None,
            vm_group: None,
            operator_group: None,
            faber_groups: vec![],
            vm_groups: vec![],
            admins: vec!["root".into(), "owner".into()],
            docker_group: None,
            cli_bin: BinProbe::File {
                sha256: SHA.into(),
                uid: 0,
                gid: 0,
                mode: 0o100_755,
                version: Some("0.4.0+gabc1234".into()),
            },
            daemon_bin: BinProbe::Missing,
            legacy: vec![LegacyAccount {
                name: op.name.clone(),
                uid: op.uid,
                home: op.home.clone(),
                instance: Some(running_instance(&op.home)),
                files: legacy_files(Os::MacOs, &op.home),
                identity: vec![],
                dirs: legacy_home_dirs(Os::MacOs, &op.home, op.uid),
            }],
            dest_exists: vec![],
            record: RecordProbe::Absent,
            service: ServiceProbe {
                installed: None,
                loaded: false,
                stuck: None,
            },
            keep_awake: Some(false),
            credential_files: vec![],
            user_manager: false,
            vm_runtime: crate::setup::vm_runtime::VmRuntimeProbe::nothing(),
            // A host whose VM runtime is in place: the launcher installed and
            // the Linux image current, so the #223 guard has nothing to say.
            // Tests of the guard take them away.
            linux_image: crate::setup::linux_image::LinuxImageProbe {
                launcher: Ok(()),
                installed: crate::setup::linux_image::Installed::Found(current_image()),
            },
            app: crate::setup::app::AppProbe::nothing(),
            opts: PlanOpts {
                remigrate: false,
                replace_daemon: false,
                keep_awake: true,
                migrate_from: None,
                fresh: false,
                keep_legacy: false,
                vm: Default::default(),
                before_vm_runtime: false,
            },
        }
    }

    /// The marker of a Linux image this release built.
    pub(crate) fn current_image() -> crate::setup::linux_image::Marker {
        crate::setup::linux_image::Marker {
            schema: 1,
            base_build: crate::setup::linux_image::BASE_BUILD.into(),
            base_sha512: crate::setup::linux_image::BASE_SHA512.into(),
            provision_digest: crate::setup::linux_image::PROVISION_DIGEST.clone(),
            built_at_unix: 1,
            docker_version: "29.0.1".into(),
        }
    }

    /// No launcher anywhere, and so no Linux image either: a binary from
    /// before the VM runtime (#223).
    fn without_vm_runtime(s: &mut HostState) {
        s.linux_image.launcher = Err("the VM launcher is not installed yet".into());
        s.linux_image.installed = crate::setup::linux_image::Installed::Absent;
    }

    /// #223: a macOS setup that would leave no runnable VM runtime is refused
    /// before anything changes, says why and what to do instead, and goes on
    /// only with `--before-vm-runtime`, saying what stops working.
    #[test]
    fn a_host_left_without_a_vm_runtime_is_refused() {
        let mut s = fresh_mac();
        without_vm_runtime(&mut s);
        let steps = plan(&s);
        let gaps = vm_runtime_gaps(&s, &steps);
        assert_eq!(gaps.len(), 2, "{gaps:?}");
        assert!(gaps[0].contains("no VM launcher"), "{}", gaps[0]);
        assert!(gaps[1].contains("Linux VM's image"), "{}", gaps[1]);
        let (_, _, refused) = counts(&steps);
        assert_eq!(refused, 1, "{}", render(&s, &steps));
        let guard = steps.iter().find(|st| st.subject == "VM runtime").unwrap();
        assert_eq!(guard.status, Status::Refused);
        assert_eq!(guard.section, Section::Preflight);
        assert!(
            guard.detail.contains("docs/manual-update.md"),
            "{}",
            guard.detail
        );
        assert!(guard.detail.contains("--before-vm-runtime"));
        // Shown with the preflight, before any change.
        let at = steps
            .iter()
            .position(|st| st.subject == "VM runtime")
            .unwrap();
        assert!(steps[..at]
            .iter()
            .all(|st| st.section == Section::Preflight));
        assert!(steps[at + 1].section != Section::Preflight);

        s.opts.before_vm_runtime = true;
        let steps = plan(&s);
        assert_eq!(counts(&steps).2, 0, "{}", render(&s, &steps));
        let guard = steps.iter().find(|st| st.subject == "VM runtime").unwrap();
        assert_eq!(guard.status, Status::Skipped);
        assert!(guard.detail.starts_with("warning: --before-vm-runtime"));
        assert!(guard.detail.contains("runs NO jobs"), "{}", guard.detail);
    }

    /// Runnable is what setup puts in place: a launcher this run installs
    /// counts, and so does a Linux image this run builds. Linux hosts run no
    /// VMs and are never refused.
    #[test]
    fn a_vm_runtime_this_run_installs_is_runnable() {
        use crate::setup::linux_image::Installed;
        // Installed launcher, image not built yet: this run builds it.
        let mut s = set_up_mac();
        s.linux_image.installed = Installed::Absent;
        let steps = plan(&s);
        assert!(
            vm_runtime_gaps(&s, &steps).is_empty(),
            "{}",
            render(&s, &steps)
        );
        // No launcher installed, but one beside the binary: installed by this
        // run, which then builds the image.
        let mut s = fresh_mac();
        without_vm_runtime(&mut s);
        s.vm_runtime.launcher_source =
            crate::setup::vm_runtime::tests::installed(&s.layout).launcher_source;
        let steps = plan(&s);
        assert!(steps.iter().any(|st| st.action
            == Some(Action::Vm(
                crate::setup::vm_runtime::VmAction::InstallLauncher
            ))));
        assert!(
            vm_runtime_gaps(&s, &steps).is_empty(),
            "{}",
            render(&s, &steps)
        );
        // A launcher but an image that cannot be built is still a gap: here,
        // an installed launcher that is not trusted.
        let mut s = set_up_mac();
        s.linux_image.installed = Installed::Absent;
        s.linux_image.launcher = Err("its directory is writable by _faber".into());
        let gaps = vm_runtime_gaps(&s, &plan(&s));
        assert_eq!(gaps.len(), 2, "{gaps:?}");
        // Linux: nothing to check.
        let mut s = fresh_mac();
        without_vm_runtime(&mut s);
        s.os = Os::Linux;
        assert!(vm_runtime_gaps(&s, &plan(&s)).is_empty());
    }

    /// #228: inside `_faber`'s state directory a root-owned directory is
    /// refused at plan time, as apply refuses it: `_faber` could have moved
    /// one of root's there. Outside it, root's own is still taken over.
    #[test]
    fn a_root_owned_directory_in_the_state_directory_is_refused() {
        let mut s = set_up_mac();
        let at = s.dirs.iter().position(|(d, _)| d.in_state_dir).unwrap();
        s.dirs[at].1 = DirProbe::Dir {
            uid: 0,
            gid: 0,
            mode: 0o040_755,
        };
        let subject = s.dirs[at].0.path.display().to_string();
        let steps = plan(&s);
        let st = steps.iter().find(|st| st.subject == subject).unwrap();
        assert_eq!(st.status, Status::Refused, "{}", st.detail);
        let top = s
            .dirs
            .iter()
            .position(|(d, _)| !d.in_state_dir && d.owner == Owner::FaberOperatorGroup)
            .unwrap();
        s.dirs[top].1 = DirProbe::Dir {
            uid: 0,
            gid: 0,
            mode: 0o040_755,
        };
        let subject = s.dirs[top].0.path.display().to_string();
        let steps = plan(&s);
        let st = steps.iter().find(|st| st.subject == subject).unwrap();
        assert_eq!(st.status, Status::WillChange, "{}", st.detail);
    }

    fn role_user(name: &str, id: Account, home: &Path, shell: &str) -> UserInfo {
        UserInfo {
            name: name.into(),
            uid: id.uid,
            gid: id.gid,
            home: home.to_path_buf(),
            shell: shell.into(),
            hidden: true,
            password: Some("*".into()),
            auth_authority: false,
        }
    }

    /// `fresh_mac()` moved to Linux: nothing of faber's there yet, and a
    /// `docker` group `_faber` is not in.
    pub(crate) fn fresh_linux() -> HostState {
        let mut s = fresh_mac();
        s.os = Os::Linux;
        s.layout = SystemLayout::for_os(Os::Linux);
        for a in &mut s.legacy {
            a.files = legacy_files(Os::Linux, &a.home);
            a.dirs = legacy_home_dirs(Os::Linux, &a.home, a.uid);
        }
        s.dirs = dir_specs(Os::Linux, &s.layout)
            .into_iter()
            .map(|d| (d, DirProbe::Missing))
            .collect();
        s.files = Vec::new();
        s.admins.clear();
        s.user_manager = true;
        s.docker_group = Some(GroupInfo {
            name: "docker".into(),
            gid: 998,
            members: vec![],
        });
        s
    }

    /// ADR-0050's refused example: `fresh_mac()` with a `_faber` user and
    /// group that already exist, the user able to authenticate.
    pub(crate) fn with_an_authenticating_faber(s: &mut HostState) {
        let ids = faber_ids();
        s.faber_group = Some(GroupInfo {
            name: FABER_ACCOUNT.into(),
            gid: ids.gid,
            members: vec![],
        });
        let home = accounts::home(FABER_ACCOUNT, &s.layout);
        s.faber_user = Some(UserInfo {
            auth_authority: true,
            ..role_user(FABER_ACCOUNT, ids, &home, "/usr/bin/false")
        });
    }

    /// The same host after a successful setup: accounts, directories, log
    /// files, binaries, copies, record and service all in place, old plist
    /// moved aside. The operator is in the operator group with `_faber`, and
    /// `_faber`'s group has nobody.
    pub(crate) fn set_up_mac() -> HostState {
        let mut s = fresh_mac();
        let layout = s.layout.clone();
        s.faber_group = Some(GroupInfo {
            name: FABER_ACCOUNT.into(),
            gid: 451,
            members: vec![],
        });
        s.vm_group = Some(GroupInfo {
            name: FABER_VM_ACCOUNT.into(),
            gid: 452,
            members: vec![],
        });
        s.operator_group = Some(GroupInfo {
            name: FABER_OPERATOR_GROUP.into(),
            gid: OPERATOR_GID,
            members: vec!["owner".into(), FABER_ACCOUNT.into()],
        });
        s.faber_user = Some(role_user(
            FABER_ACCOUNT,
            faber_ids(),
            &layout.state_dir,
            "/usr/bin/false",
        ));
        s.vm_user = Some(role_user(
            FABER_VM_ACCOUNT,
            vm_ids(),
            &layout.vm_home,
            "/usr/bin/false",
        ));
        s.ids.uids.extend([451, 452]);
        s.ids.gids.extend([451, 452, OPERATOR_GID]);
        s.faber_groups = vec![451, 12, 61, OPERATOR_GID];
        s.vm_groups = vec![452, 12, 61];
        s.dirs = dir_specs(Os::MacOs, &layout)
            .into_iter()
            .map(|d| {
                let Account { uid, gid } = owner_account(d.owner, ids());
                let mode = 0o040_000 | d.mode;
                (d, DirProbe::Dir { uid, gid, mode })
            })
            .collect();
        s.dest_exists = vec![FileKind::Config, FileKind::Audit];
        s.files = files_as_set_up(Os::MacOs, &layout, &s.dest_exists);
        s.daemon_bin = BinProbe::File {
            sha256: SHA.into(),
            uid: 451,
            gid: OPERATOR_GID,
            mode: 0o100_755,
            version: Some("0.4.0+gabc1234".into()),
        };
        s.legacy[0].instance = None;
        s.legacy[0].dirs.clear();
        let op = operator();
        let migrated = Some(MigratedFrom {
            user: "owner".into(),
            files: vec![],
        });
        s.record = RecordProbe::Trusted(record_for(&s, &op, ids(), migrated));
        s.service = ServiceProbe {
            installed: Some(service::render(Os::MacOs, &layout)),
            loaded: true,
            stuck: None,
        };
        s.keep_awake = Some(true);
        s.vm_runtime = crate::setup::vm_runtime::tests::installed(&layout);
        s.app = crate::setup::app::tests::installed();
        s
    }

    /// [`file_specs`] as setup leaves them: each log file there, and each
    /// state file that `existing` names, all the operator group's.
    fn files_as_set_up(
        os: Os,
        layout: &SystemLayout,
        existing: &[FileKind],
    ) -> Vec<(FileSpec, FileProbe)> {
        file_specs(os, layout)
            .into_iter()
            .map(|f| {
                let there = f.create || existing.iter().any(|k| k.dest(layout) == f.path);
                let Account { uid, gid } = owner_account(f.owner, ids());
                let probe = if there {
                    FileProbe::File {
                        uid,
                        gid,
                        mode: 0o100_000 | f.mode,
                    }
                } else {
                    FileProbe::Missing
                };
                (f, probe)
            })
            .collect()
    }

    fn with_action(steps: &[Step]) -> Vec<&Step> {
        steps.iter().filter(|s| s.action.is_some()).collect()
    }

    fn migrated(steps: &[Step]) -> Vec<&MigrationItem> {
        steps
            .iter()
            .filter_map(|st| match &st.action {
                Some(Action::Migrate(item)) => Some(item),
                _ => None,
            })
            .collect()
    }

    fn daemon_step(steps: &[Step]) -> &Step {
        steps
            .iter()
            .find(|st| {
                st.section == Section::Binaries && st.subject.contains("Application Support")
            })
            .expect("a daemon copy step")
    }

    /// ADR-0027: a token file still owned by `_faber` is re-owned by root; one
    /// already root's is left as it is, and so is one in any other state.
    #[test]
    fn a_legacy_token_file_is_given_to_root() {
        use crate::credential::{EntryKind, FileFacts};
        let mut host = fresh_mac();
        let (faber, _) = planned_ids(&host).unwrap();
        let file = |uid: u32, gid: u32, mode: u32| FileFacts {
            kind: EntryKind::Regular,
            uid,
            gid,
            mode: 0o100_000 | mode,
        };
        let path = PathBuf::from("/etc/faber/credentials/github-owner.json");
        let cred = |host: &HostState| -> Vec<Step> {
            plan(host)
                .into_iter()
                .filter(|st| st.section == Section::Credential)
                .collect()
        };
        assert!(cred(&host).is_empty(), "no store, no step");

        host.credential_files = vec![(path.clone(), file(faber.uid, faber.gid, 0o600))];
        let steps = cred(&host);
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].status, Status::WillChange);
        assert_eq!(steps[0].action, Some(Action::ReownCredential));

        host.credential_files = vec![(path.clone(), file(0, faber.gid, 0o640))];
        let steps = cred(&host);
        assert_eq!(
            (steps[0].status, steps[0].action.clone()),
            (Status::Ok, None)
        );

        host.credential_files = vec![(path, file(0, faber.gid, 0o644))];
        let steps = cred(&host);
        assert_eq!(
            (steps[0].status, steps[0].action.clone()),
            (Status::Ok, None)
        );
        assert!(
            steps[0].detail.starts_with("left alone"),
            "{}",
            steps[0].detail
        );
    }

    #[test]
    fn a_fresh_host_plans_every_part_of_setup() {
        let s = fresh_mac();
        let steps = plan(&s);
        let (_, _, refused) = counts(&steps);
        assert_eq!(refused, 0, "{}", render(&s, &steps));

        let cmds: Vec<String> = steps
            .iter()
            .filter_map(|st| match &st.action {
                Some(Action::Run(cmd)) => Some(cmd.display()),
                _ => None,
            })
            .collect();
        // 480 is taken on this host; 450 and 451 are the lowest free ids.
        assert!(
            cmds[0].ends_with("-o create -i 450 -r \"faber daemon\" _faber"),
            "{cmds:?}"
        );
        assert!(
            cmds[1].ends_with("-o create -i 451 -r \"faber VM host\" _faber_vm"),
            "{cmds:?}"
        );
        assert!(
            cmds[3].contains("-addUser _faber -UID 450 -GID 450"),
            "{cmds:?}"
        );
        assert!(cmds
            .iter()
            .any(|c| c.contains("-addUser _faber_vm -UID 451 -GID 451")));
        assert!(
            cmds[2].ends_with("-o create -i 452 -r \"faber operator\" _faber_operator"),
            "{cmds:?}"
        );
        // S1: the operator joins the operator group, as does `_faber`, and
        // nobody joins `_faber`'s, which reads the token.
        let joins = &cmds[cmds.len() - 2..];
        assert!(
            joins[0].ends_with("-o edit -a owner -t user _faber_operator"),
            "{cmds:?}"
        );
        assert!(
            joins[1].ends_with("-o edit -a _faber -t user _faber_operator"),
            "{cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| c.ends_with("-t user _faber")),
            "nobody into _faber: {cmds:?}"
        );
        assert!(
            !cmds.iter().any(|c| c.contains("-a _faber_vm")),
            "never into _faber"
        );

        let actions: Vec<&Action> = steps.iter().filter_map(|s| s.action.as_ref()).collect();
        assert_eq!(
            actions
                .iter()
                .filter(|a| matches!(a, Action::EnsureDir(_)))
                .count(),
            7
        );
        assert!(
            !actions.contains(&&Action::InstallCli),
            "the CLI copy is this binary"
        );
        assert!(actions.contains(&&Action::InstallDaemon));
        assert!(actions.contains(&&Action::StopOld {
            account: "owner".into()
        }));
        let kinds: Vec<FileKind> = migrated(&steps).iter().map(|i| i.kind).collect();
        assert_eq!(kinds, vec![FileKind::Config, FileKind::Audit]);
        assert!(
            !actions.contains(&&Action::SeedConfig),
            "there is a config to migrate"
        );
        assert!(actions.contains(&&Action::WriteRecord));
        assert!(actions.contains(&&Action::InstallService { restart: false }));
        assert!(actions.contains(&&Action::KeepAwake));
    }

    /// `set_up_mac()` as a setup from before the operator group left it: the
    /// operator in `_faber`'s group, no operator group, the state and log trees
    /// and their files in `_faber`'s group, a token `_faber`'s group reads, and
    /// a record that names no operator group.
    pub(crate) fn set_up_before_the_operator_group() -> HostState {
        let mut s = set_up_mac();
        // What setup gave them then: `_faber`'s group where the operator
        // group is now.
        let then = Ids {
            operator_gid: faber_ids().gid,
            ..ids()
        };
        s.faber_group.as_mut().unwrap().members = vec!["owner".into()];
        s.operator_group = None;
        s.ids.gids.remove(&OPERATOR_GID);
        s.faber_groups = vec![451, 12, 61];
        for (d, probe) in &mut s.dirs {
            if let DirProbe::Dir { uid, gid, .. } = probe {
                (*uid, *gid) = {
                    let a = owner_account(d.owner, then);
                    (a.uid, a.gid)
                };
            }
        }
        for (f, probe) in &mut s.files {
            if let FileProbe::File { uid, gid, .. } = probe {
                (*uid, *gid) = {
                    let a = owner_account(f.owner, then);
                    (a.uid, a.gid)
                };
            }
        }
        if let BinProbe::File { gid, .. } = &mut s.daemon_bin {
            *gid = faber_ids().gid;
        }
        if let RecordProbe::Trusted(r) = &mut s.record {
            r.accounts.operator_group = None;
            r.artifacts
                .retain(|a| !matches!(a, Artifact::Group { name } if name == FABER_OPERATOR_GROUP));
        }
        s.credential_files = vec![(
            PathBuf::from("/etc/faber/credentials/github-owner.json"),
            crate::credential::FileFacts {
                kind: crate::credential::EntryKind::Regular,
                uid: 0,
                gid: faber_ids().gid,
                mode: 0o100_640,
            },
        )];
        s
    }

    /// S1: a host set up before the operator group gets one, the operator
    /// and `_faber` join it, and the operator leaves `_faber`'s group, which
    /// reads the token. The trees and the files the operator reads are given
    /// the new group, the record names it, and the daemon is restarted so it
    /// starts in it. The token keeps `_faber`'s group.
    #[test]
    fn a_host_set_up_before_the_operator_group_moves_the_operator_out_of_faber() {
        let s = set_up_before_the_operator_group();
        let steps = plan(&s);
        assert_eq!(counts(&steps).2, 0, "{}", render(&s, &steps));
        let cmds: Vec<String> = steps
            .iter()
            .filter_map(|st| match &st.action {
                Some(Action::Run(cmd)) => Some(cmd.display()),
                _ => None,
            })
            .collect();
        assert_eq!(
            cmds,
            vec![
                // 450 is the lowest gid free beside `_faber`'s 451 and
                // `_faber_vm`'s 452.
                "/usr/sbin/dseditgroup -o create -i 450 -r \"faber operator\" _faber_operator",
                "/usr/sbin/dseditgroup -o edit -a owner -t user _faber_operator",
                "/usr/sbin/dseditgroup -o edit -a _faber -t user _faber_operator",
                "/usr/sbin/dseditgroup -o edit -d owner -t user _faber",
            ],
            "{}",
            render(&s, &steps)
        );
        assert!(leaves_faber(&s, &steps));

        let changed = |path: &Path| {
            steps
                .iter()
                .find(|st| match &st.action {
                    Some(Action::EnsureDir(d)) => d.path == path,
                    Some(Action::EnsureFile(f)) => f.path == path,
                    _ => false,
                })
                .map(|st| (st.status, st.action.clone()))
        };
        let l = &s.layout;
        for dir in [
            &l.state_dir,
            &l.bin_dir,
            &l.registry_auth_dir,
            &l.log_dir,
            &l.cache_dir,
        ] {
            let (status, action) = changed(dir).unwrap();
            assert_eq!(status, Status::WillChange, "{}", dir.display());
            let Some(Action::EnsureDir(spec)) = action else {
                panic!("{}", dir.display());
            };
            assert_eq!(spec.after_stop, *dir == l.log_dir, "{}", dir.display());
        }
        for file in ["stdout.log", "stderr.log", "audit.jsonl"] {
            assert_eq!(
                changed(&l.log_dir.join(file)).unwrap().0,
                Status::WillChange
            );
        }
        let config = steps
            .iter()
            .find(
                |st| matches!(&st.action, Some(Action::EnsureFile(f)) if f.path == l.config_path()),
            )
            .unwrap();
        assert_eq!(config.section, Section::Migration);
        // One item with the migration's "kept", whose key it shares.
        assert_eq!(
            config.shown.item,
            Some(l.config_path().display().to_string())
        );
        assert_eq!(config.status, Status::WillChange, "{}", config.detail);
        assert_eq!(
            config.brief_text(),
            "given group _faber_operator, which you read it through"
        );
        // A daemon copy in `_faber`'s group is still `_faber`'s 0755.
        assert!(!steps
            .iter()
            .any(|st| st.action == Some(Action::InstallDaemon)));
        let token = steps
            .iter()
            .find(|st| st.section == Section::Credential)
            .unwrap();
        assert_eq!(
            (token.status, &token.detail[..]),
            (Status::Ok, "root:_faber 0640")
        );

        let record = steps
            .iter()
            .find(|st| st.section == Section::Record)
            .unwrap();
        assert_eq!(record.action, Some(Action::WriteRecord));
        let want = record_for(&s, &operator(), planned(&s).unwrap(), None);
        assert_eq!(want.operator_gid(), Some(450));
        assert!(want.artifacts.contains(&Artifact::Group {
            name: FABER_OPERATOR_GROUP.into()
        }));
        let service = steps
            .iter()
            .find(|st| st.section == Section::Service)
            .unwrap();
        assert_eq!(
            service.action,
            Some(Action::InstallService { restart: true })
        );
        assert!(
            service.detail.contains("starts in group _faber_operator"),
            "{}",
            service.detail
        );

        let glance: Vec<String> = Section::Accounts
            .glance(
                &s,
                &steps
                    .iter()
                    .filter(|st| st.section == Section::Accounts)
                    .collect::<Vec<_>>(),
            )
            .into_iter()
            .map(|(_, text)| text)
            .collect();
        assert!(
            glance.contains(&"you leave group _faber, so only faber reads its GitHub token".into()),
            "{glance:?}"
        );
        assert!(glance
            .iter()
            .any(|t| t.starts_with("you join group _faber_operator")));
    }

    /// Anyone but the operator in `_faber`'s group is refused, and named with
    /// the command that removes them; setup does not guess who they are.
    #[test]
    fn a_stranger_in_the_faber_group_is_refused() {
        let mut s = set_up_mac();
        s.faber_group.as_mut().unwrap().members = vec!["ci".into()];
        let steps = plan(&s);
        let refused: Vec<&Step> = steps
            .iter()
            .filter(|st| st.status == Status::Refused)
            .collect();
        assert_eq!(refused.len(), 1, "{}", render(&s, &steps));
        assert_eq!(refused[0].subject, "group _faber");
        assert!(
            refused[0]
                .detail
                .contains("`sudo /usr/sbin/dseditgroup -o edit -d ci -t user _faber`"),
            "{}",
            refused[0].detail
        );
        // `_faber_vm` in the operator group is refused too.
        let mut s = set_up_mac();
        s.operator_group
            .as_mut()
            .unwrap()
            .members
            .push(FABER_VM_ACCOUNT.into());
        let refused: Vec<Step> = plan(&s)
            .into_iter()
            .filter(|st| st.status == Status::Refused)
            .collect();
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].subject, "group _faber_operator");
    }

    /// The trees `_faber` keeps for the operator have the operator group on
    /// both OSes, the macOS log directory root's with it; `_faber_vm`'s home
    /// keeps `_faber`'s group, and so does the token, which no spec names.
    #[test]
    fn the_state_and_log_trees_have_the_operator_group() {
        for os in [Os::MacOs, Os::Linux] {
            let l = SystemLayout::for_os(os);
            for d in dir_specs(os, &l) {
                let want = if d.path == l.etc_dir {
                    Owner::Root
                } else if d.path == l.vm_home {
                    if os == Os::MacOs {
                        Owner::FaberVmFaberGroup
                    } else {
                        Owner::FaberVm
                    }
                } else if d.path == l.log_dir && os == Os::MacOs {
                    Owner::RootOperatorGroup
                } else {
                    Owner::FaberOperatorGroup
                };
                assert_eq!(d.owner, want, "{os:?} {}", d.path.display());
            }
            let files = file_specs(os, &l);
            assert!(files
                .iter()
                .all(|f| f.owner == Owner::FaberOperatorGroup && f.mode == 0o640));
            for kind in FileKind::ALL {
                let f = files.iter().find(|f| f.path == kind.dest(&l)).unwrap();
                // Only the log files are created; a state file in a directory
                // `_faber` can write is taken over only if it is `_faber`'s.
                assert_eq!(f.create, f.path.starts_with(&l.log_dir) && os == Os::MacOs);
                assert_eq!(f.in_faber_dir, !f.create, "{}", f.path.display());
            }
            assert!(!files.iter().any(|f| f.path.starts_with(&l.credentials_dir)));
        }
        let state = owner_account(Owner::FaberOperatorGroup, ids());
        assert_eq!((state.uid, state.gid), (451, OPERATOR_GID));
    }

    /// In a directory `_faber` can write, a state file that is not `_faber`'s
    /// is refused rather than taken over: `_faber` could have linked one of
    /// root's in, and root would give it away.
    #[test]
    fn a_root_owned_state_file_is_refused_not_taken_over() {
        let mut s = set_up_mac();
        let config = s.layout.config_path();
        let at = s.files.iter().position(|(f, _)| f.path == config).unwrap();
        s.files[at].1 = FileProbe::File {
            uid: 0,
            gid: 0,
            mode: 0o100_640,
        };
        let steps = plan(&s);
        let st = steps
            .iter()
            .find(|st| st.subject == config.display().to_string() && st.status == Status::Refused)
            .unwrap_or_else(|| panic!("{}", render(&s, &steps)));
        assert!(st.detail.contains("owned by uid 0"), "{}", st.detail);
        // A missing state file is no step at all: the daemon makes it.
        let overrides = s.layout.overrides_path().display().to_string();
        assert!(!steps.iter().any(|st| st.subject == overrides));
    }

    /// Root never runs a binary the operator group can write: `_faber` is in
    /// it, planned or existing.
    #[test]
    fn the_operator_group_is_untrusted_for_root() {
        let fresh = fresh_mac();
        let (faber, vm) = planned_ids(&fresh).unwrap();
        let u = untrusted(&fresh, Some((faber, vm)));
        assert!(
            u.gids.contains(&planned(&fresh).unwrap().operator_gid),
            "{u:?}"
        );
        let set_up = set_up_mac();
        assert!(untrusted(&set_up, None).gids.contains(&OPERATOR_GID));
    }

    /// Linux: the unit's `Group=` changes, so the daemon is restarted, and
    /// the audit log, in `_faber`'s own log directory, is regrouped only if it
    /// is `_faber`'s.
    #[test]
    fn a_linux_host_set_up_before_the_operator_group_restarts_in_it() {
        let mut s = set_up_before_the_operator_group();
        s.os = Os::Linux;
        s.layout = SystemLayout::for_os(Os::Linux);
        let layout = s.layout.clone();
        s.admins.clear();
        for u in [&mut s.faber_user, &mut s.vm_user].into_iter().flatten() {
            u.shell = "/usr/sbin/nologin".into();
            u.home = accounts::home(&u.name, &layout);
            u.password = Some("!".into());
        }
        let then = Ids {
            operator_gid: faber_ids().gid,
            ..ids()
        };
        s.dirs = dir_specs(Os::Linux, &layout)
            .into_iter()
            .map(|d| {
                let Account { uid, gid } = owner_account(d.owner, then);
                let mode = d.mode;
                (d, DirProbe::Dir { uid, gid, mode })
            })
            .collect();
        s.files = files_as_set_up(Os::Linux, &layout, &s.dest_exists)
            .into_iter()
            .map(|(f, probe)| match probe {
                FileProbe::File { mode, .. } => (
                    f,
                    FileProbe::File {
                        uid: 451,
                        gid: 451,
                        mode,
                    },
                ),
                other => (f, other),
            })
            .collect();
        s.docker_group = Some(GroupInfo {
            name: "docker".into(),
            gid: 998,
            members: vec![FABER_ACCOUNT.into()],
        });
        s.service.installed = Some(
            service::render(Os::Linux, &layout)
                .replace("Group=_faber_operator", "Group=_faber")
                .replace("SupplementaryGroups=_faber\n", ""),
        );
        let steps = plan(&s);
        assert_eq!(counts(&steps).2, 0, "{}", render(&s, &steps));
        let audit = layout.log_dir.join("audit.jsonl");
        let spec = steps
            .iter()
            .find_map(|st| match &st.action {
                Some(Action::EnsureFile(f)) if f.path == audit => Some(f),
                _ => None,
            })
            .unwrap_or_else(|| panic!("{}", render(&s, &steps)));
        assert!(spec.in_faber_dir && !spec.create);
        assert!(
            steps
                .iter()
                .any(|st| st.detail.starts_with("/usr/bin/gpasswd -d owner _faber: ")),
            "{}",
            render(&s, &steps)
        );
        let service = steps
            .iter()
            .find(|st| st.section == Section::Service)
            .unwrap();
        assert_eq!(
            service.action,
            Some(Action::InstallService { restart: true })
        );
        assert!(service.detail.contains("definition"), "{}", service.detail);
    }

    /// The idempotency guarantee: a set-up host plans only `ok`, with no
    /// action at all, so a re-run changes and restarts nothing.
    #[test]
    fn a_set_up_host_plans_nothing() {
        let s = set_up_mac();
        let steps = plan(&s);
        assert_eq!(counts(&steps), (0, 0, 0), "{}", render(&s, &steps));
        assert!(with_action(&steps).is_empty(), "{}", render(&s, &steps));
        assert!(render(&s, &steps).contains("nothing to change"));
    }

    /// The Linux VM image: skipped with a warning while the launcher is not
    /// installed (the fixtures' state), built once it is, and nothing at all on
    /// Linux.
    #[test]
    fn the_linux_image_is_skipped_built_or_left_alone() {
        use crate::setup::linux_image::{Installed, Marker, PROVISION_DIGEST};
        let image = |steps: &[Step]| {
            steps
                .iter()
                .find(|st| st.section == Section::LinuxImage)
                .cloned()
        };
        let mut s = set_up_mac();
        without_vm_runtime(&mut s);
        let st = image(&plan(&s)).expect("a step on macOS");
        assert_eq!(st.status, Status::Skipped);
        assert!(st.action.is_none());
        assert!(st.detail.starts_with("warning: not built"), "{}", st.detail);
        assert!(render(&s, &plan(&s)).contains("! warning"));

        s.linux_image.launcher = Ok(());
        let steps = plan(&s);
        let st = image(&steps).unwrap();
        assert_eq!(st.status, Status::WillCreate);
        assert_eq!(st.action, Some(Action::BuildLinuxImage));
        assert_eq!(counts(&steps), (1, 0, 0));
        // The build alone restarts nothing.
        assert!(!steps
            .iter()
            .any(|st| matches!(st.action, Some(Action::InstallService { .. }))));

        let current = Marker {
            schema: 1,
            base_build: crate::setup::linux_image::BASE_BUILD.into(),
            base_sha512: crate::setup::linux_image::BASE_SHA512.into(),
            provision_digest: PROVISION_DIGEST.clone(),
            built_at_unix: 1,
            docker_version: "29.0.1".into(),
        };
        s.linux_image.installed = Installed::Found(current.clone());
        let st = image(&plan(&s)).unwrap();
        assert_eq!(st.status, Status::Ok);
        assert_eq!(counts(&plan(&s)), (0, 0, 0));

        let mut stale = current;
        stale.base_sha512 = "f".repeat(128);
        s.linux_image.installed = Installed::Found(stale);
        assert_eq!(image(&plan(&s)).unwrap().status, Status::WillChange);

        s.os = Os::Linux;
        assert!(image(&plan(&s)).is_none());
    }

    #[test]
    fn a_set_up_linux_host_plans_nothing_either() {
        let mut s = set_up_mac();
        s.os = Os::Linux;
        s.layout = SystemLayout::for_os(Os::Linux);
        let layout = s.layout.clone();
        s.admins.clear();
        for u in [&mut s.faber_user, &mut s.vm_user].into_iter().flatten() {
            u.shell = "/usr/sbin/nologin".into();
            u.home = accounts::home(&u.name, &layout);
            u.password = Some("!".into());
        }
        s.dirs = dir_specs(Os::Linux, &layout)
            .into_iter()
            .map(|d| {
                let Account { uid, gid } = owner_account(d.owner, ids());
                let mode = d.mode;
                (d, DirProbe::Dir { uid, gid, mode })
            })
            .collect();
        s.files = files_as_set_up(Os::Linux, &layout, &s.dest_exists);
        s.docker_group = Some(GroupInfo {
            name: "docker".into(),
            gid: 998,
            members: vec![FABER_ACCOUNT.into()],
        });
        s.service.installed = Some(service::render(Os::Linux, &layout));
        let op = operator();
        let prev = match &s.record {
            RecordProbe::Trusted(r) => r.migrated_from.clone(),
            _ => None,
        };
        s.record = RecordProbe::Absent;
        s.record = RecordProbe::Trusted(record_for(&s, &op, ids(), prev));
        let steps = plan(&s);
        assert_eq!(counts(&steps), (0, 0, 0), "{}", render(&s, &steps));
        assert!(with_action(&steps).is_empty());
        assert!(!steps.iter().any(|st| st.section == Section::KeepAwake));
    }

    #[test]
    fn linux_plans_useradd_and_the_root_equivalent_docker_grant() {
        let s = fresh_linux();
        let steps = plan(&s);
        let shown = render(&s, &steps);
        assert!(
            shown.contains("/usr/sbin/groupadd --system -g 450 _faber"),
            "{shown}"
        );
        assert!(shown.contains("/usr/sbin/useradd --system --no-create-home -u 450 -g 450"));
        assert!(shown.contains("/usr/sbin/usermod -aG docker _faber"));
        assert!(shown.contains("root-equivalent"));
        assert!(!shown.contains("sysadminctl"));
        assert!(!shown.contains("IsHidden"));
        assert!(!steps
            .iter()
            .any(|st| matches!(st.action, Some(Action::LockPassword { .. }))));
        assert!(shown.contains("systemctl enable --now faber.service"));
    }

    /// Fix for a review finding: a new macOS role account is left with no
    /// password at all, and an existing one that has any is refused.
    #[test]
    fn role_accounts_never_keep_a_password() {
        let s = fresh_mac();
        let steps = plan(&s);
        let locks: Vec<&str> = steps
            .iter()
            .filter_map(|st| match &st.action {
                Some(Action::LockPassword { name }) => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(locks, vec![FABER_ACCOUNT, FABER_VM_ACCOUNT]);

        let mut s = set_up_mac();
        if let Some(u) = &mut s.faber_user {
            u.password = Some("********".into());
            u.auth_authority = true;
        }
        let steps = plan(&s);
        let refused: Vec<&Step> = steps
            .iter()
            .filter(|st| st.status == Status::Refused)
            .collect();
        assert_eq!(refused.len(), 1, "{}", render(&s, &steps));
        assert_eq!(refused[0].subject, "user _faber");
        assert!(refused[0].detail.contains("AuthenticationAuthority"));
    }

    #[test]
    fn a_changed_binary_on_a_set_up_host_drains_and_restarts() {
        let mut s = set_up_mac();
        s.exe_sha256 = Some("f".repeat(64));
        s.cli_bin = BinProbe::File {
            sha256: "f".repeat(64),
            uid: 0,
            gid: 0,
            mode: 0o100_755,
            version: None,
        };
        s.daemon_bin = BinProbe::File {
            sha256: SHA.into(),
            uid: 451,
            gid: 451,
            mode: 0o100_755,
            version: Some("0.3.12+g1111111".into()),
        };
        let steps = plan(&s);
        let actions: Vec<&Action> = steps.iter().filter_map(|s| s.action.as_ref()).collect();
        assert!(actions.contains(&&Action::InstallDaemon));
        assert!(actions.contains(&&Action::InstallService { restart: true }));
    }

    /// The daemon copy's version comes from its marker, an unverified hint
    /// `_faber` could have written: it decides only whether a newer copy is
    /// kept, and `--replace-daemon` overrides even that.
    #[test]
    fn the_version_marker_only_decides_keep_or_replace() {
        let with = |version: Option<&str>, replace: bool| {
            let mut s = set_up_mac();
            s.daemon_bin = BinProbe::File {
                sha256: "e".repeat(64),
                uid: 451,
                gid: 451,
                mode: 0o100_755,
                version: version.map(str::to_string),
            };
            s.opts.replace_daemon = replace;
            s
        };
        let s = with(Some("0.9.0"), false);
        let steps = plan(&s);
        assert!(with_action(&steps).is_empty(), "{}", render(&s, &steps));
        let d = daemon_step(&steps);
        assert_eq!(d.status, Status::Ok);
        assert!(d.detail.contains("--replace-daemon") && d.detail.contains("unverified"));

        for (version, replace) in [
            (Some("0.9.0"), true),
            (Some("0.3.0"), false),
            (None, false),
            (None, true),
        ] {
            let s = with(version, replace);
            let steps = plan(&s);
            assert_eq!(
                daemon_step(&steps).action,
                Some(Action::InstallDaemon),
                "{version:?} {replace}"
            );
            assert!(steps
                .iter()
                .any(|st| st.action == Some(Action::InstallService { restart: true })));
            assert_eq!(counts(&steps).2, 0);
        }
    }

    #[test]
    fn a_daemon_copy_that_is_not_a_file_is_repaired_only_on_request() {
        let mut s = set_up_mac();
        s.daemon_bin = BinProbe::NotRegular;
        assert_eq!(daemon_step(&plan(&s)).status, Status::Refused);
        s.opts.replace_daemon = true;
        assert_eq!(daemon_step(&plan(&s)).action, Some(Action::InstallDaemon));

        let mut s = set_up_mac();
        s.daemon_bin = BinProbe::Unknown("too large".into());
        assert_eq!(daemon_step(&plan(&s)).status, Status::Refused);
        s.opts.replace_daemon = true;
        assert_eq!(daemon_step(&plan(&s)).action, Some(Action::InstallDaemon));
    }

    #[test]
    fn remigrate_moves_the_existing_copy_aside() {
        let mut s = set_up_mac();
        s.opts.remigrate = true;
        let steps = plan(&s);
        let items = migrated(&steps);
        assert_eq!(items.len(), 2);
        assert!(items.iter().all(|i| i.replace));
        assert!(items
            .iter()
            .all(|i| i.account == "owner" && i.owner_uid == 501));
        // Migrating rewrites the record's provenance.
        assert!(steps
            .iter()
            .any(|st| st.action == Some(Action::WriteRecord)));
    }

    #[test]
    fn nothing_to_migrate_seeds_the_starter() {
        let mut s = fresh_mac();
        s.legacy[0].instance = None;
        for f in &mut s.legacy[0].files {
            f.2 = SrcProbe::Missing;
        }
        let steps = plan(&s);
        assert!(steps.iter().any(|st| st.action == Some(Action::SeedConfig)));
        assert!(migrated(&steps).is_empty());
        assert!(steps.iter().any(|st| st.subject == "config check"));
    }

    #[test]
    fn a_symlinked_source_is_refused() {
        let mut s = fresh_mac();
        s.legacy[0].files[0].2 =
            SrcProbe::Refused("is a symlink, and setup copies regular files only".into());
        let steps = plan(&s);
        let refused: Vec<&Step> = steps
            .iter()
            .filter(|st| st.status == Status::Refused)
            .collect();
        assert_eq!(refused.len(), 1);
        assert!(refused[0].detail.contains("symlink"));
        assert!(!steps.iter().any(|st| st.action == Some(Action::SeedConfig)));
    }

    /// A host where the operator has no per-user state of their own and an
    /// old `ci` account runs a per-user faber with a config.
    fn operator_without_state_and_ci_with_it() -> HostState {
        let mut s = fresh_mac();
        s.legacy[0].instance = None;
        for f in &mut s.legacy[0].files {
            f.2 = SrcProbe::Missing;
        }
        s.legacy[0].dirs.clear();
        let ci_home = PathBuf::from("/Users/ci");
        s.legacy.push(LegacyAccount {
            name: "ci".into(),
            uid: 502,
            home: ci_home.clone(),
            instance: Some(running_instance(&ci_home)),
            files: legacy_files(Os::MacOs, &ci_home),
            identity: vec![],
            dirs: legacy_home_dirs(Os::MacOs, &ci_home, 502),
        });
        s
    }

    /// Fix for a review finding: only the account that ran sudo is migrated
    /// from, unless `--migrate-from` names another. Another account's
    /// per-user faber is still stopped. And when it has a config the operator
    /// lacks, setup never seeds a starter config past it: it asks.
    #[test]
    fn another_accounts_config_is_a_question_not_a_silent_seed() {
        let s = operator_without_state_and_ci_with_it();
        let steps = plan(&s);
        assert!(migrated(&steps).is_empty(), "{}", render(&s, &steps));
        assert!(!steps.iter().any(|st| st.action == Some(Action::SeedConfig)));
        let refused: Vec<&Step> = steps
            .iter()
            .filter(|st| st.status == Status::Refused)
            .collect();
        assert_eq!(refused.len(), 1, "{}", render(&s, &steps));
        assert_eq!(
            refused[0].detail,
            "found faber config for ci: pass --migrate-from ci to migrate it, or --fresh to start \
             from the starter config"
        );
        assert!(steps.iter().any(|st| st.action
            == Some(Action::StopOld {
                account: "ci".into()
            })));
        // ci's directories are not the operator's: never moved aside.
        assert!(!steps
            .iter()
            .any(|st| matches!(st.action, Some(Action::MoveLegacyAside { .. }))));
    }

    #[test]
    fn fresh_seeds_the_starter_and_copies_nothing() {
        let mut s = operator_without_state_and_ci_with_it();
        s.opts.fresh = true;
        let steps = plan(&s);
        assert_eq!(counts(&steps).2, 0, "{}", render(&s, &steps));
        assert!(migrated(&steps).is_empty());
        assert!(steps.iter().any(|st| st.action == Some(Action::SeedConfig)));
        // ci's directories are not the operator's: they stay, and say so.
        let ci = steps
            .iter()
            .find(|st| st.subject == "not copied: ci's per-user state")
            .expect("ci's state is named");
        assert_eq!(ci.detail, "--fresh: not copied, and left where it is");
        assert!(!steps
            .iter()
            .any(|st| matches!(st.action, Some(Action::MoveLegacyAside { .. }))));

        // --fresh wins over the operator's own state too.
        let mut s = fresh_mac();
        s.opts.fresh = true;
        let steps = plan(&s);
        assert!(migrated(&steps).is_empty());
        assert!(steps.iter().any(|st| st.action == Some(Action::SeedConfig)));
        // Fix for a review finding: not copied, but not left where it is
        // either. The text says what the plan does to those directories.
        let own = |steps: &[Step]| {
            steps
                .iter()
                .find(|st| st.subject == "not copied: owner's per-user state")
                .expect("the operator's state is named")
                .detail
                .clone()
        };
        assert!(
            own(&steps).contains("Renamed aside once the new daemon answers"),
            "{}",
            render(&s, &steps)
        );
        assert!(steps
            .iter()
            .any(|st| matches!(st.action, Some(Action::MoveLegacyAside { .. }))));

        // With --keep-legacy nothing moves, and the text says that instead.
        s.opts.keep_legacy = true;
        let steps = plan(&s);
        assert_eq!(own(&steps), "--fresh: not copied, and left where it is");
        assert!(!steps
            .iter()
            .any(|st| matches!(st.action, Some(Action::MoveLegacyAside { .. }))));
    }

    #[test]
    fn migrate_from_copies_the_named_accounts_state() {
        let mut s = operator_without_state_and_ci_with_it();
        s.opts.migrate_from = Some("ci".into());
        let steps = plan(&s);
        assert_eq!(counts(&steps).2, 0, "{}", render(&s, &steps));
        let items = migrated(&steps);
        assert_eq!(items.len(), 2);
        assert!(items
            .iter()
            .all(|i| i.account == "ci" && i.owner_uid == 502));
        assert!(!steps.iter().any(|st| st.action == Some(Action::SeedConfig)));
        // The named account's directories are moved aside with the operator's.
        let moved: Vec<&str> = steps
            .iter()
            .filter_map(|st| match &st.action {
                Some(Action::MoveLegacyAside { account, .. }) => Some(account.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(moved, vec!["ci", "ci", "ci"]);

        s.opts.migrate_from = Some("nobody-here".into());
        let steps = plan(&s);
        assert!(steps
            .iter()
            .any(|st| st.status == Status::Refused && st.detail.contains("--migrate-from")));
    }

    #[test]
    fn a_wrong_shaped_account_is_refused() {
        let mut s = fresh_mac();
        s.faber_user = Some(UserInfo {
            name: FABER_ACCOUNT.into(),
            uid: 501,
            gid: 20,
            home: "/Users/owner".into(),
            shell: "/bin/zsh".into(),
            hidden: false,
            password: Some("*".into()),
            auth_authority: false,
        });
        s.faber_group = Some(GroupInfo {
            name: FABER_ACCOUNT.into(),
            gid: 20,
            members: vec![],
        });
        let steps = plan(&s);
        assert!(steps
            .iter()
            .any(|st| st.status == Status::Refused && st.subject == "user _faber"));
    }

    /// macOS 27's `sysadminctl -roleAccount` ignores `-home` and leaves
    /// `/var/empty` (2026-10-21). An account setup created that way is adopted
    /// and its home set; a fresh plan sets it right after creating it.
    #[test]
    fn a_role_account_left_at_var_empty_gets_its_home_set() {
        let mut s = fresh_mac();
        let home = accounts::home(FABER_ACCOUNT, &s.layout);
        let created = plan(&s);
        assert!(created.iter().any(|st| st.status == Status::WillCreate
            && st.subject == format!("_faber's home {}", home.display())
            && st.detail.contains("NFSHomeDirectory")));

        s.faber_user = Some(UserInfo {
            name: FABER_ACCOUNT.into(),
            uid: 450,
            gid: 450,
            home: accounts::ROLE_ACCOUNT_HOME.into(),
            shell: "/usr/bin/false".into(),
            hidden: true,
            password: Some("*".into()),
            auth_authority: false,
        });
        s.faber_group = Some(GroupInfo {
            name: FABER_ACCOUNT.into(),
            gid: 450,
            members: vec![],
        });
        let steps = plan(&s);
        assert!(!steps
            .iter()
            .any(|st| st.status == Status::Refused && st.subject == "user _faber"));
        let set = steps
            .iter()
            .find(|st| st.subject == format!("_faber's home {}", home.display()))
            .expect("no step sets the home");
        assert_eq!(set.status, Status::WillChange);
        assert_eq!(
            set.action,
            Some(Action::Run(accounts::set_home(FABER_ACCOUNT, &home)))
        );

        // Any other wrong home is still refused, not repaired.
        s.faber_user.as_mut().unwrap().home = "/Users/someone".into();
        let steps = plan(&s);
        assert!(steps
            .iter()
            .any(|st| st.status == Status::Refused && st.subject == "user _faber"));
    }

    #[test]
    fn missing_sudo_identity_and_an_untrusted_binary_refuse() {
        let mut s = fresh_mac();
        s.operator = Err("SUDO_UID is 0".into());
        let mut chain = trusted_chain();
        chain[1].uid = 450; // the directory above the binary, owned by the id _faber gets
        s.exe_chain = Ok(chain);
        let steps = plan(&s);
        let refused: Vec<&str> = steps
            .iter()
            .filter(|st| st.status == Status::Refused)
            .map(|st| st.subject.as_str())
            .collect();
        assert_eq!(refused, vec!["operator", "this binary"]);
        assert!(!steps
            .iter()
            .any(|st| st.action == Some(Action::WriteRecord)));
    }

    #[test]
    fn an_untrusted_record_is_rewritten() {
        let mut s = set_up_mac();
        s.record = RecordProbe::Untrusted("is owned by uid 451, not root".into());
        let steps = plan(&s);
        let rec = steps
            .iter()
            .find(|st| st.section == Section::Record)
            .unwrap();
        assert_eq!(rec.status, Status::WillChange);
        assert!(rec.detail.contains("uid 451"));
        assert_eq!(rec.action, Some(Action::WriteRecord));
    }

    #[test]
    fn a_symlinked_directory_is_refused() {
        let mut s = fresh_mac();
        s.dirs[4].1 = DirProbe::NotADir("a symlink".into());
        let steps = plan(&s);
        assert_eq!(counts(&steps).2, 1);
    }

    /// Fix for a review finding: `/Library/Caches` is world-writable, so
    /// anyone could have made `/Library/Caches/faber` first. A directory owned
    /// by neither root nor the account it is for is refused, with the command
    /// that clears it; a root-owned one is taken over.
    #[test]
    fn a_planted_directory_is_refused_not_adopted() {
        let mut s = set_up_mac();
        let cache = s
            .dirs
            .iter()
            .position(|(d, _)| d.path == s.layout.cache_dir)
            .unwrap();
        s.dirs[cache].1 = DirProbe::Dir {
            uid: 501,
            gid: 20,
            mode: 0o040_755,
        };
        let steps = plan(&s);
        let r: Vec<&Step> = steps
            .iter()
            .filter(|st| st.status == Status::Refused)
            .collect();
        assert_eq!(r.len(), 1, "{}", render(&s, &steps));
        assert!(
            r[0].detail
                .contains("sudo rm -rf \"/Library/Caches/faber\""),
            "{}",
            r[0].detail
        );

        s.dirs[cache].1 = DirProbe::Dir {
            uid: 0,
            gid: 0,
            mode: 0o040_755,
        };
        let steps = plan(&s);
        assert_eq!(counts(&steps).2, 0);
        assert!(steps.iter().any(|st| matches!(
            &st.action,
            Some(Action::EnsureDir(d)) if d.path == s.layout.cache_dir
        )));
    }

    /// Fix for a review finding: on macOS the log directory is root's, and
    /// the three files launchd and the daemon write are pre-created as
    /// `_faber`'s, so `_faber` can neither create nor swap anything there.
    #[test]
    fn macos_logs_live_in_a_root_owned_directory() {
        let l = SystemLayout::for_os(Os::MacOs);
        let logs = dir_specs(Os::MacOs, &l)
            .into_iter()
            .find(|d| d.path == l.log_dir)
            .unwrap();
        assert_eq!(logs.owner, Owner::RootOperatorGroup);
        assert_eq!(
            owner_account(logs.owner, ids()),
            Account {
                uid: 0,
                gid: OPERATOR_GID
            }
        );
        assert_eq!(logs.mode, 0o750);
        let names: Vec<PathBuf> = file_specs(Os::MacOs, &l)
            .into_iter()
            .filter(|f| f.create)
            .map(|f| {
                assert_eq!((f.owner, f.mode), (Owner::FaberOperatorGroup, 0o640));
                assert!(!f.in_faber_dir, "{}", f.path.display());
                f.path
            })
            .collect();
        assert_eq!(
            names,
            ["stdout.log", "stderr.log", "audit.jsonl"].map(|n| l.log_dir.join(n))
        );
        let lin = SystemLayout::for_os(Os::Linux);
        assert!(!file_specs(Os::Linux, &lin).iter().any(|f| f.create));
        let lin_logs = dir_specs(Os::Linux, &lin)
            .into_iter()
            .find(|d| d.path == lin.log_dir)
            .unwrap();
        assert_eq!(lin_logs.owner, Owner::FaberOperatorGroup);

        // A fresh host creates the two launchd logs; the audit log comes from
        // the migration instead of being created empty.
        let steps = plan(&fresh_mac());
        let created: Vec<PathBuf> = steps
            .iter()
            .filter_map(|st| match &st.action {
                Some(Action::EnsureFile(f)) => Some(f.path.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(
            created,
            vec![l.log_dir.join("stdout.log"), l.log_dir.join("stderr.log")]
        );

        // A log file someone else owns, or a symlink, is refused.
        let mut s = set_up_mac();
        s.files[1].1 = FileProbe::File {
            uid: 501,
            gid: 20,
            mode: 0o100_644,
        };
        s.files[2].1 = FileProbe::NotRegular("a symlink".into());
        let steps = plan(&s);
        assert_eq!(counts(&steps).2, 2, "{}", render(&s, &steps));
        assert!(steps
            .iter()
            .all(|st| st.status != Status::Refused || st.detail.contains("sudo rm")));

        // The log directory as the previous release left it, `_faber`'s, is
        // taken over by root.
        let mut s = set_up_mac();
        let at = s
            .dirs
            .iter()
            .position(|(d, _)| d.path == l.log_dir)
            .unwrap();
        s.dirs[at].1 = DirProbe::Dir {
            uid: 451,
            gid: 451,
            mode: 0o040_750,
        };
        let steps = plan(&s);
        assert_eq!(counts(&steps), (0, 1, 0), "{}", render(&s, &steps));
    }

    #[test]
    fn an_unprivileged_preview_guesses_instead_of_refusing() {
        let mut s = fresh_mac();
        s.privileged = false;
        s.dirs[2].1 = DirProbe::Unknown("permission denied".into());
        let steps = plan(&s);
        assert_eq!(counts(&steps).2, 0, "{}", render(&s, &steps));
    }

    /// Everything a record lists is something this release knows how to
    /// remove, and entries a later release added survive a rewrite.
    #[test]
    fn the_record_keeps_a_previous_records_unknown_artifacts() {
        let mut s = set_up_mac();
        let later = Artifact::Other(serde_json::json!({"kind": "vm", "name": "faber-vm"}));
        if let RecordProbe::Trusted(r) = &mut s.record {
            r.artifacts.push(later.clone());
        }
        let r = record_for(&s, &operator(), ids(), None);
        assert_eq!(r.artifacts.last(), Some(&later));
        assert_eq!(r.artifacts.len(), artifacts(Os::MacOs, &s.layout).len() + 1);
    }

    /// Fix for a review finding: `--remigrate` on a set-up host copies into a
    /// state directory a running daemon would overwrite from memory, so the
    /// daemon is drained and stopped first and started again after, as for a
    /// new binary, with the same rollback.
    #[test]
    fn remigrate_on_a_running_host_drains_and_restarts() {
        let mut s = set_up_mac();
        s.opts.remigrate = true;
        let steps = plan(&s);
        let service = steps
            .iter()
            .find(|st| st.section == Section::Service)
            .unwrap();
        assert_eq!(
            service.action,
            Some(Action::InstallService { restart: true })
        );
        assert!(
            service.detail.contains("its state swapped"),
            "{}",
            service.detail
        );
        // Without it, nothing restarts.
        let steps = plan(&set_up_mac());
        assert!(!steps
            .iter()
            .any(|st| matches!(st.action, Some(Action::InstallService { .. }))));
    }

    /// Fix for a review finding: the config to migrate is checked with this
    /// binary's check-config at plan time, so a config the new daemon would
    /// refuse is a refusal before anything is drained, not a rollback after.
    #[test]
    fn a_source_config_that_fails_the_check_is_refused_before_anything_stops() {
        let mut s = fresh_mac();
        s.source_check = Some(Err("faber: profile x: dind needs a wall on Linux".into()));
        let steps = plan(&s);
        let refused: Vec<&Step> = steps
            .iter()
            .filter(|st| st.status == Status::Refused)
            .collect();
        assert_eq!(refused.len(), 1, "{}", render(&s, &steps));
        assert_eq!(refused[0].subject, "source config check");
        assert!(refused[0].detail.contains("run as owner"));
        assert!(refused[0].detail.contains("dind needs a wall"));

        s.source_check = Some(Ok(()));
        let steps = plan(&s);
        assert_eq!(counts(&steps).2, 0);
        assert!(steps
            .iter()
            .any(|st| st.subject == "source config check" && st.status == Status::Ok));
        // The post-migration check as _faber stays.
        assert!(steps.iter().any(|st| st.subject == "config check"));
    }

    /// Fix for a review finding: after a healthy start the operator's LEGACY
    /// directories are renamed aside, unless `--keep-legacy`; one that is not
    /// theirs, or not a real directory, is left alone.
    #[test]
    fn legacy_dirs_are_moved_aside_unless_kept() {
        let s = fresh_mac();
        let moved: Vec<PathBuf> = plan(&s)
            .into_iter()
            .filter_map(|st| match st.action {
                Some(Action::MoveLegacyAside { account, uid, path }) => {
                    assert_eq!((account.as_str(), uid), ("owner", 501));
                    Some(path)
                }
                _ => None,
            })
            .collect();
        let home = Path::new("/Users/owner");
        assert_eq!(moved, legacy_dirs(&legacy_paths(Os::MacOs, home)));
        assert_eq!(
            moved,
            [
                "Library/Application Support/faber",
                "Library/Logs/faber",
                "Library/Caches/faber"
            ]
            .map(|d| home.join(d))
        );

        let mut kept = fresh_mac();
        kept.opts.keep_legacy = true;
        let steps = plan(&kept);
        assert!(!steps
            .iter()
            .any(|st| matches!(st.action, Some(Action::MoveLegacyAside { .. }))));
        assert!(steps
            .iter()
            .any(|st| st.detail == "kept where it is (--keep-legacy)"));

        let mut odd = fresh_mac();
        odd.legacy[0].dirs[0].owner = Some(0);
        odd.legacy[0].dirs[1].owner = None;
        let steps = plan(&odd);
        let moves = steps
            .iter()
            .filter(|st| matches!(st.action, Some(Action::MoveLegacyAside { .. })))
            .count();
        assert_eq!(moves, 1);

        // On Linux, the XDG directories.
        let lin = legacy_dirs(&legacy(&Env::with_home("/home/owner")));
        #[cfg(target_os = "linux")]
        assert_eq!(
            lin,
            [".config/faber", ".local/state/faber", ".cache/faber"]
                .map(|d| Path::new("/home/owner").join(d))
        );
        assert!(!lin.is_empty());
    }

    /// Fix for a review finding: the root copy `sudo faber` runs must sit
    /// under directories only root can write. An operator-owned
    /// `/usr/local/bin` is refused at plan time, with why.
    #[test]
    fn an_operator_writable_cli_directory_is_refused() {
        let mut s = fresh_mac();
        s.cli_chain[0].uid = 501;
        let steps = plan(&s);
        let r: Vec<&Step> = steps
            .iter()
            .filter(|st| st.status == Status::Refused)
            .collect();
        assert_eq!(r.len(), 1, "{}", render(&s, &steps));
        assert_eq!(r[0].subject, "/usr/local/bin/faber");
        assert!(r[0].detail.contains("uid 501"), "{}", r[0].detail);
        assert!(r[0].detail.contains("Homebrew"), "{}", r[0].detail);

        let mut s = fresh_mac();
        s.cli_chain[1].mode = 0o040_775;
        assert_eq!(counts(&plan(&s)).2, 1);
        let mut s = fresh_mac();
        s.cli_chain[0].mode = 0o120_755; // a symlink
        assert_eq!(counts(&plan(&s)).2, 1);
    }

    /// Fix for a review finding: a binary the operator can write is fine to
    /// run once, and the plan says to prefer the root-owned copy afterwards.
    #[test]
    fn an_operator_writable_binary_gets_a_nudge_not_a_refusal() {
        let mut s = fresh_mac();
        if let Ok(chain) = &mut s.exe_chain {
            chain[0].uid = 501;
        }
        let steps = plan(&s);
        assert_eq!(counts(&steps).2, 0);
        let binary = steps
            .iter()
            .find(|st| st.subject.starts_with("this binary"))
            .unwrap();
        assert_eq!(
            binary.detail,
            "this binary is writable by owner; after this install, prefer `sudo \
             /usr/local/bin/faber`"
        );
    }
}
