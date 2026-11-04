//! `sudo faber setup` and `sudo faber uninstall` (ADR-0025): faber runs as its
//! own role accounts, `_faber` for the daemon and `_faber_vm` for the host side
//! of its VMs, under a LaunchDaemon or a systemd system unit, never as a
//! person.
//!
//! Setup is one privileged command run by the operator from a terminal they
//! already trust. It shows its plan first, from read-only probes, and asks
//! before changing anything:
//!
//! 1. the two accounts and their groups, with the operator added to `_faber`'s
//! 2. the SYSTEM directories ([`crate::platform::SystemLayout`])
//! 3. the binaries: a root-owned CLI copy and the daemon's own copy
//! 4. a starter config when there is nothing to migrate
//! 5. every old per-user faber, found
//! 6. and drained, stopped and moved aside
//! 7. its config and state copied (never moved), each copy's sha256 recorded,
//!    the config checked as `_faber` before anything starts
//! 8. the setup record, `/etc/faber/setup.json`, written
//! 9. macOS: the VM launcher and the guest agent, `_faber_vm`'s directories, the wall's network
//!    plan and stored rules, the anchor checker's and the VM supervisor's
//!    LaunchDaemons, and any image imported ([`vm_runtime`], [`vm_images`])
//! 10. the service installed and started, and its API answering
//! 11. the Mac kept awake, and on macOS the menu-bar app at
//!     `/Applications/faber.app`, the operator's, from the release's archive
//!     beside the launcher, once the daemon answers; it updates itself from
//!     then on ([`app`])
//! 12. macOS, when asked (`--macos-session`): the account whose login session
//!     runs macOS VMs, and its reboot mode (ADR-0051, [`session`])
//! 13. the Linux VM's image built, last ([`linux_image`])
//!
//! A re-run on a set-up host plans nothing and restarts nothing. A failure
//! after step 6 puts the old instance back ([`apply`]).
//!
//! # The root invariant
//!
//! Root never executes or trusts anything `_faber` or `_faber_vm` can write.
//! Setup refuses to run from a binary either account could have replaced
//! ([`exe`]), runs every tool by absolute path ([`tools`]), writes into
//! `_faber`'s directories only through descriptors that refuse symlinks
//! ([`fsx`]), copies only regular files their account owns, and never parses a
//! config it migrates. It never executes the daemon's binary either: its
//! version comes from a marker in its bytes ([`marker`]), a hint that only
//! decides whether a newer copy is kept (`--replace-daemon` replaces it
//! anyway). The one process it starts as a role account, the CLI copy checking
//! a config, gets a session of its own and no terminal ([`tools::run_as`]).
//!
//! Setup migrates only from the account that ran sudo, unless
//! `--migrate-from` names another: someone else's config would decide what the
//! daemon runs.
//!
//! # Layout
//!
//! [`plan`] is pure and decides everything; [`probe`] reads the host; [`apply`]
//! and [`uninstall`] execute. This process's own layout is LEGACY until the
//! record exists, so setup works from [`crate::platform::system`] and
//! [`crate::platform::legacy`] directly, never the cached path accessors.

use std::io::{BufRead, IsTerminal, Write};
use std::time::Duration;

use anyhow::{bail, Result};

pub mod accounts;
pub mod app;
pub mod docker_cli;
pub mod exe;
pub mod fsx;
pub mod golden_build;
pub mod golden_prepare;
pub mod linux_image;
pub mod marker;
pub mod migrate;
pub mod old;
pub mod plan;
pub mod session;
pub mod tools;
pub mod uninstall;
pub mod view;
pub mod vm_images;
pub mod vm_runtime;
pub mod walled_vm;

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod apply;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub(crate) mod probe;

/// Options for `sudo faber setup`.
#[derive(Debug, Clone)]
pub struct SetupOpts {
    /// Print the plan and change nothing.
    pub dry_run: bool,
    /// Print every step under its item, with its commands, owners and
    /// modes (ADR-0050).
    pub verbose: bool,
    /// Skip the confirmation. Never answers the autoupdate question.
    pub yes: bool,
    /// Keep the Mac awake (`pmset -a sleep 0`, `powernap 0`).
    pub keep_awake: bool,
    /// How long to wait for an old instance, or a running daemon being
    /// replaced, to finish its admitted jobs.
    pub drain_timeout: Duration,
    /// Replace the daemon's copy whenever it differs from this binary, even
    /// when its version marker claims it is newer: the repair path.
    pub replace_daemon: bool,
    /// Copy the old instance's state again, moving the existing copies aside.
    pub remigrate: bool,
    /// Migrate from this account's per-user install instead of the one
    /// running sudo. Without it, nobody else's config is ever copied.
    pub migrate_from: Option<String>,
    /// Migrate nothing, and start from the starter config.
    pub fresh: bool,
    /// Leave the per-user directories in the operator's home where they are,
    /// instead of renaming them aside after a healthy start.
    pub keep_legacy: bool,
    /// The VM launcher, golden image and runner tarball (macOS).
    pub vm: vm_runtime::VmOpts,
    /// macOS: set up even though the host will have no runnable VM runtime
    /// and so run no jobs (#223). For testing.
    pub before_vm_runtime: bool,
    /// Only say whether this run would give the host a runnable VM runtime,
    /// and exit 1 if not: what `deploy/install.sh` asks before handing off.
    /// Needs no root, and changes nothing.
    pub check_vm_runtime: bool,
    /// macOS: ADR-0051's session account and its reboot mode.
    pub session: session::SessionOpts,
}

/// Options for `sudo faber uninstall`.
#[derive(Debug, Clone)]
pub struct UninstallOpts {
    /// Print the plan and change nothing.
    pub dry_run: bool,
    /// Skip the confirmation.
    pub yes: bool,
    /// Copy `audit.jsonl` and `config.toml` to the operator's home first.
    pub keep_logs: bool,
    /// How long to wait for the daemon's admitted jobs to finish.
    pub drain_timeout: Duration,
    /// Stop the daemon without draining it, orphaning what it runs. Always
    /// asks first, `--yes` or not.
    pub no_drain: bool,
}

/// The control socket of the SYSTEM daemon. Setup runs as root while its
/// process layout is still LEGACY, so this never asks `platform`.
pub(crate) fn system_socket() -> std::path::PathBuf {
    crate::platform::layout::SystemLayout::for_os(crate::platform::layout::Os::HOST)
        .control_socket()
}

/// The control socket of the per-user daemon of the account whose home is
/// `home`: its LEGACY state directory. An unresolvable one is the empty path,
/// which does not exist, so a request falls back to the plain-TCP port.
pub(crate) fn legacy_socket(home: &std::path::Path) -> std::path::PathBuf {
    crate::platform::legacy(&crate::platform::Env::with_home(home))
        .control_socket
        .unwrap_or_default()
}

/// Whether a faber daemon answers: a 2xx JSON object from `GET /api/state`,
/// over `socket`, or, when `fallback` allows it, over `127.0.0.1:port` for a
/// daemon from before ADR-0028.
pub(crate) async fn api_answers(
    socket: &std::path::Path,
    fallback: crate::control::Fallback,
) -> bool {
    let Ok((status, text)) =
        crate::control::request(socket, fallback, reqwest::Method::GET, "/api/state", None).await
    else {
        return false;
    };
    status.is_success()
        && serde_json::from_str::<serde_json::Value>(&text).is_ok_and(|v| v.is_object())
}

/// UTC now as `20260928T101530Z`: the suffix for backups setup makes.
pub(crate) fn timestamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    stamp(secs)
}

/// Seconds since the epoch as `YYYYMMDDTHHMMSSZ`, by Howard Hinnant's
/// `civil_from_days`.
pub(crate) fn stamp(secs: u64) -> String {
    let (days, rem) = (secs / 86_400, secs % 86_400);
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + u64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// Ask `prompt` on the terminal; only `y` or `yes` is a yes.
pub(crate) fn confirm(prompt: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("no terminal to confirm on: re-run with --yes to apply the plan above");
    }
    print!("{prompt} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(matches!(line.trim(), "y" | "Y" | "yes" | "Yes"))
}

/// Fleet autoupdate (ADR-0006) for a freshly seeded config. Off by default,
/// and `--yes` never turns it on: installing faber is not permission for it
/// to replace its own binary.
fn ask_autoupdate(yes: bool) -> bool {
    if yes {
        println!(
            "autoupdate: left off. --yes accepts the plan, not this (ADR-0006); enable it later \
             with [update] enabled = true"
        );
        return false;
    }
    if !std::io::stdin().is_terminal() {
        println!("autoupdate: no terminal to ask, so it stays off (the default)");
        return false;
    }
    match confirm("Enable fleet autoupdate for this host? Opt-in, off by default (ADR-0006).") {
        Ok(true) => true,
        _ => {
            println!("autoupdate: left off; enable it later with [update] enabled = true");
            false
        }
    }
}

/// What the operator needs to know once setup is done. `left_faber`: this
/// run took the operator out of `_faber`'s group, which reads the token, and
/// `token_stored`: there is a token that group could read.
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn handoff(
    os: crate::platform::layout::Os,
    layout: &crate::platform::SystemLayout,
    legacy_backups: &[std::path::PathBuf],
    left_faber: bool,
    token_stored: bool,
) -> String {
    use crate::platform::layout::{Os, FABER_ACCOUNT, FABER_OPERATOR_GROUP};
    use crate::platform::service;

    let logs = match os {
        Os::MacOs => format!(
            "{0}/stderr.log and {0}/audit.jsonl",
            layout.log_dir.display()
        ),
        Os::Linux => format!(
            "`journalctl -u {}` and {}/audit.jsonl",
            service::UNIT_NAME,
            layout.log_dir.display()
        ),
    };
    let mut out = format!(
        "\nfaber runs as _faber now.\n  \
         dashboard  faber dashboard\n  \
         logs       {logs} (readable through group {FABER_OPERATOR_GROUP})\n  \
         restart    {restart}\n  \
         remove     sudo faber uninstall\n\n\
         Next:\n  \
         - Your membership of group {FABER_OPERATOR_GROUP} takes effect at your next login; \
         until then, read faber's files with sudo.\n",
        restart = service::restart_hint(os),
    );
    if left_faber {
        out.push_str(&format!(
            "  - You left group {FABER_ACCOUNT}, which reads faber's GitHub token. A process \
             keeps the groups it started with, so log out of every session (or restart this \
             machine) to end every process still in it."
        ));
        out.push_str(if token_stored {
            " Until now every process you ran could read the stored token: once you have, \
             replace it (`faber auth link`, create the new token on GitHub, `sudo faber auth \
             set`) and revoke the old one on GitHub.\n"
        } else {
            "\n"
        });
    }
    // Said once: replacing an exposed token is already the step above.
    if !(left_faber && token_stored) {
        out.push_str(
            "  - Give the daemon its GitHub token (ADR-0027): run `faber auth link`, create the \
             token on GitHub, then `sudo faber auth set`. Until then it waits in standby, and \
             the dashboard says why.\n",
        );
    }
    if !legacy_backups.is_empty() {
        out.push_str(
            "  - Your old per-user faber files were moved aside, not deleted. They are the \
             backup; delete them once you no longer need them:\n",
        );
        for backup in legacy_backups {
            out.push_str(&format!("      {}\n", backup.display()));
        }
    }
    if os == Os::MacOs {
        out.push_str(
            "  - The daemon starts at boot with nobody logged in, so auto-login as your own \
             account is not needed: turn it off if it was only for faber. macOS VMs run in \
             the session account's login session instead (ADR-0051, `--macos-session`), and \
             `--macos-session auto-login` is the one auto-login faber sets up.\n  \
             - An old `ci` account from the earlier isolated deployment can be removed. Do \
             it from Terminal.app: macOS refuses account deletion from terminals inside other \
             apps.\n  \
             - FileVault still needs a person to unlock the disk after a reboot, before anything \
             starts, faber included.\n",
        );
    }
    out
}

/// `faber setup --check-vm-runtime`: whether `sudo faber setup` from this
/// binary would leave a runnable VM runtime ([`plan::vm_runtime_gaps`]).
#[cfg(any(target_os = "macos", target_os = "linux"))]
fn check_vm_runtime(state: &plan::HostState, steps: &[plan::Step]) -> Result<()> {
    let gaps = plan::vm_runtime_gaps(state, steps);
    if gaps.is_empty() {
        println!("VM runtime: setup from this binary leaves this host one it can run jobs in");
        return Ok(());
    }
    bail!(
        "no runnable VM runtime: {}",
        crate::term::sanitize(&gaps.join("; "))
    )
}

/// `sudo faber setup`. Without root it prints the plan from read-only probes
/// and exits 0.
pub async fn run(opts: SetupOpts) -> Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        use tools::SystemRunner;

        // The marker a later setup reads this binary's version from, instead
        // of running it (see `marker`): kept in every build.
        marker::keep();
        let mut runner = SystemRunner;
        let plan_opts = plan::PlanOpts {
            remigrate: opts.remigrate,
            replace_daemon: opts.replace_daemon,
            keep_awake: opts.keep_awake,
            migrate_from: opts.migrate_from.clone(),
            fresh: opts.fresh,
            keep_legacy: opts.keep_legacy,
            vm: opts.vm.clone(),
            before_vm_runtime: opts.before_vm_runtime,
        };
        let state = probe::gather(plan_opts, &mut runner).await?;
        let mut steps = plan::plan(&state);
        if opts.check_vm_runtime {
            return check_vm_runtime(&state, &steps);
        }
        #[cfg(target_os = "macos")]
        let session_probe = session::probe::gather(
            &state,
            plan::planned_ids(&state).ok().map(|(faber, _)| faber.uid),
            &mut runner,
        )?;
        #[cfg(not(target_os = "macos"))]
        let session_probe = session::SessionProbe::default();
        steps.extend(session::plan_steps(&state, &session_probe, &opts.session));
        let restart = session::restart_steps(&steps, &session_probe);
        steps.extend(restart);
        plan::fold_session_record(&mut steps);
        let (create, change, refused) = plan::counts(&steps);
        // The first trailer that applies (ADR-0050); the decisions below are
        // the same ones, in the same order.
        let end = if !state.privileged {
            view::End::Preview {
                exe: &state.exe_path,
            }
        } else if opts.dry_run {
            view::End::DryRun
        } else if refused > 0 {
            view::End::Refused
        } else if create + change == 0 {
            view::End::Done
        } else {
            view::End::Prompt
        };
        // The plan quotes what other accounts chose, such as a config path from
        // an old per-user plist, on root's terminal: `out` sanitizes every
        // string it lays out.
        let style = crate::out::Style::stdout(opts.verbose);
        print!("{}", view::render(&state, &steps, style, end));

        if !state.privileged || opts.dry_run {
            return Ok(());
        }
        if refused > 0 {
            // The plan's last line already says how many items, and that
            // nothing was changed; this only sets the exit status.
            bail!("setup refused: fix what is marked refused above, then re-run");
        }
        let token_stored = !state.credential_files.is_empty();
        if create + change == 0 {
            print!(
                "{}",
                handoff(state.os, &state.layout, &[], false, token_stored)
            );
            return Ok(());
        }
        let left_faber = plan::leaves_faber(&state, &steps);
        if !opts.yes && !confirm("Proceed?")? {
            bail!("setup cancelled; nothing changed");
        }
        let autoupdate = steps
            .iter()
            .any(|s| s.action == Some(plan::Action::SeedConfig))
            && ask_autoupdate(opts.yes);
        println!();
        let backups =
            apply::apply(&state, &steps, &opts, autoupdate, &timestamp(), &mut runner).await?;
        // After the service answers: a failure here leaves faber itself set
        // up, and a re-run plans only what is left of the session account.
        #[cfg(target_os = "macos")]
        session::apply::apply(&state, &steps, &mut runner)?;
        print!(
            "{}",
            crate::term::sanitize(&handoff(
                state.os,
                &state.layout,
                &backups,
                left_faber,
                token_stored
            ))
        );
        // Last, with the daemon already serving: it takes up to half an hour,
        // and its failure leaves everything above, and the previous image, as
        // it is.
        apply::linux_image(&state, &steps).await
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = opts;
        bail!("faber setup supports macOS and Linux only")
    }
}

/// `sudo faber uninstall`. Without root it prints the plan and exits 0.
pub async fn uninstall(opts: UninstallOpts) -> Result<()> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let mut runner = tools::SystemRunner;
        let state = uninstall::gather(opts.keep_logs, opts.no_drain, &mut runner)?;
        let steps = uninstall::uninstall_plan(&state, &timestamp())
            .map_err(|e| anyhow::anyhow!("uninstall refused: {e}"))?;
        print!(
            "{}",
            crate::term::sanitize(&uninstall::render(&state, &steps))
        );
        if !state.privileged {
            println!(
                "\nThis preview used read-only probes; nothing changed. Re-run with sudo to \
                 apply it:\n  sudo {} uninstall",
                state.exe_path.display()
            );
            return Ok(());
        }
        if opts.dry_run {
            println!("\n--dry-run: nothing changed.");
            return Ok(());
        }
        if steps
            .iter()
            .all(|s| matches!(s, uninstall::UStep::Leave(_)))
        {
            return Ok(());
        }
        if !opts.yes && !confirm("\nRemove faber as listed above?")? {
            bail!("uninstall cancelled; nothing changed");
        }
        // Skipping the drain orphans running jobs: `--yes` accepts the plan,
        // never this, so it is asked for on its own.
        if steps.contains(&uninstall::UStep::SkipDrain) {
            eprintln!("\nWARNING: {}", uninstall::NO_DRAIN_WARNING);
            if !std::io::stdin().is_terminal() {
                bail!("--no-drain needs a terminal to confirm on: --yes does not answer it");
            }
            if !confirm("Stop faber WITHOUT draining it?")? {
                bail!("uninstall cancelled; nothing changed");
            }
        }
        println!();
        uninstall::execute(&state, &steps, opts.drain_timeout, &mut runner).await?;
        println!("\nfaber is uninstalled.");
        // The commands name what was found on disk, never a placeholder: a
        // copied block runs as it is.
        let operator = state.operator.as_ref().ok().map(|op| op.name.as_str());
        print!(
            "{}",
            crate::term::sanitize(&uninstall::restore_note(
                state.os,
                &state.restores,
                operator
            ))
        );
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = opts;
        bail!("faber uninstall supports macOS and Linux only")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_are_utc_and_sortable() {
        assert_eq!(stamp(0), "19700101T000000Z");
        // 2026-09-28T10:15:30Z
        assert_eq!(stamp(1_790_590_530), "20260928T101530Z");
        assert_eq!(stamp(951_782_400), "20000229T000000Z");
        assert!(timestamp().ends_with('Z'));
    }

    /// S1: a run that took the operator out of `_faber` says to end every
    /// process still holding the group, then to replace the token it could
    /// read; with no token stored, only the first.
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn leaving_faber_says_to_log_out_then_replace_the_token() {
        use crate::platform::layout::{Os, SystemLayout};
        let l = SystemLayout::for_os(Os::MacOs);
        let moved = handoff(Os::MacOs, &l, &[], true, true);
        assert!(moved.contains("You left group _faber"), "{moved}");
        assert!(moved.contains("log out of every session"), "{moved}");
        assert!(moved.contains("replace it"), "{moved}");
        assert!(moved.contains("revoke the old one on GitHub"), "{moved}");
        assert!(
            !moved.contains("Give the daemon its GitHub token"),
            "{moved}"
        );
        assert!(
            moved.contains("readable through group _faber_operator"),
            "{moved}"
        );

        let no_token = handoff(Os::MacOs, &l, &[], true, false);
        assert!(no_token.contains("log out of every session"), "{no_token}");
        assert!(!no_token.contains("replace it"), "{no_token}");
        assert!(no_token.contains("Give the daemon its GitHub token"));

        let fresh = handoff(
            Os::Linux,
            &SystemLayout::for_os(Os::Linux),
            &[],
            false,
            false,
        );
        assert!(!fresh.contains("You left group"), "{fresh}");
        assert!(fresh.contains("Give the daemon its GitHub token"));
    }
}
