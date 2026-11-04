//! faber CLI entry point.
//!
//! `run` is the daemon (discover → poll → gate → provision) with a localhost
//! control-panel dashboard; `run --once` does a single cycle.

use std::sync::Arc;
use std::time::Duration;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "faber",
    // The build-stamped version (`0.2.0+g1a2b3c4`), so `faber --version` on any
    // host in the fleet names the exact commit it is running.
    version = env!("FABER_VERSION"),
    about = "Local just-in-time GitHub Actions runner provisioner"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
    /// Colour the output: by the terminal and the environment (`NO_COLOR`,
    /// `CLICOLOR`, `TERM`), always, or never (ADR-0050). `sudo` drops the
    /// environment, so this is how a choice reaches `sudo faber setup`.
    #[arg(long, global = true, value_enum, default_value_t, value_name = "WHEN")]
    color: faber::out::ColorWhen,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon: discover → poll → gate → provision, with a dashboard.
    Run {
        /// Do a single reconcile cycle and exit (no dashboard).
        #[arg(long)]
        once: bool,
        /// Repo topic that opts a repo into faber.
        #[arg(long, default_value = "faber")]
        topic: String,
        /// Ignored since ADR-0052: it labelled the native profile this path
        /// synthesized, and that backend is gone. Still accepted, so an
        /// existing service definition that passes it keeps starting.
        #[arg(long, default_value = "")]
        labels: String,
        /// Seconds between reconcile cycles.
        #[arg(long, default_value_t = 20)]
        interval: u64,
        /// Ignored since ADR-0052: it bounded native jobs, and that backend is
        /// gone. Still accepted, so an existing service definition that passes
        /// it keeps starting.
        #[arg(long, default_value_t = 3)]
        concurrency: usize,
        /// Port for the localhost control panel.
        #[arg(long, default_value_t = 8787)]
        port: u16,
        /// Config file with routing profiles (default:
        /// ~/Library/Application Support/faber/config.toml). Absent = no
        /// profiles, so nothing is served until a config is saved.
        #[arg(long)]
        config: Option<String>,
    },
    /// Run faber as its own accounts (ADR-0025): create `_faber` and
    /// `_faber_vm`, install the system service, and migrate a per-user
    /// install. Run it with sudo; without sudo it prints the plan.
    Setup {
        /// Print the plan without changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Show every step of the plan, with the commands it runs and the
        /// owner and mode of every path.
        #[arg(long, short = 'v')]
        verbose: bool,
        /// Skip the confirmation prompt. Never turns autoupdate on.
        #[arg(long)]
        yes: bool,
        /// Skip the keep-awake step (`pmset -a sleep 0`).
        #[arg(long)]
        skip_keep_awake: bool,
        /// How long to wait, in seconds, for a running faber's admitted jobs
        /// to finish before it is stopped.
        #[arg(long, default_value_t = 3600)]
        drain_timeout: u64,
        /// Replace the daemon's own binary whenever it differs from this one,
        /// even when its version marker claims it is newer: the repair path.
        #[arg(long, alias = "allow-downgrade")]
        replace_daemon: bool,
        /// Copy the per-user install's config and state again, moving the
        /// copies made before aside.
        #[arg(long)]
        remigrate: bool,
        /// Migrate the per-user install of this account instead of yours.
        /// Without it, only the account running sudo is migrated from.
        #[arg(long, value_name = "USER")]
        migrate_from: Option<String>,
        /// Migrate nothing, and start from the starter config, even when
        /// another account's per-user install has a config.
        #[arg(long, conflicts_with = "migrate_from")]
        fresh: bool,
        /// Leave your per-user faber directories where they are, instead of
        /// renaming them aside once the new daemon answers.
        #[arg(long)]
        keep_legacy: bool,
        /// Removed: faber no longer runs as a person's account.
        #[arg(long, hide = true)]
        as_runner: bool,
        /// Skip the first step, `upgrade-cli`: set when setup re-runs itself
        /// from the CLI copy that step just installed.
        #[arg(long, hide = true)]
        skip_cli_upgrade: bool,
        /// macOS: the VM launcher to install (ADR-0026). Default: `faber-vm`
        /// or the release asset's name, beside this binary.
        #[arg(long, value_name = "PATH")]
        launcher: Option<std::path::PathBuf>,
        /// macOS: import this golden macOS image bundle (disk, vz-aux,
        /// vz-hwmodel, vz-identifier) into the root-owned image store.
        #[arg(long, value_name = "PATH")]
        golden_image: Option<std::path::PathBuf>,
        /// The golden image's name in the store, as a profile's `image`
        /// names it. Default: the bundle directory's name.
        #[arg(long, value_name = "NAME", requires = "golden_image")]
        golden_name: Option<String>,
        /// Replace a different golden image already stored under that name.
        #[arg(long, requires = "golden_image")]
        replace_golden: bool,
        /// macOS: keep this `docker save` archive of the runner image in the
        /// root-owned image store, for faber's Linux VM to load.
        #[arg(long, value_name = "PATH")]
        runner_image_tar: Option<std::path::PathBuf>,
        /// macOS: set up even though this binary leaves no runnable VM
        /// runtime (no launcher, or no Linux VM image it can build). The host
        /// then runs no jobs: for testing only.
        #[arg(long)]
        before_vm_runtime: bool,
        /// Only check whether setup from this binary would leave a runnable
        /// VM runtime; exit 1 if not. Needs no sudo; changes nothing.
        #[arg(long, conflicts_with_all = ["dry_run", "yes", "before_vm_runtime"])]
        check_vm_runtime: bool,
        /// macOS: create or update the account whose own login session runs
        /// macOS VMs (ADR-0051), with this reboot mode: `unlock` (a SecureToken;
        /// unlock the disk as it after a reboot), `auto-login` (FileVault off
        /// only) or `manual`. Without a value: the account's current mode, or
        /// `unlock` with FileVault on and `manual` with it off.
        #[arg(long, value_name = "MODE", num_args = 0..=1)]
        macos_session: Option<Option<faber::setup::session::RebootMode>>,
        /// macOS: list the session account at the login window, or hide it.
        /// Default: visible, hidden for `auto-login`.
        #[arg(long, value_name = "VISIBILITY")]
        macos_session_visibility: Option<faber::setup::session::Visibility>,
        /// macOS: type the session account's password, or let faber generate
        /// it and show it once (`auto-login` needs `generate`). Given on an
        /// account that has one, sets it again.
        #[arg(long, value_name = "SOURCE")]
        macos_session_password: Option<faber::setup::session::PasswordSource>,
    },
    /// Install the daemon's own binary over the root-owned CLI copy,
    /// `/usr/local/bin/faber`, once it is verified against its signed release
    /// and newer (ADR-0034). Run it with sudo.
    UpgradeCli {
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
    },
    /// The VM wall (ADR-0026): render a stored rule set, or enforce one and
    /// repair it as root. Setup installs the checker as `wall-daemon`.
    #[command(hide = true)]
    Wall {
        #[command(subcommand)]
        action: faber::wall::cli::WallCommand,
    },
    /// The wall's root checker as setup installs it (ADR-0026): `wall check`
    /// on the stored rules and verdict file setup made, every few seconds.
    #[command(hide = true)]
    WallDaemon,
    /// Give VM jobs LAN or tailnet access, one IPv4 network and port at a time
    /// (ADR-0026). Changes run with sudo; `faber grant list` needs no root.
    Grant {
        #[command(subcommand)]
        action: faber::wall::grant_cmd::GrantCommand,
    },
    /// Build the golden macOS image for VM jobs (ADR-0049). `faber golden
    /// prepare` (as yourself, no sudo) installs macOS and opens an offline
    /// window for Setup Assistant; `sudo faber golden build` boots the prepared
    /// bundle on a network with no grants, lets it install Xcode and its tools,
    /// and imports the result.
    Golden {
        #[command(subcommand)]
        action: faber::setup::golden_build::GoldenCommand,
    },
    /// Remove what `sudo faber setup` created: the service, faber's files and
    /// its accounts. Run it with sudo; without sudo it prints the plan.
    Uninstall {
        /// Print the plan without changing anything.
        #[arg(long)]
        dry_run: bool,
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
        /// Copy audit.jsonl and config.toml to your home first.
        #[arg(long)]
        keep_logs: bool,
        /// How long to wait, in seconds, for admitted jobs to finish before
        /// the daemon is stopped.
        #[arg(long, default_value_t = 3600)]
        drain_timeout: u64,
        /// Stop the daemon without draining it first. Jobs it is running are
        /// orphaned. Always asks for confirmation, even with --yes.
        #[arg(long)]
        no_drain: bool,
    },
    /// Parse and validate a config file, then exit 0 (OK) or 1 (not OK).
    ///
    /// The autoupdater runs this with the *staged* binary against the live
    /// config before it swaps anything in. A release that added or renamed a
    /// config key would otherwise only reveal itself after the swap — as an
    /// `exit(1)` at startup, which under launchd's `KeepAlive` is an
    /// unbreakable crash-loop rather than an error message.
    CheckConfig {
        /// Config file to check (default: the standard app-support location).
        /// `-` reads the config from stdin, at most 1 MiB.
        #[arg(long)]
        config: Option<String>,
        /// Check only what a release needs to start on this config — parsing
        /// and shape, never policy. The autoupdater's gate: policy only ever
        /// tightens, and must never hold back or brick an unattended update.
        #[arg(long)]
        for_update: bool,
    },
    /// Read the audit log back: what faber did, what failed, and why.
    ///
    /// Works on a stopped daemon — the log outlives the process, which is the
    /// case this is for. A running daemon's live state is folded in when the
    /// control panel answers.
    Status {
        /// How far back to read: `90s`, `45m`, `24h`, `7d`.
        #[arg(long, default_value = "24h")]
        since: String,
        /// Only this `owner/repo`.
        #[arg(long)]
        repo: Option<String>,
        /// How many failed runs to print in full.
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Control-panel port to ask for live state.
        #[arg(long, default_value_t = 8787)]
        port: u16,
        /// Emit the digest as JSON.
        #[arg(long)]
        json: bool,
        /// Read this log instead of `~/Library/Logs/faber/audit.jsonl` — e.g. a
        /// trail copied off the machine that produced it.
        #[arg(long)]
        log: Option<String>,
    },
    /// Stand this host down for good: stop claiming, then wait for admitted
    /// runners to finish. Run this before `launchctl bootout` — since ADR-0011 a
    /// bare SIGTERM means "restarting", so booting out without draining leaves
    /// running containers with no lifetime cap and nobody to tear them down.
    Drain {
        /// TCP port of a daemon from before the control socket (ADR-0028); a
        /// current daemon is reached through its socket.
        #[arg(long, default_value_t = 8787)]
        port: u16,
        /// How long to wait for in-flight runners, in seconds. The default
        /// exceeds faber's own 3h runner lifetime cap, so a job cannot outlast it.
        #[arg(long, default_value_t = 11_400)]
        wait_secs: u64,
    },
    /// Hand the daemon its GitHub token (ADR-0027): `faber auth link`, then
    /// `sudo faber auth set`.
    Auth {
        #[command(subcommand)]
        command: AuthCommand,
    },
    /// Open the dashboard in the default browser through a one-time link.
    ///
    /// Asks the running daemon, over its control socket, for a link that works
    /// once and expires within a minute (ADR-0028). Only the operator's account,
    /// or root, may ask.
    Dashboard {
        /// TCP port of a daemon from before the control socket (ADR-0028).
        #[arg(long, default_value_t = 8787)]
        port: u16,
    },
    /// Change one key of the running daemon's config, or apply a whole file
    /// (issue #78). Routine and narrowing changes work as the operator; a
    /// change that widens trust needs sudo, and the refusal prints the command.
    Config {
        #[command(subcommand)]
        action: faber::config_cli::ConfigCommand,
    },
    /// Clear a repo's minting hold (ADR-0030). Widens trust: run it with sudo.
    RepoHold {
        #[command(subcommand)]
        action: faber::config_cli::RepoHoldCommand,
    },
    /// Control faber's Linux VM, the engine that serves its Docker.
    Engine {
        #[command(subcommand)]
        action: EngineCommand,
    },
    /// Print the release-verification policy compiled into this binary: who a
    /// follower accepts releases from (ADR-0031). The first line is
    /// `policy: <name>`; release builds print `policy: release`.
    ProvenancePolicy,
}

#[derive(Subcommand)]
enum EngineCommand {
    /// Stop the Linux VM through the VM supervisor and wait until it is
    /// stopped. Run it as `_faber`, the only account the supervisor serves
    /// (`sudo -u _faber`). While faber runs it starts the VM again, so stop
    /// faber first; `sudo faber uninstall` does both. The VM's disk, with the
    /// runner image on it, is never touched.
    Stop,
    /// Apply a pending grow of the Linux VM's disk now (ADR-0061): hold Docker
    /// claims, wait for the jobs in the VM to finish, and restart it at the new
    /// size. Asks the running daemon; no sudo. Nothing is killed, and nothing
    /// happens when no grow is pending.
    Grow,
}

#[derive(Subcommand)]
enum AuthCommand {
    /// Print the GitHub link that pre-fills a fine-grained token for this host,
    /// with only the permissions its enabled features need. Needs no token and
    /// no sudo.
    Link {
        /// The account whose repositories the token reaches. Asked of the
        /// running daemon when not given.
        #[arg(long)]
        owner: Option<String>,
        /// The token's lifetime in days, 1 to 366. A token that never expires
        /// is refused.
        #[arg(long, default_value_t = faber::credential::DEFAULT_DAYS)]
        days: u32,
        /// The config whose features decide the permissions (default: this
        /// host's).
        #[arg(long)]
        config: Option<String>,
        /// Control-panel port to ask the daemon for the owner.
        #[arg(long, default_value_t = 8787)]
        port: u16,
    },
    /// Store the token for the daemon. Run it with sudo. The token is read from
    /// a hidden prompt, or from stdin when it is piped, never from an argument.
    Set {
        /// The expiry date GitHub's token page shows, YYYY-MM-DD. Only for when
        /// GitHub's answer reports no expiry, which otherwise is refused as a
        /// token that never expires; ignored when GitHub reports one.
        #[arg(long, value_name = "YYYY-MM-DD")]
        expires: Option<String>,
        /// Control-panel port to watch for the daemon picking the token up.
        #[arg(long, default_value_t = 8787)]
        port: u16,
    },
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match cli().await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}", error_report(&e));
            std::process::ExitCode::FAILURE
        }
    }
}

/// What a failed command prints: `Error: ` and the error with its causes, as
/// returning it from `main` printed it, with every character a terminal would
/// act on made visible ([`faber::term`]). Errors quote text faber did not
/// write, such as a GitHub error body, a line of a config file, or a child's
/// output, and under ADR-0025 some of them end on root's terminal.
fn error_report(e: &anyhow::Error) -> String {
    format!("Error: {}", faber::term::sanitize(&format!("{e:?}")))
}

async fn cli() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_target(false)
        // Escapes the `\r` the fmt layer lets through (#82).
        .with_writer(faber::logline::Stderr)
        .compact()
        .init();
    let cli = Cli::parse();
    faber::out::set_color(cli.color);
    // ADR-0034: a CLI older than its daemon says so, once, on stderr. Not for
    // `run` (it is the daemon), `check-config` (the updater runs it on staged
    // binaries), `upgrade-cli` (it is the fix), `wall` (a root checker with
    // no daemon to ask), `grant` (root's files only; it asks no daemon) or `golden` (the build
    // runs with the daemon stopped or absent).
    if !matches!(
        cli.command,
        Command::Run { .. }
            | Command::CheckConfig { .. }
            | Command::UpgradeCli { .. }
            | Command::Wall { .. }
            | Command::WallDaemon
            | Command::Grant { .. }
            | Command::Golden { .. }
    ) {
        if let Some(line) = faber::upgrade_cli::skew_warning().await {
            eprintln!("{line}");
        }
    }
    match cli.command {
        Command::Run {
            once,
            topic,
            labels: _,
            interval,
            concurrency: _,
            port,
            config,
        } => {
            // ADR-0025, first of all: which layout this process runs in, and
            // whether it may run the daemon at all. Before any path is resolved,
            // any lock taken or any port bound — a stray per-user agent on a host
            // that now runs faber as `_faber` must touch none of them. Reading
            // the setup record cannot fail (an unreadable one is a reason, not an
            // error), and nothing here exits into a KeepAlive loop: a service
            // that must not run parks, and only root, which no service faber
            // installs runs as, is refused outright.
            let resolution = faber::platform::resolution();
            if let Some(warning) = resolution.warning() {
                tracing::warn!("{warning}");
            }
            match faber::standby::decide(resolution, faber::standby::euid(), once) {
                faber::standby::BootDecision::Run => {}
                faber::standby::BootDecision::Refuse(why) => {
                    tracing::error!("{why}");
                    std::process::exit(1);
                }
                faber::standby::BootDecision::Park(why) => {
                    match faber::standby::park(&why).await {}
                }
            }
            let layout = resolution.layout();
            let user = faber::standby::running_user();
            // This boot's standing conditions: logged now, and on the dashboard
            // for as long as the process lives.
            let mut notices = Vec::new();
            if let Some(notice) = faber::standby::legacy_notice(
                layout,
                cfg!(any(target_os = "macos", target_os = "linux")),
                &user,
            ) {
                tracing::warn!("{}", notice.text);
                notices.push(notice);
            }
            // ADR-0026 (amended 2026-10-21): a SYSTEM macOS daemon runs only
            // the pinned Docker CLI setup installs. Missing or not the pin, it
            // is said here and on the dashboard; every Docker call fails.
            #[cfg(target_os = "macos")]
            if layout == faber::platform::Layout::System {
                if let Some(text) = faber::setup::docker_cli::installed_problem(
                    std::path::Path::new(faber::platform::docker_cli()),
                    &faber::setup::docker_cli::PIN,
                ) {
                    tracing::warn!("{text}");
                    notices.push(faber::reconcile::Notice {
                        kind: "docker_cli".to_string(),
                        text,
                    });
                }
                // The CLI's plugin directories: `docker build` and `builder
                // prune` are refused while one is not root's alone.
                if let Some(text) = faber::setup::docker_cli::plugin_gate() {
                    tracing::warn!("{text}");
                    notices.push(faber::reconcile::Notice {
                        kind: "docker_cli_plugins".to_string(),
                        text,
                    });
                }
            }

            // Autoupdate's rescue path, deliberately the first thing this
            // command does after the guard above — before the config is read,
            // before a client is built, before anything that can fail. A binary
            // that dies on config load must still be able to roll itself back,
            // and under launchd's `KeepAlive` a startup failure is a crash-loop
            // with no operator in it. See ADR-0006.
            // `run --once` is a single manual cycle, not the service: it would
            // exit long before probation elapsed and leave the marker looking
            // like a crash, so it sits this out entirely.
            let update_paths = faber::update::UpdatePaths::from_home().filter(|_| !once);
            // Resolved here, not later, because the rescue below has to know
            // which file it would restore a config snapshot *into*. This is
            // pure path arithmetic — it reads nothing — so it is safe this
            // early, and threading the one path through means a host started
            // with `--config elsewhere.toml` rolls back the config it is
            // actually running, not the one at the default location.
            let live_config_path = faber::config::FileConfig::resolve_path(
                config.as_deref().map(std::path::Path::new),
            )
            .unwrap_or_else(|| {
                tracing::warn!("no HOME: settings will be read/written next to the daemon");
                std::path::PathBuf::from("faber-config.toml")
            });
            let mut rolled_back_from = None;
            let mut on_probation = false;
            if let Some(paths) = &update_paths {
                if let Some(report) = faber::update::rollback_if_needed(
                    paths,
                    env!("FABER_VERSION"),
                    &live_config_path,
                ) {
                    if report.restored {
                        tracing::error!(
                            "update to {} failed to stay up — restored {} and quarantined {}; \
                             exiting so launchd starts the restored binary",
                            report.to,
                            report.from,
                            report.to
                        );
                        return Ok(());
                    }
                    match &report.note {
                        Some(note) => tracing::warn!("update marker: {note}"),
                        // No note means this boot *is* the probation: the marker
                        // was ours and its attempt count has just been bumped.
                        None => on_probation = true,
                    }
                }
                rolled_back_from = faber::update::take_rollback_breadcrumb(paths);
                if let Some(failed) = &rolled_back_from {
                    tracing::warn!(
                        "this host rolled back from {failed}; it is quarantined and will not be re-installed"
                    );
                }
            }

            // Parsed before GitHub is asked anything: a release that cannot read
            // its own config must fail here, where ADR-0006's rollback sees the
            // crash, and not sit in standby looking alive.
            let file_config =
                faber::config::FileConfig::load(config.as_deref().map(std::path::Path::new))?;

            // The control API's front door (ADR-0028), one for the life of the
            // process: standby's stub and the live dashboard both serve through
            // it, so a dashboard session opened during standby survives the boot.
            // The operator is the setup record's under SYSTEM, and under LEGACY
            // whoever the daemon runs as.
            let operator_uid = resolution
                .record()
                .map_or_else(faber::standby::euid, |record| record.operator.uid);
            let control_socket = faber::platform::control_socket();
            if control_socket.is_none() {
                tracing::warn!(
                    "no state directory: the control socket is off, so nothing can \
                     open a dashboard session or use the control API"
                );
            }
            let door = faber::control::FrontDoor::new(
                port,
                control_socket,
                faber::control::socket_mode(layout),
                faber::control::socket_group(resolution),
                operator_uid,
                Arc::new(faber::control::Sessions::system()),
            );

            // SIGTERM and SIGINT, listened for from here until the process exits:
            // through standby and the main loop alike, so that a stop arriving
            // between the two is still a deliberate, clean exit (ADR-0011 §4).
            // `run --once` keeps Ctrl-C's default action.
            let mut stops = (!once).then(faber::standby::Stops::listen);

            // What only GitHub can give: a credential, the runner version (for
            // a macOS VM job that pins none), and the account login. A failure used to be
            // fatal, which under `KeepAlive` is a crash loop and, after an update,
            // a rollback of a release that did nothing wrong. Now the daemon
            // waits in standby instead, answering its control API with the reason
            // (ADR-0025). `run --once` is a person at a terminal, so it still fails.
            let ready = if once {
                faber::standby::prerequisites(layout, user.clone()).await?
            } else {
                let stops = stops
                    .as_mut()
                    .expect("stops are listened for unless --once");
                let audit_path = faber::platform::log_dir()
                    .map(|dir| dir.join("audit.jsonl").display().to_string())
                    .unwrap_or_default();
                let waited = faber::standby::until_ready_or_signalled(
                    faber::standby::StandbyOptions {
                        door: door.clone(),
                        retry: faber::standby::RETRY,
                        topic: file_config
                            .as_ref()
                            .map_or_else(|| topic.clone(), |fc| fc.topic.clone()),
                        audit_path,
                        notices: notices.clone(),
                        // A binary the updater just installed must not wait out
                        // failures it may be causing itself: that would strand
                        // the host on it, since the updater and the end of
                        // probation both come after this.
                        probation: on_probation,
                        // Tells a network outage from a broken client, without
                        // this binary's HTTP stack.
                        reachable: faber::standby::github_reachable,
                    },
                    || faber::standby::prerequisites(layout, user.clone()),
                    stops.next(),
                    // Where a signal's clean-exit mark goes, as at shutdown.
                    update_paths.clone(),
                )
                .await;
                match waited {
                    Ok(ready) => ready,
                    // ADR-0006's rollback counts boots that end without the
                    // clean-exit mark, and restores the previous release at the
                    // third. So this exit writes no mark (`note_clean_exit`):
                    // failing probation is what it is for.
                    Err(faber::standby::Stop::GaveUp(gave_up)) => {
                        tracing::error!("{gave_up}");
                        std::process::exit(1);
                    }
                    // A deliberate stop: standby has written the clean-exit mark,
                    // so the next boot is not counted against probation.
                    Err(faber::standby::Stop::Signalled) => std::process::exit(0),
                }
            };
            let faber::standby::Ready {
                gh,
                runner_version,
                owner,
            } = ready;
            // ADR-0027: say at boot when the token expires soon.
            {
                let credential = gh.credential_view();
                if credential.status == faber::credential::Status::Expiring {
                    if let Some(reason) = &credential.reason {
                        tracing::warn!("{reason}");
                    }
                }
                // ADR-0027: a token file `_faber` could rewrite, or one others
                // can read through its group on a host set up before the
                // operator group, is used, but said at every boot until it is
                // put right.
                if let Some(warning) = &credential.file_warning {
                    tracing::warn!("GitHub token: {warning}");
                }
            }

            // Machine-local runtime tuning (concurrency + container limits) set
            // via the control panel, layered over the file/flag defaults so a
            // restart preserves it. Absent fields fall back to those defaults.
            let mut overrides = faber::config::Overrides::load();
            // A zero ceiling is a hold from before faber had holds (issue #66).
            // Read as one here, ahead of both `resolve` calls below, so it never
            // reaches the limiter as a number: admission stays stopped, but now
            // with a reason, a date and a Release control. Nothing is written back.
            let written_ms =
                faber::config::Overrides::written_ms().unwrap_or_else(faber::audit::now_ms);
            if overrides.migrate_zero_ceiling(written_ms) {
                tracing::info!(
                    "overrides.json: hard_ceiling = 0 read as a hold on Docker admission; \
                     the config file's ceiling applies"
                );
            }

            // Where the settings page reads and writes. Resolved even when no file
            // exists yet — saving from the UI is what creates the first one.
            let config_path = live_config_path.clone();

            // `boot` is the config this process actually runs — from the file when
            // there is one, synthesized from the flags when there isn't. The
            // settings page diffs saved edits against it to say what a restart
            // would change.
            let (mut cfg, sweep_secs, boot, boot_from_file) = match file_config {
                // A config file drives the routing profiles + daemon settings.
                Some(fc) => {
                    let boot = fc.clone();
                    let profiles = fc.into_profiles(&runner_version)?;
                    let sweep_secs = fc.sweep_interval_secs;
                    // Autolimiter config = file defaults with machine-local
                    // overrides layered on; pinned caps come from overrides.
                    let autolimit = fc.autolimit.resolve(&overrides);
                    let services = fc.services_policy();
                    let mirror = fc.mirror_policy();
                    let vm_layout = fc.vm_layout();
                    (
                        faber::reconcile::Config {
                            topic: fc.topic,
                            poll_interval: Duration::from_secs(fc.poll_interval_secs),
                            autolimit,
                            pinned_caps: overrides.pinned_caps.clone(),
                            hold: overrides.hold.clone(),
                            repo_holds: overrides.repo_holds.clone(),
                            profiles,
                            playwright_drift: fc.playwright_drift,
                            retry: fc.retry,
                            waste: fc.waste,
                            host: fc.host,
                            runner_image: fc.runner_image,
                            needs: fc.needs,
                            provisioning: fc.provisioning,
                            services,
                            mirror,
                            vm_layout,
                        },
                        sweep_secs,
                        boot,
                        true,
                    )
                }
                // No config: no profiles. This path used to synthesize a native
                // profile from `--labels`, and that backend is gone (ADR-0026,
                // ADR-0052); nothing else can be built from flags alone, so the
                // daemon runs, serves its dashboard and claims nothing until a
                // config with a profile is saved. `--labels` and `--concurrency`
                // are still parsed, and ignored.
                None => {
                    let profiles = Vec::new();
                    let autolimit = faber::config::AutoLimitFile::default().resolve(&overrides);
                    // The same settings, in config-file shape: what the page
                    // shows, and what Save would write as the first file.
                    let boot = faber::config::FileConfig {
                        topic: topic.clone(),
                        poll_interval_secs: interval,
                        ..Default::default()
                    };
                    (
                        faber::reconcile::Config {
                            topic,
                            poll_interval: Duration::from_secs(interval),
                            autolimit,
                            pinned_caps: overrides.pinned_caps.clone(),
                            hold: overrides.hold.clone(),
                            repo_holds: overrides.repo_holds.clone(),
                            profiles,
                            // No config file: the global default (surface).
                            playwright_drift: faber::config::DriftPolicy::default(),
                            // No config file: auto-retry stays off (opt-in).
                            retry: faber::config::RetryPolicy::default(),
                            // No config file: every waste signal off (ADR-0063).
                            waste: Default::default(),
                            host: faber::config::HostPolicy::default(),
                            // No config file: one static image per profile,
                            // faber's historical behavior (ADR-0008 is opt-in).
                            runner_image: faber::config::RunnerImagePolicy::default(),
                            needs: Default::default(),
                            // Demand mode is a shared-pool, config-file feature;
                            // the flags-only path keeps the historical per-job behavior.
                            provisioning: faber::config::ProvisioningMode::default(),
                            // No config file: no `[services.*]`, so no services.
                            services: Default::default(),
                            // No config file: no `[mirror]`, so no mirror.
                            mirror: None,
                            // No config file: today's VM layout.
                            vm_layout: Default::default(),
                        },
                        faber::config::DEFAULT_SWEEP_SECS,
                        boot,
                        false,
                    )
                }
            };
            // ADR-0051: the profiles the VM layout starts no VM for, dropped
            // loudly. The Linux VM's switch matters only where Docker is faber's
            // own VM: a set-up Mac.
            let has_engine = cfg!(target_os = "macos") && faber::platform::setup_record().is_some();
            let (profiles, dropped) = faber::config::ignore_vm_layout(
                std::mem::take(&mut cfg.profiles),
                cfg.vm_layout,
                has_engine,
            );
            cfg.profiles = profiles;
            for (kind, text) in dropped.notices() {
                tracing::warn!("{text}");
                notices.push(faber::reconcile::Notice {
                    kind: kind.to_string(),
                    text,
                });
            }
            let linux_vm_off = has_engine && !cfg.vm_layout.linux_vm;
            // The lifetime cap the dropped docker profiles would have applied to
            // the runners a switched-off VM still runs: top-level, one value for
            // every docker profile.
            let off_runner_timeout = boot
                .runner_timeout_secs
                .map_or(faber::docker::DEFAULT_RUNNER_TIMEOUT, Duration::from_secs);
            for warning in
                faber::config::vm_layout_warnings(&boot, cfg!(target_os = "linux"), has_engine)
            {
                tracing::warn!("{warning}");
            }
            if let Some(warning) = faber::config::recreatable_ignored_warning(&boot.host) {
                tracing::warn!("{warning}");
            }
            // ADR-0046: a `[services.*]` table that does not resolve is left out
            // of the policy, and an image faber may not pull fails every job
            // that declares it. Warned, never refused (AGENTS.md).
            if let Some(warning) = faber::config::services_ignored_warning(&boot) {
                tracing::warn!("{warning}");
            }
            for warning in faber::config::services_pull_warnings(&boot) {
                tracing::warn!("{warning}");
            }
            for warning in faber::config::mirror_warnings(&boot) {
                tracing::warn!("{warning}");
            }
            // The mirrors that carry faber's GitHub token stay off where a job
            // is, or was, root on the engine that would hold it
            // (docs/git-mirror.md, "Not beside dind"). Warned and shown at every
            // boot, never a refusal to start (AGENTS.md).
            let dind_marker = faber::mirror_beside_dind::marker_path();
            let beside_dind = faber::mirror_beside_dind::at_boot(
                &boot,
                dind_marker.as_deref(),
                faber::audit::now_ms(),
            );
            beside_dind.apply(&mut cfg.profiles, &mut cfg.mirror);
            if let Some(text) = beside_dind.warning(dind_marker.as_deref()) {
                tracing::warn!("{text}");
                notices.push(faber::reconcile::Notice {
                    kind: faber::mirror_beside_dind::NOTICE_KIND.to_string(),
                    text,
                });
            }
            if let Some(problem) =
                faber::docker::password_env_problem(&boot.registry_auth.password_env)
            {
                tracing::warn!("registry auth is DISABLED until this is fixed: {problem}");
            }
            if let Some(warning) =
                faber::standby::registry_secret_warning(layout, &boot.registry_auth)
            {
                tracing::warn!("{warning}");
            }
            // ADR-0028 (#238): warn, never refuse to boot. Each variant build
            // from this recipe is refused until it is moved under root.
            if let Some(problem) = faber::image::recipe_problem(&boot.runner_image) {
                let text =
                    format!("runner-image variants will NOT be built from this recipe: {problem}");
                tracing::warn!("{text}");
                notices.push(faber::reconcile::Notice {
                    kind: "recipe_untrusted".to_string(),
                    text,
                });
            }
            // ADR-0059 §3: dispatches made with a job's own token, turned back
            // on. Resolving the profiles warns of it; this keeps it on the
            // dashboard for the life of the process.
            if let Some(text) = faber::config::job_token_dispatches_warning(&boot.gate) {
                notices.push(faber::reconcile::Notice {
                    kind: "job_token_dispatches".to_string(),
                    text,
                });
            }

            // What the enabled features need of the token (ADR-0027), so a
            // refusal names the feature it turns off, and the hourly probe
            // checks the reads they need.
            gh.set_needs(faber::credential::required(&boot));
            // Captured before `boot` and `config_path` are handed to the
            // settings store: the updater needs the policy it was booted with,
            // and the path it must prove a staged binary can still parse.
            if let Some(warning) = boot.host.ignored_keys_warning() {
                tracing::warn!("{warning}");
            }
            if let Some(warning) = boot.host.deprecated_sleep_key_warning() {
                tracing::warn!("{warning}");
            }
            // ADR-0026: the size faber's Linux VM starts with, where it runs one.
            let (engine_cfg, engine_warnings) =
                faber::vm::engine::EngineConfig::from_file(&boot.linux_vm);
            for warning in engine_warnings {
                tracing::warn!("{warning}");
            }
            // ADR-0061 §7: the mirror's cache volume counts at its limit.
            let mirror_gib = boot.mirror.as_ref().map(|m| u64::from(m.max_size_gib));
            let update_policy = boot.update.clone();
            let update_config_path = config_path.clone();
            let settings = Arc::new(faber::settings::SettingsStore::new(
                config_path,
                boot,
                boot_from_file,
            ));
            // The janitor only matters where per-job Docker sandboxes exist.
            let wanted_images: Vec<String> = cfg
                .profiles
                .iter()
                .filter_map(|p| match &p.backend {
                    faber::backend::Backend::Docker(d) => Some(d.image.clone()),
                    _ => None,
                })
                .collect();
            let sweeps_docker = cfg
                .profiles
                .iter()
                .any(|p| matches!(p.backend, faber::backend::Backend::Docker(_)));
            // macOS VMs outlive a restart (ADR-0026): adopted and swept only where a
            // `macos-vm` profile could have started one.
            let runs_vms = cfg
                .profiles
                .iter()
                .any(|p| matches!(p.backend, faber::backend::Backend::MacosVm(_)));
            // ADR-0049 §7: the profiles whose golden images are verified here.
            let vm_profiles: Vec<faber::backend::Profile> = cfg
                .profiles
                .iter()
                .filter(|p| matches!(p.backend, faber::backend::Backend::MacosVm(_)))
                .cloned()
                .collect();
            let orch = faber::reconcile::Orchestrator::new(gh.clone(), owner, cfg).await?;

            // ADR-0011 §4 — exactly one faber may provision on this host. Taken
            // before anything can mint or adopt, and held by an open descriptor
            // for the whole life of the process: the kernel drops it on exit or
            // crash alike, so there is no stale lock to detect or clean up.
            let _host_lock = match faber::lock::default_path() {
                Some(path) => {
                    let held = faber::lock::acquire(&path)?;
                    tracing::debug!("host lock held at {}", held.path().display());
                    Some(held)
                }
                None => {
                    tracing::warn!(
                        "no HOME to place the host lock in — running unlocked. A second faber \
                         on this machine would double-watch every container."
                    );
                    None
                }
            };

            // ADR-0011 — rebuild in-flight claims from the containers already
            // running here. Before the reconcile loop, so nothing is claimed
            // against an empty ledger, and before the janitor, whose startup
            // sweep would otherwise reap the very corpses adoption turns into
            // outcome records.
            if sweeps_docker {
                orch.limiter().hold_for_adoption();
                match orch.adopt().await {
                    Some(n) => {
                        if n > 0 {
                            tracing::info!(
                                "adopt: rebuilt {n} in-flight claim(s) from running containers \
                                 — no drain was needed"
                            );
                        }
                        orch.limiter().adoption_settled();
                    }
                    None => {
                        // Docker could not be read. That is not "nothing is
                        // running", so admission stays shut and this retries in
                        // the background — the dashboard comes up either way, and
                        // says why nothing is starting.
                        tracing::warn!(
                            "adopt: cannot read Docker — holding admission shut until it answers \
                             (ADR-0011 §5). Nothing already running is affected."
                        );
                        let retry = orch.clone();
                        tokio::spawn(async move {
                            loop {
                                tokio::time::sleep(Duration::from_secs(15)).await;
                                if let Some(n) = retry.adopt().await {
                                    tracing::info!(
                                        "adopt: Docker answered — rebuilt {n} claim(s); \
                                         admission open"
                                    );
                                    retry.limiter().adoption_settled();
                                    return;
                                }
                            }
                        });
                    }
                }
            }
            // ADR-0026 — the same for VMs: take back what the supervisor runs
            // before any VM job can lease a slot and before the first VM sweep,
            // which would otherwise stop the very VMs this adopts.
            //
            // ADR-0051: where the record names the session account, macOS VM
            // jobs run through its session's supervisor, and wait while that
            // does not answer ("no VM session"). Its account is checked and
            // its supervisor probed once first, so adoption's `list` goes to a
            // supervisor already trusted. Without the account, nothing is
            // gated.
            #[cfg(target_os = "macos")]
            if runs_vms && faber::vm::session::start_for_this_host(orch.limiter().clone()).await {
                tracing::info!(
                    "macOS VM jobs run through the session account's supervisor at {}",
                    faber::platform::session_supervisor_socket().display()
                );
            }
            if runs_vms {
                let vm_jobs = faber::vm::job::global();
                vm_jobs.hold_for_adoption();
                match orch.adopt_vms().await {
                    Some(n) => {
                        if n > 0 {
                            tracing::info!(
                                "adopt: rebuilt {n} in-flight claim(s) from running macOS VMs"
                            );
                        }
                        vm_jobs.adoption_settled();
                    }
                    None => {
                        // Not "no VM is running": no VM job starts until it answers.
                        tracing::warn!(
                            "adopt: cannot reach the VM supervisor — no macOS VM job starts \
                             until it answers. VMs already running are not affected."
                        );
                        let retry = orch.clone();
                        tokio::spawn(async move {
                            loop {
                                tokio::time::sleep(Duration::from_secs(15)).await;
                                if let Some(n) = retry.adopt_vms().await {
                                    tracing::info!(
                                        "adopt: the VM supervisor answered — rebuilt {n} \
                                         claim(s); macOS VM jobs may start"
                                    );
                                    faber::vm::job::global().adoption_settled();
                                    return;
                                }
                            }
                        });
                    }
                }
            }
            if once {
                faber::reconcile::run_loop(orch, true).await?;
            } else {
                // The updater reads public releases anonymously and never
                // borrows the CI token (ADR-0027): two checks an hour at the
                // default poll, a handful more per release, well inside
                // GitHub's 60 anonymous requests an hour.
                let update_gh = Arc::new(faber::github::Github::anonymous()?);
                // Autoupdate (ADR-0006). The handle exists whatever the policy
                // says — the panel still has to be able to report "disabled",
                // and a host with no HOME to stage into is exactly the host that
                // most needs to be told why it will never update itself.
                let (updater, update_handle) = match &update_paths {
                    Some(paths) => faber::update::Updater::new(
                        update_gh.clone(),
                        orch.clone(),
                        update_policy.clone(),
                        paths.clone(),
                        update_config_path,
                        env!("FABER_VERSION").to_string(),
                        rolled_back_from,
                    ),
                    None => {
                        if update_policy.enabled {
                            tracing::warn!(
                                "autoupdate is enabled but there is no HOME to stage into — staying on this binary"
                            );
                        }
                        let mut disabled = update_policy.clone();
                        disabled.enabled = false;
                        faber::update::Updater::new(
                            update_gh.clone(),
                            orch.clone(),
                            disabled,
                            faber::update::UpdatePaths::with_root(
                                std::env::temp_dir().join("faber"),
                            ),
                            update_config_path,
                            env!("FABER_VERSION").to_string(),
                            rolled_back_from,
                        )
                    }
                };
                tokio::spawn(updater.run_loop());

                // End of probation: this process has been up long enough that
                // the update which installed it counts as good. Until the marker
                // is gone, a second boot into this same version reads as a
                // crash-loop and rolls back — which is precisely what should
                // happen if we never reach this point.
                if on_probation {
                    if let Some(paths) = update_paths.clone() {
                        let probation = Duration::from_secs(update_policy.probation_secs);
                        let repo = update_policy.repo.clone();
                        let handle = update_handle.clone();
                        // The Updater's own client, anonymous like it (#92).
                        let self_check_gh = update_gh.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(probation).await;
                            // ADR-0026: a release that ships the VM launcher must
                            // also prove it is this build and holds the wall. A
                            // definite failure arms the same rollback below.
                            let proof =
                                faber::update::prove_launcher(&paths, env!("FABER_VERSION")).await;
                            if proof == faber::update::LauncherProof::Armed {
                                std::process::exit(1);
                            }
                            // Surviving is not enough (#92): the binary must also
                            // prove it can still verify the release it came from.
                            // `prove_updater` clears the marker itself when that is
                            // settled, and arms the rollback when it is refused.
                            let settled = faber::self_check::prove_updater(
                                &paths,
                                &handle,
                                &self_check_gh,
                                &repo,
                                env!("FABER_VERSION"),
                            )
                            .await;
                            if let faber::self_check::Settled::Refused(_) = settled {
                                // The marker now reads as a crash-loop. Exit so the
                                // supervisor restarts us, and the boot-time rollback
                                // restores the previous binary. Running containers
                                // are adopted across the restart (ADR-0011).
                                std::process::exit(1);
                            }
                            // Only now, with probation settled: a rollback after
                            // this point would otherwise strand the supervisor on
                            // a launcher the daemon was rolled back from.
                            faber::update::restart_supervisor_after_proof(
                                proof,
                                env!("FABER_VERSION"),
                                &handle,
                            )
                            .await;
                            tracing::info!(
                                "update to {} survived probation",
                                env!("FABER_VERSION")
                            );
                        });
                    }
                }

                // ADR-0028 addendum B: the week's allowed trust-widening changes,
                // seeded from the audit log the orchestrator just opened, so a
                // restart cannot take one off the notice.
                let trust_changes = Arc::new(match faber::audit::Audit::open() {
                    Ok(audit) => faber::audit::TrustChanges::from_audit(&audit),
                    Err(e) => {
                        tracing::warn!("trust-change notice not seeded from the audit log: {e}");
                        faber::audit::TrustChanges::new(Arc::new(faber::audit::now_ms))
                    }
                });
                let attention = Arc::new(faber::attention::AttentionStore::load(
                    faber::attention::AttentionStore::default_path(),
                ));
                let web = orch.clone();
                tokio::spawn(async move {
                    if let Err(e) = faber::web::serve(
                        web,
                        door,
                        settings,
                        update_handle,
                        notices,
                        trust_changes,
                        attention,
                    )
                    .await
                    {
                        tracing::error!("dashboard failed: {e}");
                    }
                });
                // ADR-0026: judge the Linux VM image setup built against this
                // release's pin, now and every few minutes, for `/api/state`.
                // Display only; Linux hosts run no VMs and leave it unknown.
                #[cfg(target_os = "macos")]
                tokio::spawn(faber::setup::linux_image::watch(orch.limiter().clone()));
                // ADR-0026: on a set-up Mac faber's Docker is its own Linux VM, kept
                // in service by the engine driver; until it serves, Gate 0 reads
                // Docker as unreachable with the driver's reason. Only where a
                // Docker profile needs it: the VM holds its memory while it runs.
                #[cfg(target_os = "macos")]
                let engine_spawned = sweeps_docker
                    && faber::vm::engine::spawn_for_this_host(
                        orch.limiter().clone(),
                        engine_cfg,
                        wanted_images,
                    );
                // ADR-0051: with the Linux VM switched off, a VM a previous boot
                // left running is stopped once no docker job it still runs is left
                // (ADR-0011), never deleted: its disk holds the only copy of the
                // runner image. The share counts it until it is stopped.
                #[cfg(target_os = "macos")]
                if linux_vm_off {
                    let retire = orch.clone();
                    faber::vm::engine::keep_off_for_this_host(
                        orch.limiter().clone(),
                        engine_cfg,
                        move || {
                            let retire = retire.clone();
                            async move { retire.retire_unwatched(off_runner_timeout).await }
                        },
                    );
                }
                #[cfg(not(target_os = "macos"))]
                let _ = (linux_vm_off, off_runner_timeout);
                #[cfg(target_os = "macos")]
                if engine_spawned {
                    // The Linux VM's row in `/api/state` carries its size.
                    orch.limiter()
                        .set_linux_vm_size(engine_cfg.cpus.get(), engine_cfg.memory_mib.get());
                    // Docker now runs behind the wall: its admission waits on
                    // the wall's verdict as VM admission does.
                    orch.limiter().set_wall_required(true);
                    tracing::info!(
                        "engine: keeping faber's Linux VM in service ({} vCPUs, {} MiB)",
                        engine_cfg.cpus.get(),
                        engine_cfg.memory_mib.get()
                    );
                    // ADR-0061: its disk grows within `[linux_vm] disk_max_gib`.
                    let policy = orch.host_policy();
                    faber::vm::engine::spawn_grow_for_this_host(
                        orch.clone(),
                        faber::vm::grow::GrowBoot {
                            ceiling_gib: engine_cfg.disk_max_gib,
                            floor_gib: policy.disk_floor_gib,
                            mirror_gib,
                            cache_max_gib: policy.cache_max_gib,
                            runner_timeout: off_runner_timeout,
                        },
                    );
                }
                #[cfg(not(target_os = "macos"))]
                let _ = mirror_gib;
                // ADR-0026: the root checker's verdict, polled into the
                // limiter's wall gate, wherever something waits on it: VM jobs,
                // or Docker served from faber's own VM. Until the first read the
                // gate is unread, and closed.
                #[cfg(target_os = "macos")]
                if runs_vms || engine_spawned {
                    tokio::spawn(faber::wall::reader::watch(orch.limiter().clone()));
                }
                // ADR-0049 §7: only a verified golden image runs jobs. The
                // daemon verifies each image a profile names, records the pass
                // against its file hashes and the host's macOS build, and
                // re-verifies when either changes.
                #[cfg(target_os = "macos")]
                if runs_vms {
                    faber::vm::verify::spawn_for_this_host(
                        orch.limiter().clone(),
                        gh.clone(),
                        orch.instance_id().to_string(),
                        &vm_profiles,
                    );
                }
                #[cfg(not(target_os = "macos"))]
                let _ = (engine_cfg, &wanted_images, &vm_profiles);
                // The janitor: an immediate sweep reclaims anything a previous
                // process (or a Docker restart) leaked, then it recurs.
                if sweeps_docker && sweep_secs > 0 {
                    let janitor = orch.clone();
                    tokio::spawn(faber::sweep::run_loop(
                        janitor,
                        Duration::from_secs(sweep_secs),
                    ));
                }
                // The VM sweep (ADR-0026): stops VMs of this host's own that no job
                // holds, and frees each slot held for a VM once it is gone. Its own
                // loop, since the janitor runs only where Docker profiles exist; each
                // pass waits for boot adoption to have answered. It deletes nothing:
                // the supervisor deletes clones, by their marker.
                if runs_vms {
                    tokio::spawn(faber::vm::adopt::sweep_loop(
                        orch.instance_id().to_string(),
                        orch.limiter().clone(),
                    ));
                    tokio::spawn(faber::vm::adopt::list_poll_loop(
                        faber::vm::job::ProductionSupervisor::for_macos_jobs(),
                        faber::vm::proto::VmKind::Macos,
                        orch.limiter().clone(),
                    ));
                }
                // The critical disk floor (ADR-0032 §2): below 3 GiB free on the
                // Docker volume it freezes the newest Docker job. Its own loop,
                // apart from the janitor's two-minute tick, and not tied to the
                // sweep interval. Where the admission floor is unstated or 0 it
                // never freezes, but still thaws a job it finds frozen.
                if sweeps_docker {
                    tokio::spawn(faber::freeze::run_loop(orch.clone()));
                }
                // The git mirrors' refresh timer (ADR-0035 §1). Only when a
                // docker profile set `git_mirror = true`; absent, nothing runs.
                if faber::git_mirror::global().enabled() {
                    tokio::spawn(faber::git_mirror::run_loop(orch.clone()));
                }
                // While those mirrors are off beside dind, the volumes that held
                // or steered faber's token are deleted, by their labels, once
                // Docker answers and nothing mounts them.
                if beside_dind.refused() && sweeps_docker {
                    tokio::spawn(faber::mirror_beside_dind::clean(
                        faber::mirror_beside_dind::DockerVolumes,
                    ));
                }
                // The pull-through mirror's janitor. Only with `[mirror]`;
                // absent, nothing runs and no Docker call is made for it.
                if faber::mirror::global().enabled() {
                    tokio::spawn(faber::mirror::run_loop(orch.clone()));
                } else if sweeps_docker {
                    // Absent is off: retire a faber-labelled mirror container a
                    // previous config left, once no job network holds it. One
                    // labelled listing at boot on a host that never had one.
                    tokio::spawn(faber::mirror::retire_lingering());
                }
                // Hold off idle sleep while a job's runner is up (issue #90). Off
                // unless the operator set `[host] prevent_sleep`.
                let sleep = orch.host_policy().sleep_policy();
                if sleep != faber::config::SleepPolicy::Off {
                    tokio::spawn(faber::awake::run_loop(orch.clone(), sleep));
                }
                // Run the loop until a shutdown signal, then drain in-flight
                // runners before exiting so a stop/restart never orphans a job.
                // A second signal during the drain forces an immediate exit.
                let loop_orch = orch.clone();
                // The same listener standby used, so a stop that arrived while
                // booting is seen here at once. Decided in the select, handled
                // after it: the second signal waits on that listener too.
                let stops = stops
                    .as_mut()
                    .expect("stops are listened for unless --once");
                let stopped = tokio::select! {
                    res = faber::reconcile::run_loop(loop_orch, false) => {
                        res?;
                        false
                    }
                    () = stops.next() => true,
                };
                if stopped {
                    // ADR-0011 §3 — SIGTERM means "I'm coming back".
                    // `launchctl bootout` and `kickstart -k` send the same
                    // bare signal, so rather than guess which one this is,
                    // the meaning is declared: a restart. Decommissioning is
                    // made explicit instead (`faber drain`), which is honest
                    // — it is already a deliberate human act.
                    //
                    // Either way this exit is on purpose, so the boot that
                    // follows must not be counted against update probation.
                    if let Some(paths) = &update_paths {
                        faber::update::note_clean_exit(paths);
                    }
                    if orch.adoptable().await {
                        let n = orch.view().await.inflight.len();
                        tracing::info!(
                            "shutdown: exiting now — {n} in-flight claim(s) stay running and \
                             are adopted by the next process (ADR-0011). Nothing drains."
                        );
                    } else {
                        tracing::info!("shutdown: refusing new jobs, draining in-flight runners");
                        orch.begin_drain(faber::reconcile::Drain::Stop).await;
                        tokio::select! {
                            _ = orch.await_drain() => tracing::info!("drain complete — exiting cleanly"),
                            () = stops.next() =>
                                tracing::warn!("second signal — forcing exit, in-flight runners orphaned"),
                        }
                    }
                }
            }
        }
        Command::Setup {
            dry_run,
            verbose,
            yes,
            skip_keep_awake,
            drain_timeout,
            replace_daemon,
            remigrate,
            migrate_from,
            fresh,
            keep_legacy,
            as_runner,
            skip_cli_upgrade,
            launcher,
            golden_image,
            golden_name,
            replace_golden,
            runner_image_tar,
            before_vm_runtime,
            check_vm_runtime,
            macos_session,
            macos_session_visibility,
            macos_session_password,
        } => {
            if as_runner {
                anyhow::bail!("faber runs as _faber now: run `sudo faber setup`");
            }
            // ADR-0034: the CLI copy follows the daemon first. On a promotion
            // this re-executes the new copy and does not return.
            if !dry_run
                && !check_vm_runtime
                && !skip_cli_upgrade
                && faber::platform::record::current_euid() == 0
            {
                faber::upgrade_cli::before_setup(yes).await;
            }
            faber::setup::run(faber::setup::SetupOpts {
                dry_run,
                verbose,
                yes,
                keep_awake: !skip_keep_awake,
                drain_timeout: Duration::from_secs(drain_timeout),
                replace_daemon,
                remigrate,
                migrate_from,
                fresh,
                keep_legacy,
                vm: faber::setup::vm_runtime::VmOpts {
                    launcher,
                    golden_image,
                    golden_name,
                    replace_golden,
                    runner_image_tar,
                },
                before_vm_runtime,
                check_vm_runtime,
                session: faber::setup::session::SessionOpts {
                    requested: macos_session.is_some()
                        || macos_session_visibility.is_some()
                        || macos_session_password.is_some(),
                    mode: macos_session.flatten(),
                    visibility: macos_session_visibility,
                    password: macos_session_password,
                },
            })
            .await?;
        }
        Command::Uninstall {
            dry_run,
            yes,
            keep_logs,
            drain_timeout,
            no_drain,
        } => {
            faber::setup::uninstall(faber::setup::UninstallOpts {
                dry_run,
                yes,
                keep_logs,
                drain_timeout: Duration::from_secs(drain_timeout),
                no_drain,
            })
            .await?;
        }
        Command::CheckConfig { config, for_update } => {
            // The machine's share is checked against the machine this runs on,
            // which is the one the file configures (ADR-0021).
            let machine = faber::platform::host::read_machine().await;
            match check_config(
                config.as_deref(),
                for_update,
                machine.as_ref(),
                std::io::stdin().lock(),
            ) {
                Ok(verdict) => println!("faber: {verdict}"),
                // The config's own text comes back in both kinds of error: a
                // parse error quotes the offending line, and a check names the
                // values it rejects. Under SYSTEM the file is `_faber`'s to
                // write, so neither reaches the terminal as it is
                // ([`faber::term`]).
                Err(errors) => {
                    for e in &errors {
                        eprintln!("faber: {}", faber::term::sanitize(e));
                    }
                    std::process::exit(1);
                }
            }
        }
        Command::Status {
            since,
            repo,
            limit,
            port,
            json,
            log,
        } => {
            // The layout's own log (`platform::log_dir` answers for SYSTEM or
            // LEGACY), opened read-only: under SYSTEM the directory is `_faber`'s,
            // and the operator only ever reads it, through group
            // `_faber_operator`.
            let system_log =
                log.is_none() && faber::platform::layout() == faber::platform::Layout::System;
            let audit = match log {
                Some(path) => faber::audit::Audit::at(std::path::PathBuf::from(path)),
                None => faber::audit::Audit::at(
                    faber::platform::log_dir()
                        .ok_or_else(|| {
                            anyhow::anyhow!("no HOME to find the audit log under — pass --log")
                        })?
                        .join("audit.jsonl"),
                ),
            };
            faber::status::run(
                &audit,
                faber::status::Opts {
                    since: faber::status::parse_since(&since)?,
                    repo,
                    limit,
                    port,
                    json,
                    system_log,
                },
            )
            .await?;
        }
        Command::Drain { port, wait_secs } => {
            let socket = faber::platform::control_socket().ok_or_else(|| {
                anyhow::anyhow!("no HOME to find the daemon's control socket under")
            })?;
            faber::drain::run(
                &socket,
                faber::control::fallback_here(port),
                Duration::from_secs(wait_secs),
            )
            .await?;
        }
        Command::Dashboard { port } => {
            let socket = faber::platform::control_socket().ok_or_else(|| {
                anyhow::anyhow!("no HOME to find the daemon's control socket under")
            })?;
            faber::dashboard::run(&socket, port).await?;
        }
        Command::Auth { command } => match command {
            AuthCommand::Link {
                owner,
                days,
                config,
                port,
            } => {
                faber::auth::link(faber::auth::LinkOpts {
                    owner,
                    days,
                    config: config.map(std::path::PathBuf::from),
                    port,
                })
                .await?;
            }
            AuthCommand::Set { expires, port } => {
                faber::auth::set(faber::auth::SetOpts { expires, port }).await?;
            }
        },
        Command::UpgradeCli { yes } => {
            let outcome = faber::upgrade_cli::run(yes).await?;
            println!("{}", faber::upgrade_cli::report(&outcome));
        }
        Command::Wall { action } => {
            tokio::task::spawn_blocking(move || faber::wall::cli::run(action)).await??;
        }
        Command::WallDaemon => {
            tokio::task::spawn_blocking(faber::wall::cli::run_daemon).await??;
        }
        Command::Grant { action } => {
            tokio::task::spawn_blocking(move || faber::wall::grant_cmd::run(action)).await??;
        }
        Command::Golden { action } => {
            faber::setup::golden_build::run(action).await?;
        }
        Command::Config { action } => {
            faber::config_cli::run_config(action).await?;
        }
        Command::RepoHold { action } => {
            faber::config_cli::run_repo_hold(action).await?;
        }
        Command::Engine {
            action: EngineCommand::Grow,
        } => {
            faber::vm::grow::cli().await?;
        }
        Command::Engine {
            action: EngineCommand::Stop,
        } => {
            #[cfg(target_os = "macos")]
            {
                faber::vm::engine::restart_this_host().await?;
                println!("faber's Linux VM is stopped; its disk is untouched");
            }
            #[cfg(not(target_os = "macos"))]
            anyhow::bail!("this host runs no Linux VM of faber's (macOS only)");
        }
        Command::ProvenancePolicy => {
            print!("{}", faber::provenance::describe_active_policy());
        }
    }
    Ok(())
}

/// `faber check-config`: the line to print on success, or every error. With
/// `--config -` the config is read from `stdin`, the descriptor this process
/// already holds, never by opening a path: `sudo faber setup` pipes the config
/// it is about to migrate to a check that has dropped to the operator, who may
/// not be able to open that file, or the pipe, by name.
fn check_config(
    config: Option<&str>,
    for_update: bool,
    machine: Option<&faber::machine::MachineReading>,
    stdin: impl std::io::Read,
) -> Result<String, Vec<String>> {
    let (shown, loaded) = if config == Some("-") {
        (
            "stdin".to_string(),
            faber::config::FileConfig::load_from_reader(stdin, "from stdin").map(Some),
        )
    } else {
        match faber::config::FileConfig::resolve_path(config.map(std::path::Path::new)) {
            // A host with no config file at all is a valid host — faber
            // synthesizes a profile from its flags. Treat "not there" as OK
            // rather than as a reason to hold an update back forever.
            Some(path) if !path.exists() => {
                return Ok(format!(
                    "no config file at {} — nothing to check.",
                    path.display()
                ))
            }
            Some(path) => (
                path.display().to_string(),
                faber::config::FileConfig::load(Some(&path)),
            ),
            None => return Ok("no HOME and no --config — nothing to check.".to_string()),
        }
    };
    match loaded {
        Err(e) => Err(vec![format!("{e:#}")]),
        Ok(None) => Ok("no config file — nothing to check.".to_string()),
        Ok(Some(cfg)) => {
            let mut errors = faber::update::config_errors(&cfg, for_update);
            if !for_update {
                if let Some(machine) = machine {
                    errors.extend(cfg.host.machine_share().errors(machine.into()));
                }
            }
            if errors.is_empty() {
                let mut warnings = Vec::new();
                warnings.extend(faber::config::recreatable_ignored_warning(&cfg.host));
                // ADR-0051: a layout key this host has nothing for.
                warnings.extend(faber::config::vm_layout_warnings(
                    &cfg,
                    cfg!(target_os = "linux"),
                    cfg!(target_os = "macos") && faber::platform::setup_record().is_some(),
                ));
                warnings.extend(faber::config::services_pull_warnings(&cfg));
                // Only the warning a valid file can still earn: errors (an invalid
                // `[mirror]`, `mirror` on a macos-vm profile) are refused above.
                warnings.extend(faber::config::mirror_warnings(&cfg));
                warnings.extend(cfg.host.ignored_keys_warning());
                warnings.extend(cfg.host.deprecated_sleep_key_warning());
                // An entry the settings form would refuse: a warning only, so
                // a file the daemon runs on is never held back over one. A
                // save refuses the entries it adds.
                warnings.extend(faber::settings::item_errors(&cfg));
                if warnings.is_empty() {
                    Ok(format!("config OK ({shown})."))
                } else {
                    Ok(format!(
                        "config OK ({shown}). warning: {}",
                        warnings.join(" warning: ")
                    ))
                }
            } else {
                Err(errors)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The autoupdater runs a staged binary with exactly
    /// `faber::update::check_config_args`, so that argv must parse here, as the
    /// structural-only check (ADR-0025) and not the full one.
    #[test]
    fn the_updaters_check_config_argv_parses_as_for_update() {
        let argv = std::iter::once(std::ffi::OsString::from("faber")).chain(
            faber::update::check_config_args(std::path::Path::new("/srv/faber/config.toml")),
        );
        match Cli::try_parse_from(argv)
            .expect("the updater's argv parses")
            .command
        {
            Command::CheckConfig { config, for_update } => {
                assert!(for_update);
                assert_eq!(config.as_deref(), Some("/srv/faber/config.toml"));
            }
            _ => panic!("parsed as another command"),
        }
        // A person running it gets the full check.
        let plain = Cli::try_parse_from(["faber", "check-config"]).unwrap();
        assert!(matches!(
            plain.command,
            Command::CheckConfig {
                for_update: false,
                ..
            }
        ));
    }

    /// `--color` is global (ADR-0050): before or after any subcommand, `auto`
    /// when absent, and in the argv setup gives its `check-config` children.
    #[test]
    fn color_is_a_global_flag() {
        use faber::out::ColorWhen;
        let color = |args: &[&str]| Cli::try_parse_from(args).unwrap().color;
        assert_eq!(color(&["faber", "status"]), ColorWhen::Auto);
        assert_eq!(
            color(&["faber", "--color", "never", "status"]),
            ColorWhen::Never
        );
        assert_eq!(
            color(&["faber", "setup", "--color=always"]),
            ColorWhen::Always
        );
        let child =
            Cli::try_parse_from(["faber", "check-config", "--config", "-", "--color", "never"])
                .unwrap();
        assert_eq!(child.color, ColorWhen::Never);
        assert!(matches!(child.command, Command::CheckConfig { .. }));
        assert!(Cli::try_parse_from(["faber", "--color", "sometimes", "status"]).is_err());
    }

    /// `check-config --config -` reads the config from stdin, the way `sudo
    /// faber setup` hands over the config it is about to migrate, through the
    /// same parse and checks as a file, `--for-update` included, and at most
    /// 1 MiB of it. A path is still a path, and stdin is not read for one.
    #[test]
    fn check_config_reads_a_config_from_stdin() {
        const VALID: &str = r#"
            topic = "faber"

            [[profile]]
            name = "linux-docker"
            labels = ["faber", "docker"]
            backend = "docker"
            image = "ghcr.io/owner/my-runner:latest"
            dind = false
        "#;
        for for_update in [false, true] {
            assert_eq!(
                check_config(Some("-"), for_update, None, VALID.as_bytes()),
                Ok("config OK (stdin).".to_string())
            );
        }

        let err = check_config(Some("-"), false, None, &b"topic = [1"[..]).unwrap_err();
        assert!(err[0].contains("parsing config from stdin"), "{err:?}");

        let mut big = VALID.as_bytes().to_vec();
        big.resize(faber::config::FileConfig::READ_CAP + 1, b'#');
        let err = check_config(Some("-"), true, None, &big[..]).unwrap_err();
        assert!(err[0].contains("larger than 1 MiB"), "{err:?}");

        struct Untouched;
        impl std::io::Read for Untouched {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                panic!("stdin was read for a path");
            }
        }
        assert_eq!(
            check_config(
                Some("/nonexistent-faber-dir/config.toml"),
                false,
                None,
                Untouched
            ),
            Ok("no config file at /nonexistent-faber-dir/config.toml — nothing to check.".into())
        );
    }

    /// ADR-0046 through `check-config`: a bad `[services.*]` table is an error,
    /// for the updater's gate too, and an image faber may not pull is a warning.
    #[test]
    fn check_config_judges_the_services_tables() {
        const BASE: &str = r#"
            topic = "faber"

            [[profile]]
            name = "linux-docker"
            labels = ["faber", "docker"]
            backend = "docker"
            image = "ghcr.io/owner/my-runner:latest"
            dind = false
        "#;
        let bad = format!("{BASE}\n[services.postgres]\nimage = \"postgres:16\"\nmemory_mib = 1");
        for for_update in [false, true] {
            let err = check_config(Some("-"), for_update, None, bad.as_bytes()).unwrap_err();
            assert!(
                err.iter().any(|e| e.contains("services.postgres")),
                "{err:?}"
            );
        }
        let blocked = format!(
            "{BASE}\n[services.redis]\nimage = \"redis:7\"\nmemory_mib = 64\n\
             [host]\npull_unqualified_images = false"
        );
        let ok = check_config(Some("-"), false, None, blocked.as_bytes()).unwrap();
        assert!(
            ok.starts_with("config OK (stdin). warning: service redis"),
            "{ok}"
        );
    }

    #[test]
    fn config_and_repo_hold_parse() {
        use faber::config_cli::{ConfigCommand, RepoHoldCommand};
        let parse = |args: &[&str]| Cli::try_parse_from(args).unwrap().command;
        assert!(matches!(
            parse(&["faber", "config", "set", "update.pin", "0.4.1"]),
            Command::Config { action: ConfigCommand::Set { key, value } }
                if key == "update.pin" && value == "0.4.1"
        ));
        // A value may start with a dash, and may be empty.
        assert!(matches!(
            parse(&["faber", "config", "set", "topic", "-x"]),
            Command::Config { action: ConfigCommand::Set { value, .. } } if value == "-x"
        ));
        assert!(matches!(
            parse(&["faber", "config", "set", "trusted_actors", ""]),
            Command::Config { action: ConfigCommand::Set { value, .. } } if value.is_empty()
        ));
        assert!(matches!(
            parse(&["faber", "config", "unset", "update.pin"]),
            Command::Config { action: ConfigCommand::Unset { key } } if key == "update.pin"
        ));
        assert!(matches!(
            parse(&["faber", "config", "apply", "c.toml"]),
            Command::Config { action: ConfigCommand::Apply { file, yes: false } }
                if file == std::path::Path::new("c.toml")
        ));
        assert!(matches!(
            parse(&["faber", "repo-hold", "clear", "owner/demo"]),
            Command::RepoHold { action: RepoHoldCommand::Clear { repo } } if repo == "owner/demo"
        ));
        assert!(matches!(
            parse(&["faber", "config", "apply", "--yes", "c.toml"]),
            Command::Config {
                action: ConfigCommand::Apply { yes: true, .. }
            }
        ));
        assert!(Cli::try_parse_from(["faber", "config", "set", "update.pin"]).is_err());
    }

    #[test]
    fn upgrade_cli_and_setups_hidden_flag_parse() {
        let cli = Cli::try_parse_from(["faber", "upgrade-cli", "--yes"]).unwrap();
        assert!(matches!(cli.command, Command::UpgradeCli { yes: true }));
        let cli = Cli::try_parse_from(["faber", "upgrade-cli"]).unwrap();
        assert!(matches!(cli.command, Command::UpgradeCli { yes: false }));
        // What setup re-executes itself with after promoting the CLI copy.
        let cli = Cli::try_parse_from(["faber", "setup", "--yes", "--skip-cli-upgrade"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Setup {
                yes: true,
                skip_cli_upgrade: true,
                ..
            }
        ));
        let cli = Cli::try_parse_from(["faber", "setup"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Setup {
                skip_cli_upgrade: false,
                ..
            }
        ));
    }

    #[test]
    fn setups_vm_options_and_the_wall_daemon_parse() {
        let cli = Cli::try_parse_from([
            "faber",
            "setup",
            "--launcher",
            "/opt/faber-vm",
            "--golden-image",
            "/opt/golden",
            "--golden-name",
            "sonoma",
            "--replace-golden",
            "--runner-image-tar",
            "/opt/runner.tar",
        ])
        .unwrap();
        match cli.command {
            Command::Setup {
                launcher,
                golden_image,
                golden_name,
                replace_golden,
                runner_image_tar,
                ..
            } => {
                assert_eq!(
                    launcher.as_deref(),
                    Some(std::path::Path::new("/opt/faber-vm"))
                );
                assert_eq!(
                    golden_image.as_deref(),
                    Some(std::path::Path::new("/opt/golden"))
                );
                assert_eq!(golden_name.as_deref(), Some("sonoma"));
                assert!(replace_golden);
                assert_eq!(
                    runner_image_tar.as_deref(),
                    Some(std::path::Path::new("/opt/runner.tar"))
                );
            }
            _ => panic!("not setup"),
        }
        // A name or a replace without an image to apply it to is an error.
        assert!(Cli::try_parse_from(["faber", "setup", "--golden-name", "x"]).is_err());
        assert!(Cli::try_parse_from(["faber", "setup", "--replace-golden"]).is_err());
        // #223: the override and the check install.sh runs.
        for (args, before, check) in [
            (&["faber", "setup"][..], false, false),
            (&["faber", "setup", "--before-vm-runtime"][..], true, false),
            (&["faber", "setup", "--check-vm-runtime"][..], false, true),
        ] {
            match Cli::try_parse_from(args).unwrap().command {
                Command::Setup {
                    before_vm_runtime,
                    check_vm_runtime,
                    ..
                } => assert_eq!((before_vm_runtime, check_vm_runtime), (before, check)),
                _ => panic!("not setup"),
            }
        }
        // The check changes nothing, so it takes nothing that would.
        for extra in ["--yes", "--dry-run", "--before-vm-runtime"] {
            assert!(
                Cli::try_parse_from(["faber", "setup", "--check-vm-runtime", extra]).is_err(),
                "{extra}"
            );
        }
        let cli = Cli::try_parse_from(["faber", "wall-daemon"]).unwrap();
        assert!(matches!(cli.command, Command::WallDaemon));
        // It takes nothing: its paths are fixed in the binary.
        assert!(Cli::try_parse_from(["faber", "wall-daemon", "--stored", "/tmp/s"]).is_err());
    }

    #[test]
    fn setup_takes_the_session_flags() {
        use faber::setup::session::{PasswordSource, RebootMode, Visibility};
        let parse = |args: &[&str]| {
            let mut argv = vec!["faber", "setup"];
            argv.extend(args);
            match Cli::try_parse_from(argv).unwrap().command {
                Command::Setup {
                    macos_session,
                    macos_session_visibility,
                    macos_session_password,
                    ..
                } => (
                    macos_session,
                    macos_session_visibility,
                    macos_session_password,
                ),
                _ => panic!("not setup"),
            }
        };
        assert_eq!(parse(&[]), (None, None, None));
        // Bare: the pre-selected or current mode.
        assert_eq!(parse(&["--macos-session"]).0, Some(None));
        assert_eq!(
            parse(&["--macos-session", "auto-login"]).0,
            Some(Some(RebootMode::AutoLogin))
        );
        assert_eq!(
            parse(&[
                "--macos-session=unlock",
                "--macos-session-visibility",
                "hidden",
                "--macos-session-password",
                "generate"
            ]),
            (
                Some(Some(RebootMode::Unlock)),
                Some(Visibility::Hidden),
                Some(PasswordSource::Generate)
            )
        );
        assert!(Cli::try_parse_from(["faber", "setup", "--macos-session", "sometimes"]).is_err());
    }

    #[test]
    fn the_grant_subcommand_parses() {
        use faber::wall::grant_cmd::GrantCommand;
        let cli = Cli::try_parse_from([
            "faber",
            "grant",
            "add",
            "--profile",
            "macos-vm",
            "--to",
            "10.0.0.5/32",
            "--port",
            "5432/tcp",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Grant { action: GrantCommand::Add { profile, to, port, overlaps_host: false } }
                if profile == "macos-vm" && to == "10.0.0.5/32" && port == "5432/tcp"
        ));
        let cli = Cli::try_parse_from([
            "faber",
            "grant",
            "add",
            "--profile",
            "p",
            "--to",
            "0.0.0.0/0",
            "--port",
            "22/tcp",
            "--overlaps-host",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Grant {
                action: GrantCommand::Add {
                    overlaps_host: true,
                    ..
                }
            }
        ));
        let cli = Cli::try_parse_from([
            "faber",
            "grant",
            "remove",
            "--profile",
            "p",
            "--to",
            "10.0.0.5/32",
            "--port",
            "22/tcp",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Grant {
                action: GrantCommand::Remove { .. }
            }
        ));
        for (args, list) in [
            (
                vec!["faber", "grant", "dns", "9.9.9.9", "149.112.112.112"],
                2,
            ),
            (vec!["faber", "grant", "dns", "--default"], 0),
        ] {
            let cli = Cli::try_parse_from(args).unwrap();
            assert!(matches!(
                cli.command,
                Command::Grant { action: GrantCommand::Dns { resolvers, .. } } if resolvers.len() == list
            ));
        }
        // `dns` needs a list or `--default`, never both.
        assert!(Cli::try_parse_from(["faber", "grant", "dns"]).is_err());
        assert!(Cli::try_parse_from(["faber", "grant", "dns", "9.9.9.9", "--default"]).is_err());
        assert!(Cli::try_parse_from(["faber", "grant", "add", "--profile", "p"]).is_err());
        for (verb, want) in [("list", GrantCommand::List), ("apply", GrantCommand::Apply)] {
            let cli = Cli::try_parse_from(["faber", "grant", verb]).unwrap();
            assert!(matches!(cli.command, Command::Grant { action } if action == want));
        }
    }

    #[test]
    fn the_hidden_wall_subcommand_parses() {
        use faber::wall::cli::WallCommand;
        let cli = Cli::try_parse_from(["faber", "wall", "render", "--plan", "p.json"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Wall { action: WallCommand::Render { plan, grants: None } } if plan == std::path::Path::new("p.json")
        ));
        let cli = Cli::try_parse_from([
            "faber", "wall", "render", "--plan", "p.json", "--grants", "g.toml",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Wall { action: WallCommand::Render { grants: Some(g), .. } } if g == std::path::Path::new("g.toml")
        ));
        let cli = Cli::try_parse_from([
            "faber",
            "wall",
            "check",
            "--stored",
            "s.json",
            "--verdict",
            "v.json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Wall {
                action: WallCommand::Check {
                    once: false,
                    interval_secs: 5,
                    ..
                }
            }
        ));
        let cli = Cli::try_parse_from([
            "faber",
            "wall",
            "check",
            "--stored",
            "s.json",
            "--verdict",
            "v.json",
            "--once",
            "--interval-secs",
            "9",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Wall {
                action: WallCommand::Check {
                    once: true,
                    interval_secs: 9,
                    ..
                }
            }
        ));
        assert!(Cli::try_parse_from(["faber", "wall", "check", "--stored", "s.json"]).is_err());
        // The checker enforces a stored set; it takes no plan to render.
        assert!(Cli::try_parse_from([
            "faber",
            "wall",
            "check",
            "--plan",
            "p.json",
            "--verdict",
            "v.json",
        ])
        .is_err());
        // Hidden: not in the top-level help.
        use clap::CommandFactory;
        let help = Cli::command().render_help().to_string();
        assert!(!help.contains("wall"), "{help}");
    }

    #[test]
    fn golden_verify_takes_an_image_name() {
        match Cli::try_parse_from(["faber", "golden", "verify", "xcode-27.0"])
            .unwrap()
            .command
        {
            Command::Golden {
                action: faber::setup::golden_build::GoldenCommand::Verify { name },
            } => assert_eq!(name, "xcode-27.0"),
            _ => panic!("not golden verify"),
        }
        assert!(Cli::try_parse_from(["faber", "golden", "verify"]).is_err());
    }

    #[test]
    fn golden_build_takes_its_flags() {
        let cli = Cli::try_parse_from([
            "faber",
            "golden",
            "build",
            "/b",
            "--name",
            "xcode-27.0",
            "--xcode",
            "/X.xip",
            "--runtime",
            "iOS:18.5",
            "--runtime",
            "watchOS",
            "--brew",
            "ruby",
            "--replace",
        ])
        .unwrap();
        match cli.command {
            Command::Golden {
                action:
                    faber::setup::golden_build::GoldenCommand::Build {
                        bundle,
                        name,
                        xcode,
                        runtimes,
                        brew,
                        replace,
                    },
            } => {
                assert_eq!(bundle, std::path::PathBuf::from("/b"));
                assert_eq!(name, "xcode-27.0");
                assert_eq!(xcode, std::path::PathBuf::from("/X.xip"));
                assert_eq!(runtimes, ["iOS:18.5", "watchOS"]);
                assert_eq!(brew, ["ruby"]);
                assert!(replace);
            }
            _ => panic!("not golden build"),
        }
        // The name and the Xcode are required.
        assert!(
            Cli::try_parse_from(["faber", "golden", "build", "/b", "--xcode", "/X.xip"]).is_err()
        );
        assert!(Cli::try_parse_from(["faber", "golden", "build", "/b", "--name", "x"]).is_err());
    }

    #[test]
    fn golden_prepare_takes_its_flags() {
        let cli = Cli::try_parse_from([
            "faber",
            "golden",
            "prepare",
            "b/macos-27",
            "--xcode",
            "/Applications/Xcode.app",
            "--runtime",
            "iOS",
            "--runtime",
            "watchOS:11.5",
            "--brew",
            "ruby",
            "--from",
            "xcode-26.0",
            "--keep-ipsw",
            "--disk-gib",
            "120",
            "--yes",
        ])
        .unwrap();
        match cli.command {
            Command::Golden {
                action: faber::setup::golden_build::GoldenCommand::Prepare(a),
            } => {
                assert_eq!(a.bundle, std::path::PathBuf::from("b/macos-27"));
                assert_eq!(a.runtimes, ["iOS", "watchOS:11.5"]);
                assert_eq!(a.brew, ["ruby"]);
                assert_eq!(a.from.as_deref(), Some("xcode-26.0"));
                assert_eq!(a.disk_gib, 120);
                assert!(a.keep_ipsw && a.yes && !a.no_host_runtimes);
            }
            _ => panic!("not golden prepare"),
        }
        // The bundle is required; --no-host-runtimes excludes --runtime.
        assert!(Cli::try_parse_from(["faber", "golden", "prepare"]).is_err());
        assert!(Cli::try_parse_from([
            "faber",
            "golden",
            "prepare",
            "/b",
            "--no-host-runtimes",
            "--runtime",
            "iOS"
        ])
        .is_err());
    }

    /// A failed command's error, and everything it quotes, reaches the
    /// terminal as text, in the shape returning it from `main` printed.
    #[test]
    fn a_failed_commands_error_is_printed_as_text() {
        let e = anyhow::anyhow!("unexpected status 502: \u{1b}]52;c;cm0=\u{7}\u{9b}2J\rok")
            .context("draining the running daemon");
        let report = error_report(&e);
        assert!(
            report.starts_with("Error: draining the running daemon\n\nCaused by:\n"),
            "{report}"
        );
        assert!(
            report.ends_with("unexpected status 502: \\x1b]52;c;cm0=\\x07\\x9b2J\\x0dok"),
            "{report}"
        );
        assert!(
            !report.chars().any(|c| c.is_control() && c != '\n'),
            "{report:?}"
        );
    }
}
