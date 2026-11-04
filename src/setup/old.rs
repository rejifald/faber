//! Old per-user fabers: reading their definitions, stopping them, and putting
//! them back if setup fails (ADR-0025).
//!
//! Before ADR-0025 faber ran as a person: a LaunchAgent in `gui/<uid>` on
//! macOS, a `systemd --user` unit on Linux. Setup drains such an instance
//! first (`POST /api/drain`, then waiting until nothing it admitted is still
//! running), stops it, and moves its definition aside to
//! `<name>.pre-system-<time>` so it does not come back at the next login. Its
//! files are left where they are: migration copies them.
//!
//! Root works in the account's home here, which that account (and anything
//! running as it) controls, so every file operation checks that the file is a
//! regular file the account owns before touching it ([`fsx::open_source`]).

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};

use super::fsx::{self, Dir, SourceRefusal};
use super::plan::{HostState, LegacyAccount, DEFAULT_PORT};
use super::tools::Runner;
use crate::platform::layout::Os;
use crate::platform::service::{self, launchd, systemd, UNIT_NAME};

/// How long to wait for a booted-out label to free: a little above the
/// plist's `ExitTimeOut`.
const LABEL_WAIT: Duration = Duration::from_secs(service::STOP_TIMEOUT.as_secs() + 60);

/// What an old instance's definition file turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Definition {
    Missing,
    Found(String),
    /// There, but not a regular file its account owns: not moved, not read.
    Refused(String),
    /// Not readable by this run (another account's home, without sudo).
    Unknown(String),
}

/// Where an account's per-user faber was defined: its LaunchAgent plist on
/// macOS, its `systemd --user` unit on Linux.
pub fn definition_path(os: Os, home: &Path) -> PathBuf {
    match os {
        Os::MacOs => home
            .join("Library/LaunchAgents")
            .join(format!("{}.plist", service::LABEL)),
        Os::Linux => home.join(".config/systemd/user").join(UNIT_NAME),
    }
}

/// Read the definition at `path`, which `uid` must own.
pub fn read_definition(path: &Path, uid: u32) -> Definition {
    let Ok((parent, name)) = fsx::split(path) else {
        return Definition::Unknown(format!("{} has no file name", path.display()));
    };
    let dir = match Dir::open(parent) {
        Ok(dir) => dir,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Definition::Missing,
        Err(e) => return Definition::Unknown(e.to_string()),
    };
    match fsx::open_source(&dir, name, uid) {
        Ok((file, _)) => {
            let mut bytes = Vec::new();
            match file.take(1 << 20).read_to_end(&mut bytes) {
                // A binary plist reads as noise: no arguments, so the defaults.
                Ok(_) => Definition::Found(String::from_utf8_lossy(&bytes).into_owned()),
                Err(e) => Definition::Unknown(e.to_string()),
            }
        }
        Err(SourceRefusal::Missing) => Definition::Missing,
        Err(SourceRefusal::Unreadable(e)) => Definition::Unknown(e),
        Err(other) => Definition::Refused(other.to_string()),
    }
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// `ProgramArguments` from an XML plist: the `<string>`s of the `<array>`
/// after that key. Empty when there is none to find.
pub fn plist_program_arguments(text: &str) -> Vec<String> {
    let Some(at) = text.find("<key>ProgramArguments</key>") else {
        return Vec::new();
    };
    let rest = &text[at..];
    let (Some(open), Some(close)) = (rest.find("<array>"), rest.find("</array>")) else {
        return Vec::new();
    };
    if close < open {
        return Vec::new();
    }
    let mut body = &rest[open + "<array>".len()..close];
    let mut args = Vec::new();
    while let Some(i) = body.find("<string>") {
        let after = &body[i + "<string>".len()..];
        let Some(j) = after.find("</string>") else {
            break;
        };
        args.push(xml_unescape(&after[..j]));
        body = &after[j + "</string>".len()..];
    }
    args
}

/// The words of a unit's `ExecStart=`, split as systemd does for the simple
/// cases: whitespace, and double quotes around a word with spaces.
pub fn unit_exec_start(text: &str) -> Vec<String> {
    let Some(line) = text
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("ExecStart="))
    else {
        return Vec::new();
    };
    let mut words = Vec::new();
    let mut word = String::new();
    let mut quoted = false;
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' if quoted => {
                if let Some(next) = chars.next() {
                    word.push(next);
                }
            }
            '"' => quoted = !quoted,
            c if c.is_whitespace() && !quoted => {
                if !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
            }
            c => word.push(c),
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}

/// The `--port` and `--config` an old instance was started with, or the
/// defaults. A relative config path is relative to the instance's working
/// directory, which was the account's home.
pub fn port_and_config(args: &[String], home: &Path) -> (u16, Option<PathBuf>) {
    let mut port = DEFAULT_PORT;
    let mut config = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        if arg == "--port" {
            if let Some(p) = it.next().and_then(|v| v.parse().ok()) {
                port = p;
            }
        } else if let Some(v) = arg.strip_prefix("--port=") {
            if let Ok(p) = v.parse() {
                port = p;
            }
        } else if arg == "--config" {
            config = it.next().map(PathBuf::from);
        } else if let Some(v) = arg.strip_prefix("--config=") {
            config = Some(PathBuf::from(v));
        }
    }
    let config = config.map(|c| if c.is_absolute() { c } else { home.join(c) });
    (port, config)
}

/// A definition file moved aside, with the directory it was moved in held
/// open, so putting it back never resolves a path in the account's home again.
#[derive(Debug)]
pub struct Moved {
    pub dir: Dir,
    pub original: String,
    pub backup: String,
}

/// An old instance setup stopped: enough to put it back.
#[derive(Debug)]
pub struct Stopped {
    pub account: String,
    pub uid: u32,
    pub was_loaded: bool,
    /// Its plist or unit, where it was before any move.
    pub definition: Option<PathBuf>,
    pub moved: Option<Moved>,
}

/// Move the definition at `def` to `<name>.pre-system-<ts>` in the same
/// directory, if it is a regular file `uid` owns.
pub fn move_aside(def: &Path, uid: u32, ts: &str) -> Result<Moved> {
    let (parent, name) = fsx::split(def)?;
    let dir = Dir::open(parent).with_context(|| format!("opening {}", parent.display()))?;
    fsx::open_source(&dir, name, uid)
        .map_err(|why| anyhow!("{} {why}; not moving it", def.display()))?;
    let backup = format!("{name}.pre-system-{ts}");
    dir.move_aside(name, &backup)
        .with_context(|| format!("moving {} aside", def.display()))?;
    println!("  moved {} to {backup}", def.display());
    Ok(Moved {
        dir,
        original: name.to_string(),
        backup,
    })
}

/// Ask `acct`'s per-user faber to drain, and wait until nothing it admitted
/// is still running.
///
/// Draining is one-way: `/api/resume` lifts a pause, not a drain, so from the
/// moment this is called the instance claims nothing new until its process
/// restarts. A caller that goes no further must [`revive`] it.
pub async fn drain(acct: &LegacyAccount, drain_timeout: Duration) -> Result<()> {
    let inst = acct
        .instance
        .as_ref()
        .with_context(|| format!("{} has no per-user faber", acct.name))?;
    // The account's own LEGACY socket; a daemon from before ADR-0028 has only
    // the TCP port, which the request falls back to.
    let socket = super::legacy_socket(&acct.home);
    if super::api_answers(&socket, crate::control::Fallback::Tcp(inst.port)).await {
        println!(
            "  draining {}'s faber (control socket {}, or 127.0.0.1:{}; waits up to {}s for \
             admitted jobs)",
            acct.name,
            socket.display(),
            inst.port,
            drain_timeout.as_secs()
        );
        crate::drain::run(
            &socket,
            crate::control::Fallback::Tcp(inst.port),
            drain_timeout,
        )
        .await
        .with_context(|| format!("draining {}'s per-user faber", acct.name))?;
    } else if inst.loaded == Some(true) {
        println!(
            "  {}'s faber is loaded but not answering on {} or 127.0.0.1:{}: nothing to drain",
            acct.name,
            socket.display(),
            inst.port
        );
    }
    Ok(())
}

/// Stop `acct`'s drained per-user faber. Returns whether it was loaded.
pub async fn halt(s: &HostState, acct: &LegacyAccount, runner: &mut dyn Runner) -> Result<bool> {
    let port = acct.instance.as_ref().map_or(DEFAULT_PORT, |i| i.port);
    Ok(match s.os {
        Os::MacOs => {
            let target = service::gui_target(acct.uid);
            let loaded = launchd::is_loaded(runner, &target);
            if loaded {
                launchd::bootout(runner, &target)?;
                launchd::wait_until_gone(runner, &target, LABEL_WAIT, Duration::from_secs(1))?;
                println!("  stopped {target}");
            }
            loaded
        }
        Os::Linux if s.user_manager => {
            let active = runner
                .run(&systemd::user_cmd(
                    &acct.name,
                    &["is-active", "--quiet", UNIT_NAME],
                ))
                .is_ok_and(|o| o.success);
            if active {
                runner.run_ok(&systemd::user_cmd(
                    &acct.name,
                    &["disable", "--now", UNIT_NAME],
                ))?;
                println!("  stopped {}'s {UNIT_NAME}", acct.name);
            }
            active
        }
        Os::Linux => {
            let socket = super::legacy_socket(&acct.home);
            if super::api_answers(&socket, crate::control::Fallback::Tcp(port)).await {
                bail!(
                    "{}'s per-user faber still answers ({} or 127.0.0.1:{port}), and this systemd \
                     cannot stop another user's unit: as {}, run `systemctl --user disable \
                     --now {UNIT_NAME}`, then re-run",
                    acct.name,
                    socket.display(),
                    acct.name,
                );
            }
            false
        }
    })
}

/// Restart a per-user faber that was drained but not stopped, so it claims
/// jobs again: a fresh process is the only way out of a drain, and it adopts
/// the containers still running (ADR-0011).
pub fn revive(
    s: &HostState,
    account: &str,
    uid: u32,
    definition: Option<&Path>,
    runner: &mut dyn Runner,
) -> Result<()> {
    match s.os {
        Os::MacOs => {
            let target = service::gui_target(uid);
            if !launchd::is_loaded(runner, &target) {
                let plist =
                    definition.context("it is not loaded and has no plist to bootstrap it from")?;
                launchd::bootstrap(runner, &format!("gui/{uid}"), plist, Duration::from_secs(2))?;
            }
            launchd::kickstart(runner, &target, true)
        }
        Os::Linux if s.user_manager => {
            runner.run_ok(&systemd::user_cmd(account, &["restart", UNIT_NAME]))?;
            Ok(())
        }
        Os::Linux => bail!(
            "this systemd cannot restart another user's unit: as {account}, run `systemctl \
             --user restart {UNIT_NAME}`"
        ),
    }
}

/// Put a stopped instance back: its definition where it was, and running
/// again if it was. The legacy daemon then re-adopts its containers
/// (ADR-0011).
pub fn restore(s: &HostState, st: &Stopped, runner: &mut dyn Runner) -> Result<()> {
    if let Some(m) = &st.moved {
        m.dir.rename(&m.backup, &m.original)?;
    }
    if !st.was_loaded {
        return Ok(());
    }
    match s.os {
        Os::MacOs => {
            let plist = st
                .definition
                .as_deref()
                .context("it ran without a plist, so there is nothing to bootstrap it from")?;
            launchd::bootstrap(
                runner,
                &format!("gui/{}", st.uid),
                plist,
                Duration::from_secs(2),
            )?;
            launchd::kickstart(runner, &service::gui_target(st.uid), true)?;
        }
        Os::Linux => {
            runner.run_ok(&systemd::user_cmd(
                &st.account,
                &["enable", "--now", UNIT_NAME],
            ))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::fsx::tests::{me, Scratch};
    use std::os::unix::fs::symlink;

    /// The shape `deploy/install.sh` rendered: a binary and `run`.
    const LEGACY_PLIST: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>io.github.rejifald.faber</string>
    <key>ProgramArguments</key>
    <array>
        <string>/Users/owner/Library/Application Support/faber/bin/faber</string>
        <string>run</string>
    </array>
    <key>EnvironmentVariables</key>
    <dict>
        <key>PATH</key>
        <string>/usr/bin:/bin</string>
    </dict>
</dict>
</plist>"#;

    #[test]
    fn plist_arguments_and_defaults() {
        let args = plist_program_arguments(LEGACY_PLIST);
        assert_eq!(
            args,
            vec![
                "/Users/owner/Library/Application Support/faber/bin/faber".to_string(),
                "run".to_string()
            ]
        );
        let home = Path::new("/Users/owner");
        assert_eq!(port_and_config(&args, home), (8787, None));
    }

    #[test]
    fn port_and_config_come_from_the_arguments() {
        let with = LEGACY_PLIST.replace(
            "<string>run</string>",
            "<string>run</string>\n<string>--port</string><string>9090</string>\
             <string>--config=cfg/faber.toml</string>",
        );
        let args = plist_program_arguments(&with);
        let (port, config) = port_and_config(&args, Path::new("/Users/owner"));
        assert_eq!(port, 9090);
        assert_eq!(config, Some(PathBuf::from("/Users/owner/cfg/faber.toml")));
        let (port, _) = port_and_config(&["run".into(), "--port=7000".into()], Path::new("/"));
        assert_eq!(port, 7000);
    }

    #[test]
    fn no_arguments_is_empty_not_a_panic() {
        assert!(plist_program_arguments("").is_empty());
        assert!(plist_program_arguments("<key>ProgramArguments</key></array><array>").is_empty());
        assert_eq!(
            plist_program_arguments(
                "<key>ProgramArguments</key><array><string>a &amp; b</string></array>"
            ),
            vec!["a & b".to_string()]
        );
    }

    #[test]
    fn unit_exec_start_splits_like_systemd() {
        let unit =
            "[Service]\nType=simple\nExecStart=\"/home/owner/.local/state/faber/bin/faber\" \
                    run --port 8788\nWorkingDirectory=/home/owner\n";
        let args = unit_exec_start(unit);
        assert_eq!(
            args,
            vec![
                "/home/owner/.local/state/faber/bin/faber",
                "run",
                "--port",
                "8788"
            ]
        );
        assert_eq!(port_and_config(&args, Path::new("/home/owner")).0, 8788);
        assert!(unit_exec_start("[Service]\n").is_empty());
    }

    #[test]
    fn a_definition_is_read_only_when_its_account_owns_it() {
        let s = Scratch::new("def");
        let uid = me(&s).uid;
        let plist = s.path("io.github.rejifald.faber.plist");
        std::fs::write(&plist, LEGACY_PLIST).unwrap();
        assert!(matches!(read_definition(&plist, uid), Definition::Found(_)));
        assert!(matches!(
            read_definition(&plist, uid + 1),
            Definition::Refused(_)
        ));
        assert_eq!(
            read_definition(&s.path("absent.plist"), uid),
            Definition::Missing
        );
        assert_eq!(
            read_definition(&s.path("no/such/dir.plist"), uid),
            Definition::Missing
        );
        let link = s.path("linked.plist");
        symlink(&plist, &link).unwrap();
        assert!(matches!(
            read_definition(&link, uid),
            Definition::Refused(_)
        ));
    }

    #[test]
    fn moving_aside_renames_in_place_and_refuses_a_symlink() {
        let s = Scratch::new("aside");
        let uid = me(&s).uid;
        let plist = s.path("io.github.rejifald.faber.plist");
        std::fs::write(&plist, LEGACY_PLIST).unwrap();
        let moved = move_aside(&plist, uid, "20260928T120000Z").unwrap();
        assert_eq!(
            moved.backup,
            "io.github.rejifald.faber.plist.pre-system-20260928T120000Z"
        );
        assert!(!plist.exists());
        assert!(s.path(&moved.backup).exists());

        std::fs::write(s.path("target"), b"x").unwrap();
        symlink(s.path("target"), &plist).unwrap();
        assert!(move_aside(&plist, uid, "20260928T120001Z").is_err());
        assert!(s.path("target").exists());
    }
}
