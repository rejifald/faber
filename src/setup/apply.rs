//! Executing a plan (ADR-0025): the thin, impure half of `sudo faber setup`.
//!
//! The plan's [`Action`]s run in four phases, not in the order they print:
//!
//! 1. **Additive.** Accounts (each new one left with no password, then
//!    checked), directories and the CLI copy. Nothing that runs is touched, so
//!    a failure here leaves the old instance serving.
//! 2. **Stop.** Each old per-user faber is drained and stopped, and its
//!    definition moved aside. On a set-up host whose daemon binary or service
//!    definition changes, the SYSTEM daemon is drained and stopped instead.
//! 3. **Swap.** The daemon copy (the one it replaces kept aside), migrated or
//!    seeded config and state, the log files, the config checked by the CLI
//!    copy running `check-config` as `_faber`, and the setup record, written
//!    last.
//! 4. **Start.** The service definition installed and started, and the
//!    daemon's API answering on loopback. Only then is the replaced daemon
//!    binary deleted.
//!
//! A failure in phases 2 to 4, or Ctrl-C, rolls back: the new service is
//! stopped and its definition removed or restored, the previous daemon binary
//! put back, the record removed or restored, every file this run created
//! removed, and every stopped instance put back and started, so the legacy
//! daemon re-adopts its containers (ADR-0011). Rollback works through the
//! directory descriptors the run itself opened, so nothing `_faber` rearranged
//! in the meantime can redirect it.

use std::ffi::OsStr;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};

use super::accounts;
use super::fsx::{self, Account, Dir, ROOT};
use super::migrate::{self, CopyRequest};
use super::old::{self, Stopped};
use super::plan::{
    self, Action, DirSpec, FileKind, FileSpec, HostState, MigrationItem, OperatorInfo, RecordProbe,
    Step, DEFAULT_PORT,
};
use super::tools::{self, Cmd, Runner, Tool};
use super::{api_answers, system_socket, SetupOpts};
use crate::control::Fallback;
use crate::platform::layout::{Os, FABER_ACCOUNT, FABER_VM_ACCOUNT};
use crate::platform::record::{self, MigratedFile, MigratedFrom};
use crate::platform::service::{self, launchd, systemd};

/// How long a freshly started daemon has to answer.
const READY_LIMIT: Duration = Duration::from_secs(120);
/// A little above `ExitTimeOut`: past it launchd has SIGKILLed the job anyway.
const LABEL_WAIT: Duration = Duration::from_secs(service::STOP_TIMEOUT.as_secs() + 60);

/// A file this run created in one of faber's directories, with that directory
/// held open: rollback removes it through the descriptor, never by path.
struct Created {
    dir: Dir,
    name: String,
    /// What `--remigrate` renamed the previous file to.
    moved_aside: Option<String>,
}

/// The daemon copy this run replaced, kept aside in its own directory (held
/// open) until the new one answers.
struct DaemonSwap {
    dir: Dir,
    name: String,
    /// Where the previous binary went; `None` when there was none.
    backup: Option<String>,
}

/// A per-user faber asked to drain and not yet stopped. Draining is
/// one-way, so rollback restarts it.
struct Draining {
    account: String,
    uid: u32,
    definition: Option<std::path::PathBuf>,
}

/// What phases 2 to 4 changed, for rollback.
#[derive(Default)]
struct Progress {
    draining: Vec<Draining>,
    stopped: Vec<Stopped>,
    /// The running SYSTEM daemon was asked to drain.
    system_drained: bool,
    /// The running SYSTEM daemon was stopped to swap what it runs from.
    system_stopped: bool,
    created: Vec<Created>,
    daemon: Option<DaemonSwap>,
    /// The record's previous bytes (`None`: there was none), once replaced.
    record_before: Option<Option<Vec<u8>>>,
    /// The service definition's previous bytes, once replaced.
    unit_before: Option<Option<Vec<u8>>>,
    /// The new service was (possibly partly) started.
    started: bool,
}

struct Ctx<'a> {
    s: &'a HostState,
    steps: &'a [Step],
    opts: &'a SetupOpts,
    op: &'a OperatorInfo,
    faber: Account,
    vm: Account,
    /// The operator group's gid.
    operator_gid: u32,
    ts: &'a str,
    autoupdate: bool,
}

impl Ctx<'_> {
    fn has(&self, action: &Action) -> bool {
        self.steps
            .iter()
            .any(|st| st.action.as_ref() == Some(action))
    }

    fn ids(&self) -> plan::Ids {
        plan::Ids {
            faber: self.faber,
            vm: self.vm,
            operator_gid: self.operator_gid,
        }
    }

    /// `_faber`, with the operator group: who owns what setup writes in the
    /// state and log trees, so the operator can read it.
    fn faber_operator(&self) -> Account {
        self.ids().faber_operator()
    }
}

/// Execute `steps`, which [`plan::plan`] made from `s`. Returns where the
/// LEGACY directories it moved aside went, for the handoff.
pub async fn apply(
    s: &HostState,
    steps: &[Step],
    opts: &SetupOpts,
    autoupdate: bool,
    ts: &str,
    runner: &mut dyn Runner,
) -> Result<Vec<PathBuf>> {
    let op = s.operator.as_ref().map_err(|e| anyhow!("{e}"))?;
    let plan::Ids {
        faber,
        vm,
        operator_gid,
    } = plan::planned(s).map_err(|e| anyhow!("{e}"))?;
    // Before any directory is made: the guard refuses a parent any of faber's
    // groups can write, the operator group included, which `_faber` is in.
    super::fsx::set_role_gids(&[faber.gid, vm.gid, operator_gid]);
    let c = Ctx {
        s,
        steps,
        opts,
        op,
        faber,
        vm,
        operator_gid,
        ts,
        autoupdate,
    };

    // Phase 1: additive, in plan order: each account is created, then left
    // with no password, then hidden.
    for st in steps {
        match &st.action {
            Some(Action::Run(cmd)) => {
                println!("  {}", cmd.display());
                runner.run_ok(cmd)?;
            }
            Some(Action::LockPassword { name }) => lock_password(s.os, runner, name)?,
            _ => {}
        }
    }
    verify_accounts(&c, runner)?;
    for st in steps {
        if let Some(Action::EnsureDir(spec)) = &st.action {
            if !spec.after_stop {
                ensure_dir(&c, spec)?;
            }
        }
    }
    if c.has(&Action::InstallCli) {
        install_cli(&c)?;
    }
    // macOS: the pinned Docker CLI the daemon runs (ADR-0026, amended
    // 2026-10-21). Here, with the binaries and before anything is drained: it
    // downloads, and a failure must stop setup while every old instance still
    // runs.
    #[cfg(target_os = "macos")]
    if c.has(&Action::Vm(super::vm_runtime::VmAction::InstallDockerCli)) {
        super::docker_cli::apply::install(s, faber)
            .await
            .context("installing the Docker CLI")?;
    }
    if c.has(&Action::ReownCredential) {
        reown_credential(&c);
    }

    // Phases 2 to 4: from the first drain on, a failure rolls back, and so
    // does Ctrl-C: stopping half way would leave a drained daemon that claims
    // nothing and no new one to replace it.
    let mut progress = Progress::default();
    let outcome = tokio::select! {
        outcome = stop_swap_start(&c, runner, &mut progress) => outcome,
        _ = tokio::signal::ctrl_c() => Err(anyhow!("interrupted")),
    };
    if let Err(e) = outcome {
        let report = rollback(&c, runner, &progress);
        return Err(e.context(report));
    }
    // The new daemon answers: the binary it replaced is no longer needed.
    if let Some(swap) = &progress.daemon {
        if let Some(backup) = &swap.backup {
            if let Err(e) = swap.dir.unlink(backup) {
                println!(
                    "  (could not remove {}: {e})",
                    swap.dir.path().join(backup).display()
                );
            }
        }
    }

    if c.has(&Action::KeepAwake) {
        for arg in ["sleep", "powernap"] {
            runner.run_ok(&Cmd::new(Tool::Pmset, ["-a", arg, "0"]))?;
        }
        println!("  pmset -a sleep 0; pmset -a powernap 0");
    }
    let backups = move_legacy_aside(&c).await;
    // Last, with the daemon answering on the socket the app talks to. Never
    // fatal: faber itself is set up, and a re-run plans only this.
    #[cfg(target_os = "macos")]
    if c.has(&Action::InstallApp) {
        super::app::apply::install(s, op, runner);
    }
    Ok(backups)
}

/// Build the Linux VM's image when the plan says so (ADR-0026): the last thing
/// setup does, after the daemon answers, since it takes up to half an hour.
/// A failure is loud and fails setup, and leaves the previous image, and
/// everything else this run did, in place: a re-run plans only this step.
pub async fn linux_image(s: &HostState, steps: &[Step]) -> Result<()> {
    if !steps
        .iter()
        .any(|st| st.action == Some(Action::BuildLinuxImage))
    {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        use super::linux_image::{self, BuildSpec, MacHost};
        use crate::platform::layout::IMAGE_STORE;

        let (faber, vm) = plan::planned_ids(s).map_err(|e| anyhow!("{e}"))?;
        println!(
            "\n  building the Linux VM image from Debian {} (up to 30 minutes)",
            linux_image::BASE_BUILD
        );
        // Root's own all the way up, created root-owned 0755 where missing,
        // walked without following a symlink (so from `/private/var` on macOS).
        let store = linux_image::resolve_existing_prefix(Path::new(IMAGE_STORE))
            .and_then(|store| fsx::root_dir_chain(&store).map(|_| store))
            .with_context(|| format!("preparing {IMAGE_STORE}"))?;
        let host = MacHost::new(
            vm,
            faber,
            &s.layout.vm_home,
            &linux_image::launcher_path(&s.layout),
        )?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let mut spec = BuildSpec::release();
        let kept = s.layout.log_dir.join(linux_image::FAILED_CONSOLE_FILE);
        // A console from an earlier failure would be read as this run's.
        let _ = std::fs::remove_file(&kept);
        spec.failed_console = Some(kept.clone());
        match linux_image::build(&host, &store, &spec, now).await {
            Ok(marker) => {
                println!(
                    "  Linux VM image built: Debian {}, Docker {}, in {IMAGE_STORE}/linux",
                    marker.base_build, marker.docker_version
                );
                Ok(())
            }
            Err(e) => {
                eprintln!(
                    "\nERROR: the Linux VM image was not built: {e}\n  The previous image, if \
                     any, is unchanged. Fix the cause and re-run `sudo faber setup`."
                );
                if std::fs::symlink_metadata(&kept).is_ok() {
                    eprintln!(
                        "  The build VM's console is kept, root-only: sudo tail -n 80 {}",
                        crate::control::shell_quote(&kept.display().to_string())
                    );
                }
                Err(anyhow!("building the Linux VM image: {e}"))
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = s;
        bail!("the Linux VM image is built on macOS only")
    }
}

/// Give a stored GitHub token in the store's earlier `_faber`-owned form to
/// `root:_faber` at mode 0640 (ADR-0027). Never fatal: the daemon reads the
/// earlier form too, with a warning, so a failure here is reported and setup
/// goes on.
fn reown_credential(c: &Ctx<'_>) {
    let store = crate::credential::Store::new(c.s.layout.credentials_dir.clone(), c.faber);
    match store.reown_legacy() {
        Ok(Some(path)) => println!(
            "  rewrote {} as root:{FABER_ACCOUNT}, mode 0640: faber reads its token and can no \
             longer rewrite it",
            path.display()
        ),
        Ok(None) => {}
        Err(e) => println!(
            "  left the GitHub token in {} as it is: {e}",
            c.s.layout.credentials_dir.display()
        ),
    }
}

/// After a healthy start, and only then, so rollback always has the
/// originals: rename each LEGACY directory in the plan to
/// `<name>.pre-system-<ts>` beside it. The originals stay as the backup, and
/// nothing faber reads is left live in the account's home. A rename never
/// follows a symlink in the last component, and only a real directory the
/// account owns is moved. A failure here is reported, not fatal: the new
/// daemon is already up.
async fn move_legacy_aside(c: &Ctx<'_>) -> Vec<PathBuf> {
    let moves: Vec<(&str, u32, &Path)> = c
        .steps
        .iter()
        .filter_map(|st| match &st.action {
            Some(Action::MoveLegacyAside { account, uid, path }) => {
                Some((account.as_str(), *uid, path.as_path()))
            }
            _ => None,
        })
        .collect();
    if moves.is_empty() {
        return Vec::new();
    }
    if !api_answers(&system_socket(), Fallback::Never).await {
        println!(
            "  the daemon does not answer on its control socket: the old per-user \
             directories stay where they are; re-run setup once it does"
        );
        return Vec::new();
    }
    let mut moved = Vec::new();
    for (account, uid, path) in moves {
        match legacy_aside(path, uid, c.ts) {
            Ok(backup) => {
                println!("  moved {} to {}", path.display(), backup.display());
                moved.push(backup);
            }
            Err(e) => println!("  left {}'s {} where it is: {e:#}", account, path.display()),
        }
    }
    moved
}

/// Rename one LEGACY directory aside, if it is a real directory `uid` owns.
fn legacy_aside(path: &Path, uid: u32, ts: &str) -> Result<PathBuf> {
    let (parent, name) = fsx::split(path)?;
    let dir = Dir::open(parent).with_context(|| format!("opening {}", parent.display()))?;
    let meta = dir
        .open_file(name)
        .and_then(|f| f.metadata())
        .with_context(|| format!("{} is not a directory setup will move", path.display()))?;
    if !meta.is_dir() {
        bail!("{} is not a directory", path.display());
    }
    if std::os::unix::fs::MetadataExt::uid(&meta) != uid {
        bail!("{} is not owned by uid {uid}", path.display());
    }
    let aside = format!("{name}.pre-system-{ts}");
    dir.move_aside(name, &aside)?;
    Ok(parent.join(aside))
}

/// macOS: leave a just-created role account with no password, as `_www` has:
/// `Password` set to `*`, and any `AuthenticationAuthority` deleted.
/// [`verify_accounts`] then checks the result.
fn lock_password(os: Os, runner: &mut dyn Runner, name: &str) -> Result<()> {
    if os != Os::MacOs {
        return Ok(());
    }
    let [set, delete] = accounts::lock_password(name);
    println!("  {}", set.display());
    runner.run_ok(&set)?;
    let user = accounts::probe_user(os, runner, name)?
        .with_context(|| format!("{name} does not exist after setup created it"))?;
    if user.auth_authority {
        println!("  {}", delete.display());
        runner.run_ok(&delete)?;
    }
    Ok(())
}

/// The accounts exist with the ids the plan chose, and neither can
/// authenticate.
fn verify_accounts(c: &Ctx<'_>, runner: &mut dyn Runner) -> Result<()> {
    for (name, want) in [(FABER_ACCOUNT, c.faber), (FABER_VM_ACCOUNT, c.vm)] {
        let user = accounts::probe_user(c.s.os, runner, name)?
            .with_context(|| format!("{name} does not exist after setup created it"))?;
        if (user.uid, user.gid) != (want.uid, want.gid) {
            bail!(
                "{name} has uid {} and gid {}, not the planned {} and {}",
                user.uid,
                user.gid,
                want.uid,
                want.gid
            );
        }
        match accounts::password_locked(c.s.os, &user) {
            Some(true) => {}
            Some(false) => bail!(
                "{name} can still authenticate (Password {:?}, AuthenticationAuthority {}); \
                 setup will not run a daemon as an account with a password",
                user.password.as_deref().map(tools::quote),
                if user.auth_authority { "set" } else { "unset" }
            ),
            None => bail!("cannot read {name}'s password field to check that it is locked"),
        }
    }
    Ok(())
}

fn ensure_dir(c: &Ctx<'_>, spec: &DirSpec) -> Result<()> {
    let owner = plan::owner_account(spec.owner, c.ids());
    let prior = plan::prior_owners(spec.owner, c.ids());
    let (parent, name) = fsx::split(&spec.path)?;
    let opened = |r: std::io::Result<Dir>| {
        r.with_context(|| format!("opening the directory above {}", spec.path.display()))
    };
    // The state directory is `_faber`'s, so nothing is created there and then
    // reopened by name: it is made in root's staging beside the state
    // directory and moved in (#228).
    let (_, created) = if spec.in_state_dir {
        let state = &c.s.layout.state_dir;
        let dest = opened(fsx::open_leaf_dir(state))?;
        let staging = fsx::Staging::beside(state)
            .with_context(|| format!("making root's staging beside {}", state.display()))?;
        fsx::ensure_dir_beneath(&staging, &dest, name, owner, spec.mode)
    } else {
        fsx::ensure_dir(&opened(Dir::open(parent))?, name, owner, spec.mode, &prior)
    }
    .with_context(|| format!("preparing {}", spec.path.display()))?;
    println!(
        "  {} {} ({}:{} {:o})",
        if created { "created" } else { "fixed" },
        spec.path.display(),
        owner.uid,
        owner.gid,
        spec.mode
    );
    Ok(())
}

/// Create a log file empty, or take over the one there, through a descriptor
/// for its directory: the root-owned log directory, or for a state file setup
/// only fixes, a directory `_faber` can write, where only `_faber`'s own file
/// is taken over and a missing one is left missing.
fn ensure_file(c: &Ctx<'_>, spec: &FileSpec, p: &mut Progress) -> Result<()> {
    let owner = plan::owner_account(spec.owner, c.ids());
    let prior = if spec.in_faber_dir {
        vec![c.faber.uid]
    } else {
        plan::prior_owners(spec.owner, c.ids())
    };
    let (parent, name) = fsx::split(&spec.path)?;
    let dir =
        fsx::open_leaf_dir(parent).with_context(|| format!("opening {}", parent.display()))?;
    if !spec.create {
        let fixed = dir
            .fix_file(name, owner, spec.mode, &prior)
            .with_context(|| format!("preparing {}", spec.path.display()))?;
        if fixed {
            println!("  fixed {}", spec.path.display());
        }
        return Ok(());
    }
    let created = dir
        .ensure_file(name, owner, spec.mode, &prior)
        .with_context(|| format!("preparing {}", spec.path.display()))?;
    println!(
        "  {} {}",
        if created { "created" } else { "fixed" },
        spec.path.display()
    );
    if created {
        p.created.push(Created {
            dir,
            name: name.to_string(),
            moved_aside: None,
        });
    }
    Ok(())
}

/// Copy this binary into `dir` as `name`, owned by `owner`, 0755. The copy
/// is checked against the SHA-256 the plan was made with before it is renamed
/// into place ([`Dir::write_atomic_expecting`]): a binary swapped while the
/// operator read the plan is refused with nothing installed, not left behind
/// as a root-owned `faber` that the next `sudo faber` would run.
fn install_binary(c: &Ctx<'_>, dir: &Dir, name: &str, owner: Account) -> Result<()> {
    let dest = dir.path().join(name);
    // The plan refuses a binary it could not hash, so this is a guard, not a
    // path: never an unchecked copy.
    let want =
        c.s.exe_sha256
            .as_deref()
            .context("this binary's SHA-256 is unknown; not installing an unchecked copy")?;
    let mut src =
        File::open(&c.s.exe_path).with_context(|| format!("reading {}", c.s.exe_path.display()))?;
    dir.write_atomic_expecting(name, &mut src, owner, 0o755, want)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::InvalidData => anyhow!(
                "{} changed after the plan was made ({e}); nothing was installed at {}",
                c.s.exe_path.display(),
                dest.display()
            ),
            _ => anyhow::Error::new(e).context(format!("writing {}", dest.display())),
        })?;
    println!("  installed {}", dest.display());
    Ok(())
}

fn install_cli(c: &Ctx<'_>) -> Result<()> {
    let (parent, name) = fsx::split(&c.s.layout.cli_bin)?;
    // Every directory down to it walked without following a symlink, each
    // root-owned and writable by nobody else, the missing ones created that
    // way: whatever can write one of them could replace what `sudo faber` runs.
    let dir = fsx::root_dir_chain(parent)
        .with_context(|| format!("preparing {} for a root-owned binary", parent.display()))?;
    install_binary(c, &dir, name, ROOT)
}

/// Move whatever is at `name` aside to `<name>.pre-setup-<ts>`, so the new
/// binary never overwrites the one rollback may need.
fn begin_swap(dir: Dir, name: &str, ts: &str) -> Result<DaemonSwap> {
    let backup = if dir.exists(name)? {
        let backup = format!("{name}.pre-setup-{ts}");
        dir.move_aside(name, &backup)
            .with_context(|| format!("moving {} aside", dir.path().join(name).display()))?;
        Some(backup)
    } else {
        None
    };
    Ok(DaemonSwap {
        dir,
        name: name.to_string(),
        backup,
    })
}

/// Rollback: the previous daemon binary back in place, or the new one gone.
fn restore_daemon(swap: &DaemonSwap) -> Result<()> {
    match &swap.backup {
        Some(backup) => swap.dir.rename(backup, &swap.name)?,
        None => match swap.dir.unlink(&swap.name) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        },
    }
    Ok(())
}

fn install_daemon(c: &Ctx<'_>, p: &mut Progress) -> Result<()> {
    let (parent, name) = fsx::split(&c.s.layout.daemon_bin)?;
    let dir = faber_dir(c, parent)?;
    let swap = p.daemon.insert(begin_swap(dir, name, c.ts)?);
    install_binary(c, &swap.dir, &swap.name, c.faber_operator())
}

/// A directory inside the state or log tree, reached one component at a time
/// from the tree's root without following a symlink. A missing one is made as
/// `_faber`'s, with the operator group, 0750, in root's staging beside the
/// tree and moved in, never created and reopened by name in a directory
/// `_faber` can write (#228); an existing one keeps the owner and mode the
/// directories step gave it.
fn faber_dir(c: &Ctx<'_>, path: &Path) -> Result<Dir> {
    for root in [&c.s.layout.state_dir, &c.s.layout.log_dir] {
        if let Ok(rest) = path.strip_prefix(root) {
            let mut dir =
                fsx::open_leaf_dir(root).with_context(|| format!("opening {}", root.display()))?;
            let mut staging = None;
            for part in rest.components() {
                let name = part
                    .as_os_str()
                    .to_str()
                    .with_context(|| format!("{} is not UTF-8", path.display()))?;
                let next = match dir.subdir(name) {
                    Ok(next) => next,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        let staging =
                            match &mut staging {
                                Some(s) => s,
                                None => staging.insert(fsx::Staging::beside(root).with_context(
                                    || format!("making root's staging beside {}", root.display()),
                                )?),
                            };
                        fsx::place_new_dir(staging, &dir, name, c.faber_operator(), 0o750)?
                    }
                    Err(e) => {
                        return Err(anyhow!(
                            "{} is not a real directory ({e})",
                            dir.path().join(name).display()
                        ))
                    }
                };
                dir = next;
            }
            return Ok(dir);
        }
    }
    bail!("{} is outside faber's directories", path.display())
}

async fn stop_swap_start(c: &Ctx<'_>, runner: &mut dyn Runner, p: &mut Progress) -> Result<()> {
    let s = c.s;

    // Phase 2: stop. Each step is recorded before it is taken, so rollback
    // knows what may need undoing even when the step itself fails.
    for st in c.steps {
        if let Some(Action::StopOld { account }) = &st.action {
            let acct = s
                .legacy
                .iter()
                .find(|a| &a.name == account)
                .with_context(|| format!("no per-user faber for {account}"))?;
            let definition = acct.instance.as_ref().and_then(|i| i.definition.clone());
            p.draining.push(Draining {
                account: acct.name.clone(),
                uid: acct.uid,
                definition: definition.clone(),
            });
            old::drain(acct, c.opts.drain_timeout).await?;
            let was_loaded = old::halt(s, acct, runner).await?;
            p.draining.pop();
            p.stopped.push(Stopped {
                account: acct.name.clone(),
                uid: acct.uid,
                was_loaded,
                definition: definition.clone(),
                moved: None,
            });
            if let Some(def) = &definition {
                let moved = old::move_aside(def, acct.uid, c.ts)?;
                if let Some(last) = p.stopped.last_mut() {
                    last.moved = Some(moved);
                }
            }
        }
    }
    if c.has(&Action::InstallService { restart: true }) {
        p.system_drained = true;
        if api_answers(&system_socket(), Fallback::Never).await {
            println!("  draining the running daemon (control socket)");
            crate::drain::run(&system_socket(), Fallback::Never, c.opts.drain_timeout)
                .await
                .context("draining the running daemon")?;
        }
        stop_service(s.os, runner)?;
        p.system_stopped = true;
    }

    // Phase 3: swap. First what the old daemon reached only through its
    // group, now that it is stopped: the daemon starting next holds the new
    // one.
    for st in c.steps {
        if let Some(Action::EnsureDir(spec)) = &st.action {
            if spec.after_stop {
                ensure_dir(c, spec)?;
            }
        }
    }
    if c.has(&Action::InstallDaemon) {
        install_daemon(c, p)?;
    }
    let mut files = Vec::new();
    let mut source = None;
    let mut config_written = false;
    for st in c.steps {
        if let Some(Action::Migrate(item)) = &st.action {
            files.push(migrate_one(c, item, p)?);
            source = Some(item.account.clone());
            config_written |= item.kind == FileKind::Config;
        }
    }
    if c.has(&Action::SeedConfig) {
        seed_config(c, p)?;
        config_written = true;
    }
    for st in c.steps {
        if let Some(Action::EnsureFile(spec)) = &st.action {
            ensure_file(c, spec, p)?;
        }
    }
    if config_written {
        validate_config(c)?;
    }
    if c.has(&Action::WriteRecord) {
        let migrated_from = match source {
            Some(user) => Some(MigratedFrom { user, files }),
            None => match &s.record {
                RecordProbe::Trusted(r) => r.migrated_from.clone(),
                _ => None,
            },
        };
        write_record(c, migrated_from, p)?;
    }

    // macOS: the VM runtime (ADR-0026), once the config it reads and the
    // record are in place, before the daemon that uses it starts. Additive
    // and idempotent: rollback leaves what it did, and a re-run plans only
    // what is still missing.
    #[cfg(target_os = "macos")]
    super::vm_runtime::apply::apply(
        &super::vm_runtime::apply::VmCtx {
            s,
            steps: c.steps,
            faber: c.faber,
            vm: c.vm,
            operator_gid: c.operator_gid,
            ts: c.ts,
        },
        runner,
    )
    .context("installing the VM runtime")?;

    // Phase 4: start.
    if c.steps
        .iter()
        .any(|st| matches!(st.action, Some(Action::InstallService { .. })))
    {
        if api_answers(&system_socket(), Fallback::Tcp(DEFAULT_PORT)).await {
            bail!(
                "something still answers on its control socket: another faber is running, \
                 perhaps one started by hand. Stop it, then re-run"
            );
        }
        install_service(c, runner, p)?;
        wait_ready(s).await?;
    }
    Ok(())
}

fn migrate_one(c: &Ctx<'_>, item: &MigrationItem, p: &mut Progress) -> Result<MigratedFile> {
    let (src_parent, src_name) = fsx::split(&item.source)?;
    let src_dir =
        Dir::open(src_parent).with_context(|| format!("opening {}", src_parent.display()))?;
    let (dest_parent, dest_name) = fsx::split(&item.dest)?;
    let dest_dir = faber_dir(c, dest_parent)?;
    let copied = migrate::copy_one(&CopyRequest {
        src_dir: &src_dir,
        src_name,
        src_owner: item.owner_uid,
        dest_dir: &dest_dir,
        dest_name,
        owner: c.faber_operator(),
        mode: 0o640,
        remigrate: item.replace.then_some(c.ts),
    })
    .map_err(|e| anyhow!("copying {}: {e}", item.source.display()))?;
    // The source path is the old instance's `--config`, which its account
    // chose: shown on root's terminal only as text.
    println!(
        "  copied {} to {} (sha256 {})",
        crate::term::sanitize(&item.source.display().to_string()),
        item.dest.display(),
        copied.sha256
    );
    p.created.push(Created {
        dir: dest_dir,
        name: dest_name.to_string(),
        moved_aside: copied.moved_aside,
    });
    Ok(MigratedFile {
        path: item.source.clone(),
        sha256: copied.sha256,
    })
}

fn seed_config(c: &Ctx<'_>, p: &mut Progress) -> Result<()> {
    let dest = c.s.layout.config_path();
    let (parent, name) = fsx::split(&dest)?;
    let dir = faber_dir(c, parent)?;
    if dir.exists(name)? {
        bail!(
            "{} appeared while setup ran; not overwriting it",
            dest.display()
        );
    }
    let text = migrate::starter_config(c.autoupdate);
    dir.write_atomic(name, &mut text.as_bytes(), c.faber_operator(), 0o640)?;
    println!("  wrote the starter config to {}", dest.display());
    p.created.push(Created {
        dir,
        name: name.to_string(),
        moved_aside: None,
    });
    Ok(())
}

/// The root-owned CLI copy checks the config as `_faber`, in a session of its
/// own and with its output piped ([`tools::run_as`]): root never parses the
/// config, and nothing the check prints reaches the terminal unsanitized.
fn validate_config(c: &Ctx<'_>) -> Result<()> {
    let config = c.s.layout.config_path();
    let out = tools::run_as(
        &c.s.layout.cli_bin,
        &[
            OsStr::new("check-config"),
            OsStr::new("--config"),
            config.as_os_str(),
            // As for the source config's check in `probe` (ADR-0050).
            OsStr::new("--color"),
            OsStr::new("never"),
        ],
        None,
        Some(c.faber),
        &c.s.layout.state_dir,
        Duration::from_secs(60),
    )
    .with_context(|| format!("running check-config as {FABER_ACCOUNT}"))?;
    if !out.success {
        bail!(
            "{} does not pass `faber check-config` run as {FABER_ACCOUNT}:\n{}",
            config.display(),
            out.sanitized(4000)
        );
    }
    println!(
        "  {} passes check-config as {FABER_ACCOUNT}",
        config.display()
    );
    Ok(())
}

/// The bytes at `name` in `dir`, or `None` when there is nothing there.
fn read_existing(dir: &Dir, name: &str) -> Result<Option<Vec<u8>>> {
    match dir.open_file(name) {
        Ok(mut f) => {
            let mut bytes = Vec::new();
            f.read_to_end(&mut bytes)?;
            Ok(Some(bytes))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn write_record(c: &Ctx<'_>, migrated_from: Option<MigratedFrom>, p: &mut Progress) -> Result<()> {
    let mut record = plan::record_for(c.s, c.op, c.ids(), migrated_from);
    // The session account this run makes or keeps (ADR-0051), recorded before
    // the daemon starts so the daemon that starts routes through it.
    if let Some(account) = plan::session_record(c.steps) {
        record.accounts.faber_mac = account;
    }
    let json = record.to_json()?;
    let path = &c.s.layout.setup_record;
    let (parent, name) = fsx::split(path)?;
    let dir = fsx::open_leaf_dir(parent)?;
    let before = read_existing(&dir, name)?;
    dir.write_atomic(name, &mut json.as_bytes(), ROOT, 0o644)?;
    p.record_before = Some(before);
    // Read it back through the trust rule, exactly as the daemon will.
    match record::load_trusted(path) {
        Ok(Some(back)) if back == record => {
            println!("  wrote {}", path.display());
            Ok(())
        }
        Ok(_) => bail!("{} did not read back as written", path.display()),
        Err(e) => bail!("{} {e}: the daemon would not trust it", path.display()),
    }
}

fn install_service(c: &Ctx<'_>, runner: &mut dyn Runner, p: &mut Progress) -> Result<()> {
    let s = c.s;
    let unit = &s.layout.service_unit;
    let rendered = service::render(s.os, &s.layout);
    if s.service.installed.as_deref() != Some(rendered.as_str()) {
        let (parent, name) = fsx::split(unit)?;
        let dir = fsx::open_leaf_dir(parent)?;
        let before = read_existing(&dir, name)?;
        dir.write_atomic(name, &mut rendered.as_bytes(), ROOT, 0o644)?;
        p.unit_before = Some(before);
        println!("  wrote {}", unit.display());
    }
    p.started = true;
    match s.os {
        Os::MacOs => {
            launchd::lint(runner, unit)?;
            let target = service::system_target();
            launchd::enable(runner, &target)?;
            if !launchd::is_loaded(runner, &target) {
                launchd::bootstrap(runner, "system", unit, Duration::from_secs(2))?;
            }
            // Bootstrapping a RunAtLoad job already started it; the kickstart
            // covers a job that was loaded but stopped. The readiness check
            // below is what decides, so a complaint here is only reported.
            if let Err(e) = launchd::kickstart(runner, &target, false) {
                println!("  (kickstart: {e:#})");
            }
            println!("  started {target}");
        }
        Os::Linux => {
            systemd::daemon_reload(runner)?;
            systemd::enable_now(runner)?;
            println!("  started {}", service::UNIT_NAME);
        }
    }
    Ok(())
}

/// Stop the SYSTEM service: `bootout` and wait for the label, or `stop`.
fn stop_service(os: Os, runner: &mut dyn Runner) -> Result<()> {
    match os {
        Os::MacOs => {
            let target = service::system_target();
            launchd::bootout(runner, &target)?;
            launchd::wait_until_gone(runner, &target, LABEL_WAIT, Duration::from_secs(1))
        }
        Os::Linux => systemd::stop(runner),
    }
}

async fn wait_ready(s: &HostState) -> Result<()> {
    let deadline = Instant::now() + READY_LIMIT;
    loop {
        if api_answers(&system_socket(), Fallback::Never).await {
            println!("  the daemon answers on its control socket");
            return Ok(());
        }
        if Instant::now() >= deadline {
            let logs = match s.os {
                Os::MacOs => format!("{}/stderr.log", s.layout.log_dir.display()),
                Os::Linux => format!("`journalctl -u {}`", service::UNIT_NAME),
            };
            bail!(
                "the daemon did not answer on its control socket within {}s; see {logs}",
                READY_LIMIT.as_secs()
            );
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Put `path` (a root-owned file) back to `before`: its previous bytes, or
/// gone.
fn restore_file(path: &Path, before: Option<&[u8]>, mode: u32) -> Result<()> {
    let (parent, name) = fsx::split(path)?;
    let dir = fsx::open_leaf_dir(parent)?;
    match before {
        Some(bytes) => {
            dir.write_atomic(name, &mut &bytes[..], ROOT, mode)?;
        }
        None => match dir.unlink(name) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        },
    }
    Ok(())
}

/// Remove a file this run created, through the descriptor of the directory
/// it was created in, and put back what `--remigrate` moved aside for it.
fn undo_created(created: &Created) -> Result<()> {
    match created.dir.unlink(&created.name) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    if let Some(aside) = &created.moved_aside {
        created.dir.rename(aside, &created.name)?;
    }
    Ok(())
}

/// Undo phases 2 to 4, best effort, newest first. Returns what it did, for
/// the error the operator sees.
fn rollback(c: &Ctx<'_>, runner: &mut dyn Runner, p: &Progress) -> String {
    let s = c.s;
    let mut done: Vec<String> = Vec::new();
    let mut note = |what: String, result: Result<()>| match result {
        Ok(()) => done.push(what),
        Err(e) => done.push(format!("{what}: FAILED ({e:#})")),
    };
    if p.started {
        let stopped = match s.os {
            Os::MacOs => {
                let target = service::system_target();
                launchd::bootout(runner, &target).and_then(|()| {
                    launchd::wait_until_gone(runner, &target, LABEL_WAIT, Duration::from_secs(1))
                })
            }
            Os::Linux => systemd::disable_now(runner),
        };
        note("stopped the new service".into(), stopped);
    }
    if let Some(swap) = &p.daemon {
        note(
            format!(
                "put the previous {} back",
                swap.dir.path().join(&swap.name).display()
            ),
            restore_daemon(swap),
        );
    }
    if let Some(before) = &p.unit_before {
        note(
            format!("restored {}", s.layout.service_unit.display()),
            restore_file(&s.layout.service_unit, before.as_deref(), 0o644),
        );
        if s.os == Os::Linux {
            let _ = systemd::daemon_reload(runner);
        }
    }
    if let Some(before) = &p.record_before {
        note(
            format!("restored {}", s.layout.setup_record.display()),
            restore_file(&s.layout.setup_record, before.as_deref(), 0o644),
        );
    }
    for created in p.created.iter().rev() {
        note(
            format!(
                "removed {}",
                created.dir.path().join(&created.name).display()
            ),
            undo_created(created),
        );
    }
    for stopped in p.stopped.iter().rev() {
        note(
            format!("put {}'s per-user faber back", stopped.account),
            old::restore(s, stopped, runner),
        );
    }
    for d in p.draining.iter().rev() {
        note(
            format!("restarted {}'s drained per-user faber", d.account),
            old::revive(s, &d.account, d.uid, d.definition.as_deref(), runner),
        );
    }
    if p.system_drained && !p.system_stopped {
        let restarted = match s.os {
            Os::MacOs => {
                let target = service::system_target();
                if launchd::is_loaded(runner, &target) {
                    launchd::kickstart(runner, &target, true)
                } else {
                    launchd::bootstrap(
                        runner,
                        "system",
                        &s.layout.service_unit,
                        Duration::from_secs(2),
                    )
                }
            }
            Os::Linux => systemd::restart(runner),
        };
        note(
            "restarted the drained daemon, so it claims jobs again".into(),
            restarted,
        );
    }
    if p.system_stopped {
        let restarted = match s.os {
            Os::MacOs => launchd::bootstrap(
                runner,
                "system",
                &s.layout.service_unit,
                Duration::from_secs(2),
            )
            .and_then(|()| launchd::kickstart(runner, &service::system_target(), false)),
            Os::Linux => systemd::enable_now(runner),
        };
        note(
            "restarted the service that was running before".into(),
            restarted,
        );
    }
    if done.is_empty() {
        "setup failed before it changed anything that runs".into()
    } else {
        format!("setup failed and rolled back:\n  {}", done.join("\n  "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::fsx::tests::{me, Scratch};
    use std::os::unix::fs::symlink;

    /// Fix for a review finding: rollback removes what the run created through
    /// the descriptor it created it with. A directory swapped for a symlink in
    /// the meantime redirects nothing.
    #[test]
    fn rollback_never_follows_a_swapped_directory() {
        let s = Scratch::new("undo");
        let owner = me(&s);
        std::fs::create_dir(s.path("update")).unwrap();
        let dir = Dir::open_nofollow(&s.path("update")).unwrap();
        dir.write_atomic("quarantine.json", &mut &b"[]"[..], owner, 0o640)
            .unwrap();
        // Something rearranges the tree: the directory moves, and a symlink
        // to a victim holding a file of the same name takes its place.
        std::fs::rename(s.path("update"), s.path("moved")).unwrap();
        std::fs::create_dir(s.path("victim")).unwrap();
        std::fs::write(s.path("victim/quarantine.json"), b"victim").unwrap();
        symlink(s.path("victim"), s.path("update")).unwrap();

        undo_created(&Created {
            dir,
            name: "quarantine.json".into(),
            moved_aside: None,
        })
        .unwrap();
        assert_eq!(
            std::fs::read(s.path("victim/quarantine.json")).unwrap(),
            b"victim"
        );
        assert!(!s.path("moved/quarantine.json").exists());
    }

    #[test]
    fn undo_puts_back_what_remigrate_moved_aside() {
        let s = Scratch::new("undo-aside");
        std::fs::write(s.path("config.toml"), b"new").unwrap();
        std::fs::write(s.path("config.toml.pre-remigrate-T"), b"old").unwrap();
        undo_created(&Created {
            dir: s.dir(),
            name: "config.toml".into(),
            moved_aside: Some("config.toml.pre-remigrate-T".into()),
        })
        .unwrap();
        assert_eq!(std::fs::read(s.path("config.toml")).unwrap(), b"old");
        assert!(!s.path("config.toml.pre-remigrate-T").exists());
    }

    /// Fix for a review finding: the daemon binary setup replaces is kept
    /// aside until the new one answers, and rollback puts it back.
    #[test]
    fn a_replaced_daemon_binary_is_restored_on_rollback() {
        let s = Scratch::new("swap");
        let owner = me(&s);
        std::fs::create_dir(s.path("bin")).unwrap();
        std::fs::write(s.path("bin/faber"), b"old").unwrap();
        let swap = begin_swap(
            Dir::open_nofollow(&s.path("bin")).unwrap(),
            "faber",
            "20260928T120000Z",
        )
        .unwrap();
        assert_eq!(
            swap.backup.as_deref(),
            Some("faber.pre-setup-20260928T120000Z")
        );
        swap.dir
            .write_atomic("faber", &mut &b"new"[..], owner, 0o755)
            .unwrap();
        restore_daemon(&swap).unwrap();
        assert_eq!(std::fs::read(s.path("bin/faber")).unwrap(), b"old");
        assert!(!s.path("bin/faber.pre-setup-20260928T120000Z").exists());

        // With nothing there before, rollback removes the new binary.
        std::fs::remove_file(s.path("bin/faber")).unwrap();
        let swap = begin_swap(Dir::open_nofollow(&s.path("bin")).unwrap(), "faber", "T2").unwrap();
        assert_eq!(swap.backup, None);
        swap.dir
            .write_atomic("faber", &mut &b"new"[..], owner, 0o755)
            .unwrap();
        restore_daemon(&swap).unwrap();
        assert!(!s.path("bin/faber").exists());
    }

    /// Fix for a review finding: a binary swapped after the plan hashed it
    /// (during the "Apply this plan?" prompt) is refused before it lands. The
    /// CLI copy has no rollback, so an existing copy stays as it was and a
    /// missing one stays missing; the daemon copy's swap is put back as usual.
    #[test]
    fn a_binary_swapped_after_the_plan_is_installed_nowhere() {
        let s = Scratch::new("cli-sha");
        let owner = me(&s);
        std::fs::write(s.path("faber-src"), b"swapped during the prompt").unwrap();
        std::fs::create_dir(s.path("bin")).unwrap();
        let mut host = crate::setup::plan::tests::fresh_mac();
        host.exe_path = s.path("faber-src");
        host.exe_sha256 = Some(fsx::sha256_of(&mut &b"the binary the plan hashed"[..]).unwrap());
        let op = host.operator.clone().unwrap();
        let opts = SetupOpts {
            verbose: false,
            dry_run: false,
            yes: true,
            keep_awake: false,
            drain_timeout: Duration::from_secs(1),
            replace_daemon: false,
            remigrate: false,
            migrate_from: None,
            fresh: false,
            keep_legacy: false,
            vm: Default::default(),
            before_vm_runtime: false,
            check_vm_runtime: false,
            session: Default::default(),
        };
        let entries = || {
            let mut names: Vec<String> = std::fs::read_dir(s.path("bin"))
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };
        let install = |host: &HostState| {
            let c = Ctx {
                s: host,
                steps: &[],
                opts: &opts,
                op: &op,
                faber: owner,
                vm: owner,
                operator_gid: owner.gid,
                ts: "T",
                autoupdate: false,
            };
            let dir = Dir::open_nofollow(&s.path("bin")).unwrap();
            install_binary(&c, &dir, "faber", owner)
        };

        // Nothing there before: nothing there after, no temp file either.
        let err = format!("{:#}", install(&host).unwrap_err());
        assert!(err.contains("changed after the plan was made"), "{err}");
        assert!(err.contains("nothing was installed"), "{err}");
        assert!(entries().is_empty(), "{:?}", entries());

        // An existing copy is left exactly as it was.
        std::fs::write(s.path("bin/faber"), b"the old faber").unwrap();
        install(&host).unwrap_err();
        assert_eq!(
            std::fs::read(s.path("bin/faber")).unwrap(),
            b"the old faber"
        );
        assert_eq!(entries(), vec!["faber".to_string()]);

        // The daemon copy: its swap moved the old one aside, and rollback
        // finds nothing new to remove and puts the old one back.
        let swap = begin_swap(Dir::open_nofollow(&s.path("bin")).unwrap(), "faber", "T").unwrap();
        install(&host).unwrap_err();
        assert_eq!(entries(), vec!["faber.pre-setup-T".to_string()]);
        restore_daemon(&swap).unwrap();
        assert_eq!(
            std::fs::read(s.path("bin/faber")).unwrap(),
            b"the old faber"
        );

        // The bytes the plan hashed install as before.
        host.exe_sha256 = Some(fsx::sha256_of(&mut &b"swapped during the prompt"[..]).unwrap());
        install(&host).unwrap();
        assert_eq!(
            std::fs::read(s.path("bin/faber")).unwrap(),
            b"swapped during the prompt"
        );
        assert_eq!(entries(), vec!["faber".to_string()]);
    }

    /// A symlink planted where the daemon binary goes is moved aside like a
    /// file: the rename never follows it, and its target is untouched.
    #[test]
    fn a_planted_symlink_at_the_daemon_path_is_moved_not_followed() {
        let s = Scratch::new("swap-link");
        let owner = me(&s);
        std::fs::create_dir(s.path("bin")).unwrap();
        std::fs::write(s.path("target"), b"not faber").unwrap();
        symlink(s.path("target"), s.path("bin/faber")).unwrap();
        let swap = begin_swap(Dir::open_nofollow(&s.path("bin")).unwrap(), "faber", "T").unwrap();
        swap.dir
            .write_atomic("faber", &mut &b"new"[..], owner, 0o755)
            .unwrap();
        assert_eq!(std::fs::read(s.path("target")).unwrap(), b"not faber");
        assert!(std::fs::symlink_metadata(s.path("bin/faber"))
            .unwrap()
            .file_type()
            .is_file());
    }
}
