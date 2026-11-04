//! The service manager faber runs under (ADR-0025): a LaunchDaemon in launchd's
//! `system` domain on macOS, a systemd system unit on Linux. Either way the
//! daemon starts at boot with nobody logged in, as faber's own role account,
//! `_faber`, and never as a person.
//!
//! This is a *behaviour* contract rather than a thin wrapper over `launchctl`
//! and `systemctl`. faber depends on what the manager does, and four of those
//! dependencies are load-bearing in ways that are easy to get wrong:
//!
//! 1. **Restart on *any* exit, including `exit(0)`.** The self-updater
//!    ([`crate::update`], ADR-0006) swaps the binary and then simply exits,
//!    relying on the manager to start the new one. systemd's `Restart=always`
//!    reproduces launchd's `KeepAlive`; `Restart=on-failure` does **not**, and
//!    would strand every host on the version it was installed with.
//!
//! 2. **A stop grace period at least as long as the longest job.** `ExitTimeOut`
//!    (600s in the plist) is the deadline the drain is designed around
//!    ([`crate::reconcile`]); exceeding it force-kills mid-drain and orphans
//!    runners on GitHub. systemd's analogue is `TimeoutStopSec`.
//!
//! 3. **The stop signal must reach faber and not its children.** faber keeps one
//!    `docker wait` child per in-flight runner ([`crate::docker`]), and manages
//!    their lifetime itself across a drain. systemd's default
//!    `KillMode=control-group` signals the whole cgroup, taking those watchers
//!    out from under the drain that is still using them; `KillMode=process`
//!    matches what faber expects. The plist sets no `AbandonProcessGroup`, for
//!    the same reason: whether the watchers outlive the daemon is ADR-0026's
//!    decision, not this one's. The runner containers themselves are started
//!    detached (`docker run -d`), so they belong to dockerd either way.
//!
//! 4. **The environment is the unit's, and holds nothing an operator can
//!    write.** `HOME` is the SYSTEM state directory, so the process and
//!    [`crate::platform`] agree on it, and `PATH` is `/usr/bin:/bin:/usr/sbin:
//!    /sbin`: no Homebrew prefix and no `/usr/local/bin`, where a tool can be a
//!    link into an operator-owned app bundle. A tool faber needs from anywhere
//!    else comes from a root-owned directory ADR-0026 adds, never from `PATH`.
//!
//! Stopping the unit is only ever a bare signal. The real decommission path is
//! out-of-band and stays that way: `faber drain` / `POST /api/drain` runs first
//! and waits for in-flight work, and only then is the unit stopped. `sudo faber
//! setup` and `sudo faber uninstall` both drain before they stop anything.
//!
//! # What lives here
//!
//! - the two definitions, rendered from `deploy/` templates by pure functions
//!   compiled on every OS, so the macOS CI leg tests the systemd unit too
//! - the manager commands setup and uninstall run, through
//!   [`crate::setup::tools::Runner`], by absolute path, so no test runs
//!   `launchctl` or `systemctl`
//!
//! The *legacy* per-user service (a LaunchAgent in `gui/<uid>`, or a `systemd
//! --user` unit) is no longer installed by anything. Setup still has to find,
//! drain, stop and back up an existing one, and on a failed migration restore
//! it, which is what the `gui`/per-user helpers below are for.

use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Result};

use super::layout::{Os, SystemLayout, FABER_ACCOUNT, FABER_OPERATOR_GROUP};
use crate::setup::tools::{Cmd, Runner, Tool};

/// faber's launchd label, and the file name of both its LaunchDaemon plist
/// and the legacy LaunchAgent's. It must match the `<key>Label</key>` literal
/// in `deploy/io.github.rejifald.faber.plist`; a test checks the two agree.
pub const LABEL: &str = "io.github.rejifald.faber";

/// The systemd unit's name, on Linux both for the system unit and for the
/// legacy per-user one.
pub const UNIT_NAME: &str = "faber.service";

/// How long the manager waits for a graceful stop before SIGKILL: the plist's
/// `ExitTimeOut` and the unit's `TimeoutStopSec`, both literals in `deploy/`.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(600);

const PLIST_TEMPLATE: &str = include_str!("../../deploy/io.github.rejifald.faber.plist");
const UNIT_TEMPLATE: &str = include_str!("../../deploy/faber.service");

/// launchd's target for the LaunchDaemon: `system/<label>`.
pub fn system_target() -> String {
    format!("system/{LABEL}")
}

/// launchd's target for a legacy LaunchAgent: `gui/<uid>/<label>`.
pub fn gui_target(uid: u32) -> String {
    format!("gui/{uid}/{LABEL}")
}

/// `s` with the five XML special characters escaped, for a plist `<string>`.
fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// The LaunchDaemon plist for `layout`. Pure.
pub fn render_plist(layout: &SystemLayout) -> String {
    let path = |p: &Path| xml_escape(&p.display().to_string());
    PLIST_TEMPLATE
        .replace("{{USER}}", &xml_escape(FABER_ACCOUNT))
        .replace("{{GROUP}}", &xml_escape(FABER_ACCOUNT))
        .replace("{{FABER_BIN}}", &path(&layout.daemon_bin))
        .replace("{{HOME}}", &path(&layout.state_dir))
        .replace("{{LOG_DIR}}", &path(&layout.log_dir))
}

// --- the VM LaunchDaemons (ADR-0026) -----------------------------------------
//
// Two more jobs in the `system` domain, macOS only, both installed by `sudo
// faber setup` ([`crate::setup::vm_runtime`]) and removed by uninstall:
//
// - the wall's anchor checker, as root, running the root-owned CLI copy
//   (`/usr/local/bin/faber wall-daemon`): the one root binary, which only
//   `sudo faber upgrade-cli` changes (ADR-0034);
// - the VM supervisor, as `_faber_vm`, running the launcher `_faber` owns and
//   its updater swaps. `AbandonProcessGroup` keeps launchd from killing the
//   per-VM launcher processes when the supervisor exits, which is how the
//   supervisor restarts on a new binary while running VMs finish on the old
//   one (ADR-0026, "The supervisor updates without stopping a VM").
//
// Both log to files in `/var/log`, which is root's alone. launchd opens
// `StandardOutPath` and `StandardErrorPath` as root, so in a directory
// `_faber` or `_faber_vm` could write, either could plant a symlink there and
// have launchd open any file on the host for writing.

/// The anchor checker's launchd label.
pub const WALL_LABEL: &str = "io.github.rejifald.faber.wall";
/// The anchor checker's LaunchDaemon plist.
pub const WALL_PLIST: &str = "/Library/LaunchDaemons/io.github.rejifald.faber.wall.plist";
/// Its log, in root's `/var/log`.
pub const WALL_LOG: &str = "/var/log/faber-wall.log";

/// The VM supervisor's launchd label.
pub const SUPERVISOR_LABEL: &str = "io.github.rejifald.faber.vm-supervisor";
/// The VM supervisor's LaunchDaemon plist.
pub const SUPERVISOR_PLIST: &str =
    "/Library/LaunchDaemons/io.github.rejifald.faber.vm-supervisor.plist";
/// Its log, in root's `/var/log`.
pub const SUPERVISOR_LOG: &str = "/var/log/faber-vm-supervisor.log";

/// launchd's target for a job in the `system` domain: `system/<label>`.
pub fn system_target_of(label: &str) -> String {
    format!("system/{label}")
}

/// One LaunchDaemon, as data: what [`render_daemon_plist`] writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonPlist {
    pub label: String,
    /// `None`: launchd's default, root.
    pub user: Option<String>,
    pub program: Vec<String>,
    /// `AbandonProcessGroup`: the job's children outlive it.
    pub abandon_process_group: bool,
    pub home: Option<String>,
    pub log: String,
    /// The `Umask` key, as the decimal launchd reads.
    pub umask: u32,
}

/// A LaunchDaemon plist: started at boot and kept alive, a fixed `PATH`, `/`
/// as its directory, both streams to `log`. Every string is escaped. Pure.
pub fn render_daemon_plist(d: &DaemonPlist) -> String {
    let string = |s: &str| format!("<string>{}</string>", xml_escape(s));
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <!-- Written by `sudo faber setup` (ADR-0026); `sudo faber uninstall` removes it. -->\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    let key = |out: &mut String, k: &str, v: String| {
        out.push_str(&format!("    <key>{k}</key>\n    {v}\n"));
    };
    key(&mut out, "Label", string(&d.label));
    if let Some(user) = &d.user {
        key(&mut out, "UserName", string(user));
        key(&mut out, "GroupName", string(user));
    }
    let args: String = d
        .program
        .iter()
        .map(|a| format!("        {}\n", string(a)))
        .collect();
    key(
        &mut out,
        "ProgramArguments",
        format!("<array>\n{args}    </array>"),
    );
    key(&mut out, "RunAtLoad", "<true/>".into());
    key(&mut out, "KeepAlive", "<true/>".into());
    if d.abandon_process_group {
        key(&mut out, "AbandonProcessGroup", "<true/>".into());
    }
    let mut env = String::from("<dict>\n");
    if let Some(home) = &d.home {
        env.push_str(&format!(
            "        <key>HOME</key>\n        {}\n",
            string(home)
        ));
    }
    env.push_str(&format!(
        "        <key>PATH</key>\n        {}\n    </dict>",
        string("/usr/bin:/bin:/usr/sbin:/sbin")
    ));
    key(&mut out, "EnvironmentVariables", env);
    key(&mut out, "WorkingDirectory", string("/"));
    key(&mut out, "Umask", format!("<integer>{}</integer>", d.umask));
    key(&mut out, "StandardOutPath", string(&d.log));
    key(&mut out, "StandardErrorPath", string(&d.log));
    key(&mut out, "ProcessType", string("Standard"));
    out.push_str("</dict>\n</plist>\n");
    out
}

/// The anchor checker's LaunchDaemon: root runs the root-owned CLI copy as
/// `faber wall-daemon`, whose paths are fixed in the binary
/// ([`crate::wall::cli::run_daemon`]). Pure.
pub fn render_wall_plist(layout: &SystemLayout) -> String {
    render_daemon_plist(&DaemonPlist {
        label: WALL_LABEL.into(),
        user: None,
        program: vec![layout.cli_bin.display().to_string(), "wall-daemon".into()],
        abandon_process_group: false,
        home: None,
        log: WALL_LOG.into(),
        umask: 0o022,
    })
}

/// What the supervisor is started with (launcher/README.md, § The
/// supervisor).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupervisorArgs {
    /// `_faber`'s uid: the only peer the supervisor serves.
    pub peer_uid: u32,
    /// `--image-dir`: the image store, root's: golden macOS images and the
    /// Linux image, each at `<image-dir>/<name>`.
    pub image_dir: std::path::PathBuf,
    /// `--tools-dir`: the tools-disk cache, `_faber`'s.
    pub tools_dir: std::path::PathBuf,
    /// `--network-plan`, canonical: the plan setup writes.
    pub network_plan: std::path::PathBuf,
}

/// The supervisor's argument vector, from `layout` and `args`. Its own
/// directories are all in `_faber_vm`'s home. Pure.
pub fn supervisor_argv(layout: &SystemLayout, args: &SupervisorArgs) -> Vec<String> {
    let socket = layout.vm_home.join("run/supervisor.sock");
    let mut argv = supervisor_argv_in(layout, &layout.vm_home, &socket, args);
    argv.extend([
        "--linux-dir".into(),
        layout.vm_home.join("linux").display().to_string(),
    ]);
    argv
}

/// The supervisor's arguments with its own directories in `home`, and no
/// Linux VM: what both supervisors share.
fn supervisor_argv_in(
    layout: &SystemLayout,
    home: &Path,
    socket: &Path,
    args: &SupervisorArgs,
) -> Vec<String> {
    let p = |path: &Path| path.display().to_string();
    vec![
        p(&layout.launcher_bin),
        "supervise".into(),
        "--socket".into(),
        p(socket),
        "--peer-uid".into(),
        args.peer_uid.to_string(),
        "--state-dir".into(),
        p(&home.join("state")),
        "--image-dir".into(),
        p(&args.image_dir),
        "--tools-dir".into(),
        p(&args.tools_dir),
        "--clone-dir".into(),
        p(&home.join("clones")),
        "--network-plan".into(),
        p(&args.network_plan),
    ]
}

/// ADR-0051's macOS VM supervisor, as the session account's LaunchAgent will
/// run it: the launcher's `supervise` with its directories in
/// [`FABER_MAC_HOME`] and no Linux VM, which stays `_faber_vm`'s. Pure.
///
/// [`FABER_MAC_HOME`]: super::layout::FABER_MAC_HOME
pub fn session_supervisor_argv(layout: &SystemLayout, args: &SupervisorArgs) -> Vec<String> {
    supervisor_argv_in(
        layout,
        Path::new(super::layout::FABER_MAC_HOME),
        &super::session_supervisor_socket(),
        args,
    )
}

/// The session account's LaunchAgent label (ADR-0051).
pub const SESSION_AGENT_LABEL: &str = "io.github.rejifald.faber.mac-vm-supervisor";

/// The session account's LaunchAgent: loaded only in its GUI (`Aqua`)
/// session, started at login and kept alive, its VMs outliving it like the
/// system supervisor's. The daemon reaches it at
/// [`super::session_supervisor_socket`] once the setup record names the
/// account, and runs every macOS VM job through it. Every string is escaped.
/// Pure.
pub fn render_session_agent_plist(program: &[String], log: &str) -> String {
    let string = |s: &str| format!("<string>{}</string>", xml_escape(s));
    let mut out = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <!-- Written by `sudo faber setup` (ADR-0051); `sudo faber uninstall` removes it. -->\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    let key = |out: &mut String, k: &str, v: String| {
        out.push_str(&format!("    <key>{k}</key>\n    {v}\n"));
    };
    key(&mut out, "Label", string(SESSION_AGENT_LABEL));
    let args: String = program
        .iter()
        .map(|a| format!("        {}\n", string(a)))
        .collect();
    key(
        &mut out,
        "ProgramArguments",
        format!("<array>\n{args}    </array>"),
    );
    key(&mut out, "LimitLoadToSessionType", string("Aqua"));
    key(&mut out, "RunAtLoad", "<true/>".into());
    key(&mut out, "KeepAlive", "<true/>".into());
    key(&mut out, "AbandonProcessGroup", "<true/>".into());
    key(
        &mut out,
        "EnvironmentVariables",
        format!(
            "<dict>\n        <key>PATH</key>\n        {}\n    </dict>",
            string("/usr/bin:/bin:/usr/sbin:/sbin")
        ),
    );
    key(&mut out, "WorkingDirectory", string("/"));
    key(&mut out, "Umask", format!("<integer>{}</integer>", 0o027));
    key(&mut out, "StandardOutPath", string(log));
    key(&mut out, "StandardErrorPath", string(log));
    out.push_str("</dict>\n</plist>\n");
    out
}

/// The VM supervisor's LaunchDaemon: `_faber_vm` runs the launcher's
/// `supervise`, and its per-VM children outlive it. Pure.
pub fn render_supervisor_plist(layout: &SystemLayout, args: &SupervisorArgs) -> String {
    render_daemon_plist(&DaemonPlist {
        label: SUPERVISOR_LABEL.into(),
        user: Some(super::layout::FABER_VM_ACCOUNT.into()),
        program: supervisor_argv(layout, args),
        abandon_process_group: true,
        home: Some(layout.vm_home.display().to_string()),
        log: SUPERVISOR_LOG.into(),
        umask: 0o027,
    })
}

/// The systemd system unit for `layout`. Pure. The daemon's primary group is
/// the operator group, so what it writes is the operator's to read; `_faber`'s
/// own, which reads the token, is supplementary.
pub fn render_unit(layout: &SystemLayout) -> String {
    UNIT_TEMPLATE
        .replace("{{LABEL}}", LABEL)
        .replace("{{USER}}", FABER_ACCOUNT)
        .replace("{{GROUP}}", FABER_OPERATOR_GROUP)
        .replace("{{TOKEN_GROUP}}", FABER_ACCOUNT)
        .replace("{{FABER_BIN}}", &layout.daemon_bin.display().to_string())
        .replace("{{HOME}}", &layout.state_dir.display().to_string())
}

/// The service definition `os` installs at `layout.service_unit`.
pub fn render(os: Os, layout: &SystemLayout) -> String {
    match os {
        Os::MacOs => render_plist(layout),
        Os::Linux => render_unit(layout),
    }
}

/// How an operator restarts the daemon by hand.
pub fn restart_hint(os: Os) -> String {
    match os {
        Os::MacOs => format!("sudo launchctl kickstart -k {}", system_target()),
        Os::Linux => format!("sudo systemctl restart {UNIT_NAME}"),
    }
}

/// launchd, through `/bin/launchctl`.
pub mod launchd {
    use super::*;

    fn launchctl<I, S>(args: I) -> Cmd
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Cmd::new(Tool::Launchctl, args)
    }

    /// Whether `target` (`system/<label>`, `gui/<uid>/<label>`) is loaded.
    pub fn is_loaded(runner: &mut dyn Runner, target: &str) -> bool {
        runner
            .run(&launchctl(["print", target]))
            .is_ok_and(|out| out.success)
    }

    /// Ask launchd to tear `target` down. A job that was never loaded makes
    /// `bootout` fail, and "not loaded" is the state being asked for, so only
    /// failing to run launchctl at all is an error.
    pub fn bootout(runner: &mut dyn Runner, target: &str) -> Result<()> {
        runner.run(&launchctl(["bootout", target]))?;
        Ok(())
    }

    /// `bootout` returns once it has *asked* launchd to tear the job down, not
    /// once the label is free, so a `bootstrap` right after it races and
    /// fails with the opaque `Bootstrap failed: 5: Input/output error`. Wait
    /// until `target` stops printing, polling every `poll`, for at most
    /// `limit`: a little above `ExitTimeOut`, past which launchd has
    /// SIGKILLed the job anyway.
    pub fn wait_until_gone(
        runner: &mut dyn Runner,
        target: &str,
        limit: Duration,
        poll: Duration,
    ) -> Result<()> {
        let deadline = std::time::Instant::now() + limit;
        let mut said = false;
        while is_loaded(runner, target) {
            if std::time::Instant::now() >= deadline {
                bail!("{target} is still loaded after {limit:?}");
            }
            if !said {
                println!("    waiting for {target} to exit (it drains in-flight work first)");
                said = true;
            }
            std::thread::sleep(poll);
        }
        Ok(())
    }

    /// Bootstrap `plist` into `domain` (`system`, `gui/<uid>`), retrying: the
    /// label can still be settling for a moment after it stops printing. The
    /// last attempt's error is the one reported.
    pub fn bootstrap(
        runner: &mut dyn Runner,
        domain: &str,
        plist: &Path,
        pause: Duration,
    ) -> Result<()> {
        let cmd = launchctl([
            "bootstrap".to_string(),
            domain.to_string(),
            plist.display().to_string(),
        ]);
        for attempt in 1..=5 {
            match runner.run_ok(&cmd) {
                Ok(_) => return Ok(()),
                Err(e) if attempt == 5 => return Err(e),
                Err(_) => std::thread::sleep(pause),
            }
        }
        unreachable!("the loop returns on its fifth attempt")
    }

    /// Load `plist` into `domain` afresh and start it: boot `target` out if
    /// it is loaded, wait for its label to clear, then bootstrap it again.
    ///
    /// This, not `kickstart -k`, restarts a job onto a binary that was just
    /// replaced. launchd pins a loaded job's launch constraint (its LWCR) to
    /// the code it saw at bootstrap, and an ad-hoc-signed build is pinned by
    /// cdhash. A `kickstart -k` onto a different binary then fails at every
    /// spawn, forever: `last exit code = 78: EX_CONFIG`, and "Unable to get
    /// updated LWCR" in launchd's log (#479). Only a fresh bootstrap
    /// registers the new code.
    pub fn reload(
        runner: &mut dyn Runner,
        domain: &str,
        target: &str,
        plist: &Path,
        limit: Duration,
        pause: Duration,
    ) -> Result<()> {
        if is_loaded(runner, target) {
            bootout(runner, target)?;
            wait_until_gone(runner, target, limit, pause)?;
        }
        bootstrap(runner, domain, plist, pause)?;
        // Bootstrapping a RunAtLoad job already started it; this covers one
        // that is not.
        kickstart(runner, target, false)
    }

    /// What `launchctl print <target>` says of the job itself: its `state`
    /// and `last exit code`. Both are the first lines of that name, which are
    /// the job's own; the nested coalitions that follow print a `state` too.
    pub fn print_status(stdout: &str) -> (Option<&str>, Option<&str>) {
        let field = |name: &str| {
            stdout
                .lines()
                .find_map(|l| l.trim().strip_prefix(name)?.strip_prefix(" = "))
                .map(str::trim)
        };
        (field("state"), field("last exit code"))
    }

    /// Wait, at most `limit`, polling every `poll`, until `target` runs. A
    /// KeepAlive job that launchd cannot spawn stays loaded and keeps
    /// retrying, so neither `bootstrap` nor `kickstart` failing is how such a
    /// job shows: this is.
    pub fn wait_running(
        runner: &mut dyn Runner,
        target: &str,
        limit: Duration,
        poll: Duration,
    ) -> Result<()> {
        let deadline = std::time::Instant::now() + limit;
        loop {
            let out = runner.run(&launchctl(["print", target]))?;
            let (state, last_exit) = print_status(&out.stdout);
            if out.success && state == Some("running") {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                bail!(
                    "{target} did not start within {limit:?} (state {}, last exit code {}); \
                     launchd says why in `/usr/bin/log show --last 5m --predicate 'process == \
                     \"launchd\"'`",
                    state.unwrap_or("unknown"),
                    last_exit.unwrap_or("unknown"),
                );
            }
            std::thread::sleep(poll);
        }
    }

    /// Clear any `disable` override for `target`. A disabled job cannot be
    /// bootstrapped, so this runs before `bootstrap`.
    pub fn enable(runner: &mut dyn Runner, target: &str) -> Result<()> {
        runner.run_ok(&launchctl(["enable", target]))?;
        Ok(())
    }

    /// Start `target` if it is not running; with `restart`, kill and start it.
    pub fn kickstart(runner: &mut dyn Runner, target: &str, restart: bool) -> Result<()> {
        let cmd = if restart {
            launchctl(["kickstart", "-k", target])
        } else {
            launchctl(["kickstart", target])
        };
        runner.run_ok(&cmd)?;
        Ok(())
    }

    /// `plutil -lint`: launchd silently ignores a malformed plist.
    pub fn lint(runner: &mut dyn Runner, plist: &Path) -> Result<()> {
        runner.run_ok(&Cmd::new(
            Tool::Plutil,
            ["-lint".to_string(), plist.display().to_string()],
        ))?;
        Ok(())
    }
}

/// systemd, through `/usr/bin/systemctl`.
pub mod systemd {
    use super::*;

    fn systemctl<I, S>(args: I) -> Cmd
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Cmd::new(Tool::Systemctl, args)
    }

    pub fn daemon_reload(runner: &mut dyn Runner) -> Result<()> {
        runner.run_ok(&systemctl(["daemon-reload"]))?;
        Ok(())
    }

    /// Enable at boot and start now.
    pub fn enable_now(runner: &mut dyn Runner) -> Result<()> {
        runner.run_ok(&systemctl(["enable", "--now", UNIT_NAME]))?;
        Ok(())
    }

    /// Stop now and no longer start at boot.
    pub fn disable_now(runner: &mut dyn Runner) -> Result<()> {
        runner.run_ok(&systemctl(["disable", "--now", UNIT_NAME]))?;
        Ok(())
    }

    /// Restart now: the only way out of a drain.
    pub fn restart(runner: &mut dyn Runner) -> Result<()> {
        runner.run_ok(&systemctl(["restart", UNIT_NAME]))?;
        Ok(())
    }

    /// Stop now; leave the boot-time enablement alone.
    pub fn stop(runner: &mut dyn Runner) -> Result<()> {
        runner.run_ok(&systemctl(["stop", UNIT_NAME]))?;
        Ok(())
    }

    pub fn is_active(runner: &mut dyn Runner) -> bool {
        runner
            .run(&systemctl(["is-active", "--quiet", UNIT_NAME]))
            .is_ok_and(|out| out.success)
    }

    pub fn is_enabled(runner: &mut dyn Runner) -> bool {
        runner
            .run(&systemctl(["is-enabled", "--quiet", UNIT_NAME]))
            .is_ok_and(|out| out.success)
    }

    /// The first line of `systemctl --version`: `systemd 252 (252.22-1)`.
    pub fn parse_version(text: &str) -> Option<u32> {
        let first = text.lines().next()?;
        let mut words = first.split_whitespace();
        (words.next()? == "systemd").then_some(())?;
        words.next()?.parse().ok()
    }

    /// Whether root can reach another user's service manager with
    /// `systemctl --user -M <user>@`, which systemd 248 added.
    pub fn user_manager_supported(runner: &mut dyn Runner) -> bool {
        runner
            .run(&systemctl(["--version"]))
            .ok()
            .filter(|out| out.success)
            .and_then(|out| parse_version(&out.stdout))
            .is_some_and(|v| v >= 248)
    }

    /// A command against `user`'s own service manager.
    pub fn user_cmd(user: &str, args: &[&str]) -> Cmd {
        let mut all = vec!["--user".to_string(), "-M".into(), format!("{user}@")];
        all.extend(args.iter().map(|a| a.to_string()));
        systemctl(all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::layout::Os;
    use crate::setup::tools::{FakeRunner, Output};

    fn mac() -> SystemLayout {
        SystemLayout::for_os(Os::MacOs)
    }

    fn linux() -> SystemLayout {
        SystemLayout::for_os(Os::Linux)
    }

    /// The value of `<key>{key}</key>`'s following `<string>` or `<integer>`.
    fn plist_value(plist: &str, key: &str) -> String {
        let at = plist
            .find(&format!("<key>{key}</key>"))
            .unwrap_or_else(|| panic!("no key {key}"));
        let rest = &plist[at..];
        let open = rest.find(['>']).unwrap();
        let rest = &rest[open + 1..];
        let start = rest.find("<string>").map(|i| (i, "<string>", "</string>"));
        let int = rest
            .find("<integer>")
            .map(|i| (i, "<integer>", "</integer>"));
        let (i, o, c) = [start, int].into_iter().flatten().min().unwrap();
        let rest = &rest[i + o.len()..];
        rest[..rest.find(c).unwrap()].to_string()
    }

    #[test]
    fn the_plist_renders_a_system_daemon_for_the_role_account() {
        let out = render_plist(&mac());
        assert!(!out.contains("{{"), "unsubstituted token left:\n{out}");
        assert!(!out.contains("}}"), "unsubstituted token left:\n{out}");
        assert_eq!(plist_value(&out, "Label"), LABEL);
        assert_eq!(plist_value(&out, "UserName"), "_faber");
        assert_eq!(plist_value(&out, "GroupName"), "_faber");
        assert_eq!(
            plist_value(&out, "WorkingDirectory"),
            "/Library/Application Support/faber"
        );
        assert_eq!(
            plist_value(&out, "HOME"),
            "/Library/Application Support/faber"
        );
        assert_eq!(plist_value(&out, "Umask"), "23");
        assert_eq!(plist_value(&out, "ExitTimeOut"), "600");
        assert_eq!(
            plist_value(&out, "StandardErrorPath"),
            "/Library/Logs/faber/stderr.log"
        );
        assert!(out.contains(
            "<string>/Library/Application Support/faber/bin/faber</string>\n        \
             <string>run</string>"
        ));
        assert!(out.contains("<key>KeepAlive</key>\n    <true/>"));
        assert!(out.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(!out.contains("AbandonProcessGroup"));
    }

    /// PATH holds nothing an operator can write: no Homebrew prefix and no
    /// /usr/local/bin (ADR-0025's root invariant).
    #[test]
    fn the_plist_path_has_nothing_operator_writable() {
        let out = render_plist(&mac());
        let path = plist_value(&out, "PATH");
        assert_eq!(path, "/usr/bin:/bin:/usr/sbin:/sbin");
        for bad in ["/usr/local", "homebrew", "/opt/"] {
            assert!(!path.contains(bad), "{path}");
        }
    }

    /// launchd is the judge of a plist's syntax, so ask it on the leg that
    /// has it. Writes a scratch file; never touches /Library.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_rendered_plist_lints() {
        let path = std::env::temp_dir().join(format!("fbr-plist-{}.plist", std::process::id()));
        std::fs::write(&path, render_plist(&mac())).unwrap();
        let out = std::process::Command::new("/usr/bin/plutil")
            .arg("-lint")
            .arg(&path)
            .output()
            .unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(
            out.status.success(),
            "plutil -lint: {}",
            String::from_utf8_lossy(&out.stdout)
        );
    }

    fn supervisor_args() -> SupervisorArgs {
        SupervisorArgs {
            peer_uid: 451,
            image_dir: "/var/db/faber/images".into(),
            tools_dir: "/var/db/faber/tools-disks".into(),
            network_plan: "/private/etc/faber/wall/plan.json".into(),
        }
    }

    /// Everything a key holds, from `<key>K</key>` to the next `<key>` or the
    /// end of the dict.
    fn plist_section(plist: &str, key: &str) -> String {
        let start = plist
            .find(&format!("<key>{key}</key>"))
            .unwrap_or_else(|| panic!("no {key} in {plist}"));
        let rest = &plist[start + key.len() + 11..];
        let end = rest.find("<key>").unwrap_or(rest.len());
        rest[..end].trim().to_string()
    }

    fn strings(section: &str) -> Vec<String> {
        section
            .split("<string>")
            .skip(1)
            .map(|s| s.split("</string>").next().unwrap().to_string())
            .collect()
    }

    /// The anchor checker runs as root (no UserName), from the root-owned
    /// CLI copy, with no arguments but `wall-daemon`, and logs in `/var/log`.
    #[test]
    fn the_wall_plist_runs_the_cli_copy_as_root() {
        let plist = render_wall_plist(&mac());
        assert_eq!(plist_value(&plist, "Label"), WALL_LABEL);
        assert!(!plist.contains("<key>UserName</key>"), "{plist}");
        assert_eq!(
            strings(&plist_section(&plist, "ProgramArguments")),
            ["/usr/local/bin/faber", "wall-daemon"]
        );
        assert_eq!(plist_section(&plist, "RunAtLoad"), "<true/>");
        assert_eq!(plist_section(&plist, "KeepAlive"), "<true/>");
        assert!(!plist.contains("AbandonProcessGroup"));
        assert_eq!(plist_value(&plist, "StandardOutPath"), WALL_LOG);
        assert_eq!(plist_value(&plist, "StandardErrorPath"), WALL_LOG);
        assert_eq!(
            system_target_of(WALL_LABEL),
            "system/io.github.rejifald.faber.wall"
        );
        assert!(WALL_PLIST.ends_with(&format!("/{WALL_LABEL}.plist")));
    }

    /// The supervisor runs as `_faber_vm`, abandons its process group, and
    /// gets exactly the README's arguments.
    #[test]
    fn the_supervisor_plist_runs_the_launcher_as_faber_vm() {
        let plist = render_supervisor_plist(&mac(), &supervisor_args());
        assert_eq!(plist_value(&plist, "Label"), SUPERVISOR_LABEL);
        assert_eq!(plist_value(&plist, "UserName"), "_faber_vm");
        assert_eq!(plist_value(&plist, "GroupName"), "_faber_vm");
        assert_eq!(plist_section(&plist, "AbandonProcessGroup"), "<true/>");
        assert_eq!(plist_section(&plist, "RunAtLoad"), "<true/>");
        assert_eq!(plist_section(&plist, "KeepAlive"), "<true/>");
        assert_eq!(
            strings(&plist_section(&plist, "ProgramArguments")),
            [
                "/var/db/faber/bin/faber-vm",
                "supervise",
                "--socket",
                "/var/db/faber-vm/run/supervisor.sock",
                "--peer-uid",
                "451",
                "--state-dir",
                "/var/db/faber-vm/state",
                "--image-dir",
                "/var/db/faber/images",
                "--tools-dir",
                "/var/db/faber/tools-disks",
                "--clone-dir",
                "/var/db/faber-vm/clones",
                "--network-plan",
                "/private/etc/faber/wall/plan.json",
                "--linux-dir",
                "/var/db/faber-vm/linux",
            ]
        );
        assert_eq!(plist_value(&plist, "StandardErrorPath"), SUPERVISOR_LOG);
        assert!(SUPERVISOR_PLIST.ends_with(&format!("/{SUPERVISOR_LABEL}.plist")));
        for log in [WALL_LOG, SUPERVISOR_LOG] {
            assert!(log.starts_with("/var/log/"), "{log}");
        }
    }

    #[test]
    fn a_daemon_plist_escapes_every_string() {
        let plist = render_daemon_plist(&DaemonPlist {
            label: "a<b".into(),
            user: Some("u&v".into()),
            program: vec!["/x\"y".into()],
            abandon_process_group: false,
            home: Some("/h'".into()),
            log: "/var/log/l>".into(),
            umask: 18,
        });
        for raw in ["a<b", "u&v", "x\"y", "/h'", "l>"] {
            assert!(!plist.contains(raw), "{raw} unescaped in {plist}");
        }
        assert!(plist.contains("<integer>18</integer>"));
    }

    /// launchd's own syntax check, on both new plists.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_vm_plists_lint() {
        for (i, plist) in [
            render_wall_plist(&mac()),
            render_supervisor_plist(&mac(), &supervisor_args()),
        ]
        .iter()
        .enumerate()
        {
            let path =
                std::env::temp_dir().join(format!("fbr-vm-plist-{}-{i}.plist", std::process::id()));
            std::fs::write(&path, plist).unwrap();
            let out = std::process::Command::new("/usr/bin/plutil")
                .arg("-lint")
                .arg(&path)
                .output()
                .unwrap();
            let _ = std::fs::remove_file(&path);
            assert!(
                out.status.success(),
                "plutil -lint: {}",
                String::from_utf8_lossy(&out.stdout)
            );
        }
    }

    #[test]
    fn xml_specials_are_escaped() {
        assert_eq!(xml_escape("a&b<c>\"'"), "a&amp;b&lt;c&gt;&quot;&apos;");
    }

    fn unit_lines(unit: &str) -> Vec<&str> {
        unit.lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect()
    }

    #[test]
    fn the_unit_renders_a_system_service_for_the_role_account() {
        let out = render_unit(&linux());
        assert!(!out.contains("{{"), "unsubstituted token left:\n{out}");
        assert!(!out.contains("--user"), "a system unit, not a per-user one");
        let lines = unit_lines(&out);
        for want in [
            "User=_faber",
            "Group=_faber_operator",
            "SupplementaryGroups=_faber",
            "ExecStart=/var/lib/faber/bin/faber run",
            "Environment=HOME=/var/lib/faber",
            "Environment=PATH=/usr/bin:/bin:/usr/sbin:/sbin",
            "WorkingDirectory=/var/lib/faber",
            "StateDirectory=faber",
            "StateDirectoryMode=0750",
            "CacheDirectory=faber",
            "CacheDirectoryMode=0750",
            "LogsDirectory=faber",
            "LogsDirectoryMode=0750",
            "UMask=0027",
            "Restart=always",
            "RestartSec=2",
            "StartLimitIntervalSec=0",
            "TimeoutStopSec=600",
            "KillMode=process",
            "NoNewPrivileges=yes",
            "PrivateTmp=yes",
            "ProtectHome=yes",
            "WantedBy=multi-user.target",
        ] {
            assert!(lines.contains(&want), "missing {want:?} in:\n{out}");
        }
        assert!(!lines
            .iter()
            .any(|l| l.starts_with("WantedBy=default.target")));
        assert!(out.contains(&format!("({LABEL})")));
    }

    /// systemd creates the three directories relative to its own roots, so
    /// they have to be where the SYSTEM layout says they are.
    #[test]
    fn the_units_directories_are_the_layouts() {
        let l = linux();
        assert_eq!(l.state_dir, Path::new("/var/lib").join("faber"));
        assert_eq!(l.cache_dir, Path::new("/var/cache").join("faber"));
        assert_eq!(l.log_dir, Path::new("/var/log").join("faber"));
        assert_eq!(
            l.service_unit,
            Path::new("/etc/systemd/system").join(UNIT_NAME)
        );
        assert_eq!(
            mac().service_unit,
            Path::new("/Library/LaunchDaemons").join(format!("{LABEL}.plist"))
        );
    }

    #[test]
    fn render_dispatches_by_os() {
        assert_eq!(render(Os::MacOs, &mac()), render_plist(&mac()));
        assert_eq!(render(Os::Linux, &linux()), render_unit(&linux()));
    }

    #[test]
    fn the_manager_commands_are_absolute_and_exact() {
        let mut fake = FakeRunner::default();
        launchd::enable(&mut fake, &system_target()).unwrap();
        launchd::bootstrap(&mut fake, "system", &mac().service_unit, Duration::ZERO).unwrap();
        launchd::kickstart(&mut fake, &system_target(), false).unwrap();
        launchd::kickstart(&mut fake, &gui_target(501), true).unwrap();
        systemd::daemon_reload(&mut fake).unwrap();
        systemd::enable_now(&mut fake).unwrap();
        let shown: Vec<String> = fake.ran.iter().map(Cmd::display).collect();
        assert_eq!(
            shown,
            vec![
                "/bin/launchctl enable system/io.github.rejifald.faber",
                "/bin/launchctl bootstrap system /Library/LaunchDaemons/io.github.rejifald.faber.plist",
                "/bin/launchctl kickstart system/io.github.rejifald.faber",
                "/bin/launchctl kickstart -k gui/501/io.github.rejifald.faber",
                "/usr/bin/systemctl daemon-reload",
                "/usr/bin/systemctl enable --now faber.service",
            ]
        );
        assert_eq!(
            systemd::user_cmd("owner", &["disable", "--now", UNIT_NAME]).display(),
            "/usr/bin/systemctl --user -M owner@ disable --now faber.service"
        );
    }

    /// `launchctl print` of a job launchd cannot spawn, as the wall prints it
    /// after a `kickstart -k` onto a replaced binary (#479), trimmed; the run
    /// count and the coalition id are example values.
    const SPAWN_FAILED: &str = "system/io.github.rejifald.faber.wall = {\n\
        \tactive count = 0\n\
        \tpath = /Library/LaunchDaemons/io.github.rejifald.faber.wall.plist\n\
        \ttype = LaunchDaemon\n\
        \tstate = spawn scheduled\n\
        \tprogram = /usr/local/bin/faber\n\
        \truns = 5\n\
        \tlast exit code = 78: EX_CONFIG\n\
        \tresource coalition = {\n\
        \t\tID = 100\n\
        \t\tstate = active\n\
        \t}\n\
        }\n";

    fn printed(stdout: &str) -> Output {
        Output {
            success: true,
            code: Some(0),
            stdout: stdout.into(),
            ..Output::default()
        }
    }

    fn not_loaded() -> Output {
        Output {
            success: false,
            code: Some(113),
            ..Output::default()
        }
    }

    #[test]
    fn print_status_reads_the_jobs_own_lines() {
        assert_eq!(
            launchd::print_status(SPAWN_FAILED),
            (Some("spawn scheduled"), Some("78: EX_CONFIG"))
        );
        assert_eq!(
            launchd::print_status("x = {\n\tstate = running\n\tpid = 3159\n}\n"),
            (Some("running"), None)
        );
        assert_eq!(launchd::print_status(""), (None, None));
    }

    /// The commands a run issued, without its `print` probes.
    fn issued(fake: &FakeRunner) -> Vec<String> {
        fake.ran
            .iter()
            .map(Cmd::display)
            .filter(|c| !c.starts_with("/bin/launchctl print "))
            .collect()
    }

    /// A loaded job is booted out and bootstrapped again, never kickstarted
    /// with `-k`: that leaves it pinned to the code it replaced (#479).
    #[test]
    fn reload_boots_a_loaded_job_out_and_in_again() {
        let target = system_target_of(WALL_LABEL);
        let print = format!("/bin/launchctl print {target}");
        let mut fake = FakeRunner {
            answers: vec![
                (print.clone(), printed("state = running")),
                (print.clone(), printed("state = running")),
                (print, not_loaded()),
            ],
            ..FakeRunner::default()
        };
        launchd::reload(
            &mut fake,
            "system",
            &target,
            Path::new(WALL_PLIST),
            Duration::from_secs(5),
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(
            issued(&fake),
            vec![
                format!("/bin/launchctl bootout {target}"),
                format!("/bin/launchctl bootstrap system {WALL_PLIST}"),
                format!("/bin/launchctl kickstart {target}"),
            ]
        );
    }

    #[test]
    fn reload_bootstraps_a_job_that_is_not_loaded() {
        let target = system_target_of(SUPERVISOR_LABEL);
        let mut fake = FakeRunner {
            answers: vec![(format!("/bin/launchctl print {target}"), not_loaded())],
            ..FakeRunner::default()
        };
        launchd::reload(
            &mut fake,
            "system",
            &target,
            Path::new(SUPERVISOR_PLIST),
            Duration::from_secs(5),
            Duration::ZERO,
        )
        .unwrap();
        assert_eq!(
            issued(&fake),
            vec![
                format!("/bin/launchctl bootstrap system {SUPERVISOR_PLIST}"),
                format!("/bin/launchctl kickstart {target}"),
            ]
        );
    }

    /// A job that launchd keeps failing to spawn is an error naming its state
    /// and exit code, not a quiet success.
    #[test]
    fn wait_running_refuses_a_job_that_never_spawns() {
        let target = system_target_of(WALL_LABEL);
        let mut fake = FakeRunner {
            answers: vec![(
                format!("/bin/launchctl print {target}"),
                printed(SPAWN_FAILED),
            )],
            ..FakeRunner::default()
        };
        let err = launchd::wait_running(&mut fake, &target, Duration::ZERO, Duration::ZERO)
            .unwrap_err()
            .to_string();
        assert!(err.contains("spawn scheduled"), "{err}");
        assert!(err.contains("78: EX_CONFIG"), "{err}");
    }

    #[test]
    fn wait_running_waits_for_the_spawn() {
        let target = system_target_of(WALL_LABEL);
        let print = format!("/bin/launchctl print {target}");
        let mut fake = FakeRunner {
            answers: vec![
                (print.clone(), printed(SPAWN_FAILED)),
                (print, printed("state = running\npid = 3159")),
            ],
            ..FakeRunner::default()
        };
        launchd::wait_running(&mut fake, &target, Duration::from_secs(5), Duration::ZERO).unwrap();
        assert_eq!(fake.ran.len(), 2);
    }

    /// A bootstrap that keeps failing reports its last error, after exactly
    /// five attempts.
    #[test]
    fn bootstrap_retries_five_times_then_reports() {
        let plist = mac().service_unit;
        let cmd = format!("/bin/launchctl bootstrap system {}", plist.display());
        let mut fake = FakeRunner {
            answers: vec![(
                cmd,
                Output {
                    success: false,
                    code: Some(5),
                    stderr: "Bootstrap failed: 5: Input/output error".into(),
                    ..Output::default()
                },
            )],
            ..FakeRunner::default()
        };
        let err = launchd::bootstrap(&mut fake, "system", &plist, Duration::ZERO).unwrap_err();
        assert!(err.to_string().contains("Input/output error"), "{err}");
        assert_eq!(fake.ran.len(), 5);
    }

    #[test]
    fn systemd_versions_parse() {
        assert_eq!(
            systemd::parse_version("systemd 252 (252.22-1~deb12u1)\n+PAM +AUDIT"),
            Some(252)
        );
        assert_eq!(
            systemd::parse_version("systemd 245 (245.4-4ubuntu3)"),
            Some(245)
        );
        assert_eq!(systemd::parse_version("not systemd"), None);
        assert_eq!(systemd::parse_version(""), None);
    }

    #[test]
    fn restart_hints_name_the_system_service() {
        assert_eq!(
            restart_hint(Os::MacOs),
            "sudo launchctl kickstart -k system/io.github.rejifald.faber"
        );
        assert_eq!(
            restart_hint(Os::Linux),
            "sudo systemctl restart faber.service"
        );
    }
}
