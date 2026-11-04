//! `faber wall`: the wall's two commands, and `faber wall-daemon`, the
//! checker as setup installs it. All hidden.
//!
//! - `faber wall render --plan PATH [--grants PATH]` prints the stored wall
//!   (JSON) that the plan and grants render to. Setup and `sudo faber grant`
//!   will store it; nothing here installs it. It reads no live facts and only
//!   prints, so it needs no root, runs on every OS and reads its inputs
//!   without the root-owned check: an operator can preview a draft. No
//!   `--grants` is no grants file: the default resolvers and no grants.
//! - `faber wall check --stored PATH --verdict PATH` is the root checker. It
//!   first refuses to run from a binary faber's own accounts could have
//!   replaced (ADR-0025, the same check as `sudo faber upgrade-cli`), then
//!   refuses to run as anyone but root. It reads the stored wall only if root
//!   alone could have written it and every directory above it, enforces it
//!   and never renders, and writes the verdict every `--interval-secs` into a
//!   directory only root can write. Both paths must be absolute and canonical
//!   (on macOS `/private/etc/…`, not `/etc/…`). macOS only.
//! - `faber wall-daemon` is `wall check` with its paths fixed: the stored set
//!   at [`paths::STORED_CANONICAL`], the verdict at
//!   [`paths::VERDICT_CANONICAL`], every [`DEFAULT_INTERVAL_SECS`], never
//!   once. It takes no arguments, so its LaunchDaemon, which `sudo faber
//!   setup` installs to run the root-owned CLI copy as root
//!   (`/usr/local/bin/faber wall-daemon`, ADR-0026 amended 2026-10-17), names
//!   nothing a later edit to the plist could point elsewhere. `sudo faber
//!   upgrade-cli` restarts it on the new copy.

use std::path::PathBuf;

use super::paths;

/// Seconds between the checker's ticks, unless `--interval-secs` says.
pub const DEFAULT_INTERVAL_SECS: u64 = 5;

/// `faber wall …`.
#[derive(Debug, Clone, PartialEq, Eq, clap::Subcommand)]
pub enum WallCommand {
    /// Print the stored wall (JSON) a plan and grants render to.
    Render {
        /// The network plan (JSON).
        #[arg(long)]
        plan: PathBuf,
        /// The grants file (TOML). Absent: the default resolvers, no grants.
        #[arg(long)]
        grants: Option<PathBuf>,
    },
    /// Enforce a stored wall, repair what root may, and write the verdict.
    /// Root only.
    Check {
        /// The stored wall (JSON) setup wrote. Must be root-owned, in a
        /// root-owned directory.
        #[arg(long)]
        stored: PathBuf,
        /// Where to write the verdict file.
        #[arg(long)]
        verdict: PathBuf,
        /// Check once, then exit 0 if green and 1 if red.
        #[arg(long)]
        once: bool,
        /// Seconds between checks.
        #[arg(long, default_value_t = DEFAULT_INTERVAL_SECS)]
        interval_secs: u64,
    },
}

/// Run a `faber wall` command. Blocking: call it from `spawn_blocking`.
pub fn run(cmd: WallCommand) -> anyhow::Result<()> {
    match cmd {
        WallCommand::Render { plan, grants } => {
            let stored = render_files(&plan, grants.as_deref())?;
            std::io::Write::write_all(&mut std::io::stdout(), &stored.to_json())?;
            Ok(())
        }
        #[cfg(target_os = "macos")]
        WallCommand::Check {
            stored,
            verdict,
            once,
            interval_secs,
        } => mac::check(&stored, &verdict, once, interval_secs),
        #[cfg(not(target_os = "macos"))]
        WallCommand::Check { .. } => {
            anyhow::bail!(
                "faber wall check is macOS-only: the VM wall (ADR-0026) runs on macOS hosts"
            )
        }
    }
}

/// `faber wall-daemon`: the root checker on the stored set and verdict file
/// setup installs it against. Blocking, and returns only on an error.
pub fn run_daemon() -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        mac::check(
            std::path::Path::new(paths::STORED_CANONICAL),
            std::path::Path::new(paths::VERDICT_CANONICAL),
            false,
            DEFAULT_INTERVAL_SECS,
        )
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (paths::STORED_CANONICAL, paths::VERDICT_CANONICAL);
        anyhow::bail!("faber wall-daemon is macOS-only: the VM wall (ADR-0026) runs on macOS hosts")
    }
}

/// Read a plan and an optional grants file, and render them. A grant whose
/// profile no slot lists is reported on stderr: it renders no rule.
fn render_files(
    plan: &std::path::Path,
    grants: Option<&std::path::Path>,
) -> anyhow::Result<super::stored::StoredWall> {
    use anyhow::Context;

    use super::grants::Grants;
    use super::model::NetworkPlan;

    let bytes = std::fs::read(plan).with_context(|| format!("reading {}", plan.display()))?;
    let plan_v =
        NetworkPlan::from_json(&bytes).with_context(|| format!("parsing {}", plan.display()))?;
    let grants_v = match grants {
        None => Grants::default(),
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading {}", path.display()))?;
            Grants::parse(&text).with_context(|| format!("parsing {}", path.display()))?
        }
    };
    for profile in super::render::unmatched_profiles(&plan_v, &grants_v) {
        eprintln!(
            "faber wall: no VM slot runs profile {}, so its grants render no rule",
            profile.escape_debug()
        );
    }
    Ok(super::render::render(&plan_v, &grants_v)?)
}

#[cfg(target_os = "macos")]
mod mac {
    use std::time::Duration;

    use anyhow::bail;

    use std::path::Path;

    use super::super::sys::{run_once, CheckerState, MacHost};
    use crate::platform::record::current_euid;
    use crate::term::sanitize;

    /// The root checker's loop.
    pub(super) fn check(
        stored: &Path,
        verdict: &Path,
        once: bool,
        interval_secs: u64,
    ) -> anyhow::Result<()> {
        // The checker runs as root and loads what it renders.
        crate::setup::exe::trust_running_binary()?;
        if current_euid() != 0 {
            bail!("faber wall check must run as root: it loads a pf anchor");
        }
        let interval = Duration::from_secs(interval_secs.max(1));
        let mut last: Option<(bool, Vec<String>)> = None;
        let mut state = CheckerState::start(&MacHost);
        loop {
            match run_once(&MacHost, &mut state, stored, verdict) {
                Ok(out) => {
                    let now = (out.verdict.is_green(), out.file.reasons.clone());
                    // One line per change, not one per tick.
                    if last.as_ref() != Some(&now) {
                        eprintln!("faber wall: {}", summary(&now.0, &now.1));
                    }
                    if once {
                        if now.0 {
                            return Ok(());
                        }
                        bail!("the wall is red");
                    }
                    last = Some(now);
                }
                Err(e) => {
                    // The reader sees the old verdict age into red.
                    eprintln!(
                        "faber wall: writing {}: {e}",
                        sanitize(&verdict.display().to_string())
                    );
                    if once {
                        return Err(e.into());
                    }
                    last = None;
                }
            }
            std::thread::sleep(interval);
        }
    }

    fn summary(green: &bool, reasons: &[String]) -> String {
        if *green {
            "green".to_string()
        } else {
            sanitize(&format!("red: {}", reasons.join("; "))).into_owned()
        }
    }
}
