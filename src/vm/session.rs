//! ADR-0051's "no VM session" gate, its input: whether the session account's
//! macOS VM supervisor answers.
//!
//! Where the setup record names the session account (`_faber_mac`), macOS VM
//! jobs run through the supervisor its LaunchAgent starts at login
//! ([`super::job::ProductionSupervisor::for_macos_jobs`]). After a reboot that session is down
//! until the operator, or auto-login, brings it back, and a job sent there
//! would only fail. So the daemon probes it, a handshake every
//! [`PROBE_INTERVAL`] on the client jobs use, and the limiter refuses macOS VM
//! jobs while it does not answer ([`crate::vm::admit::VmRefusal::NoVmSession`]),
//! saying which reboot mode applies and what to do.
//!
//! A failed handshake while the account is logged in at the console is not a
//! missing session but its LaunchAgent gone or not serving (#95): the agent
//! was booted out of a live session once, and the reboot-mode words ("unlock
//! the disk") pointed the operator at a fix that does nothing. So a failed
//! probe also asks who is logged in ([`SessionPresence`]), and says which.
//!
//! A record without the account has no gate, and its macOS VM jobs keep
//! `_faber_vm`'s supervisor, as before (AGENTS.md: an absent key keeps the old
//! behaviour).

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::admit::{Presence, ProbeFailure, SessionMode};
use super::client::SupervisorClient;
use crate::limiter::Limiter;
use crate::platform::record::SetupRecord;
use crate::setup::accounts::SessionLookup;

/// How often the session supervisor is probed. One handshake on a local
/// socket: cheap, and a session that comes back is admitted within this.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(30);

/// Something that can be asked whether it answers: the session client's
/// handshake, or a fake in tests.
pub trait Handshake {
    /// `Ok` when the supervisor answered in this release's protocol, else how
    /// it failed.
    fn reach(&self) -> impl Future<Output = Result<(), ProbeFailure>> + Send;
}

impl Handshake for SupervisorClient {
    async fn reach(&self) -> Result<(), ProbeFailure> {
        let Err(e) = self.handshake().await else {
            return Ok(());
        };
        let unreachable = matches!(e, super::client::ClientError::Unreachable(_));
        // Looked at after the failure: a socket that went in between is gone.
        let missing = match std::fs::symlink_metadata(self.socket_path()) {
            Ok(_) => Some(false),
            Err(io) if io.kind() == std::io::ErrorKind::NotFound => Some(true),
            Err(_) => None,
        };
        Err(ProbeFailure::classify(missing, unreachable, e.to_string()))
    }
}

/// Something that can say whether the session account is logged in: the
/// console users list, or a fake in tests.
pub trait SessionPresence {
    fn presence(&self) -> impl Future<Output = Presence> + Send;
}

/// The console users list (utmpx, through `who`): world-readable, so no root.
pub struct ConsoleUsers;

/// How long `who` may take before the presence is "not known".
const WHO_LIMIT: Duration = Duration::from_secs(5);

impl SessionPresence for ConsoleUsers {
    async fn presence(&self) -> Presence {
        tokio::task::spawn_blocking(console_presence_bounded)
            .await
            .unwrap_or_else(|e| Presence::Unknown(format!("the lookup did not finish: {e}")))
    }
}

/// `who`, bounded, read for `_faber_mac`. Blocking.
fn console_presence_bounded() -> Presence {
    use crate::setup::tools::{output_within, Tool, SAFE_PATH};
    let mut cmd = std::process::Command::new(Tool::Who.path());
    cmd.env_clear()
        .env("PATH", SAFE_PATH)
        .stdin(std::process::Stdio::null());
    match output_within(cmd, WHO_LIMIT) {
        Ok(out) if out.success => {
            console_presence(&out.stdout, crate::platform::layout::FABER_MAC_ACCOUNT)
        }
        Ok(out) => Presence::Unknown(format!(
            "who failed: {}",
            crate::setup::tools::sanitize_child_output(out.stderr.trim(), 200)
        )),
        Err(e) => Presence::Unknown(format!("who: {e:#}")),
    }
}

/// Whether `who`'s output shows `account` logged in at the console: a GUI
/// session, which is what runs a LaunchAgent limited to `Aqua`. A terminal or
/// SSH login (`ttys…`) is not one. Pure.
pub fn console_presence(who: &str, account: &str) -> Presence {
    let at_console = who.lines().any(|line| {
        let mut cols = line.split_whitespace();
        cols.next() == Some(account) && cols.next() == Some("console")
    });
    if at_console {
        Presence::LoggedIn
    } else {
        Presence::Absent
    }
}

/// The reboot mode to gate with, `None` when `record` names no session
/// account and so there is no gate. Pure.
pub fn gated_mode(record: &SetupRecord) -> Option<SessionMode> {
    record
        .session_account()
        .map(|a| SessionMode::from_record(a.reboot_mode.as_deref()))
}

/// Whether the record's uid `recorded` is this host's session account, given
/// the uid the directory gives `_faber_mac` (`found`, `None` when it has no
/// such account). Pure. faber connects to the session supervisor, and runs
/// jobs there, only while this is `Ok`: a uid an account no longer holds may
/// be someone else's.
pub fn account_check(recorded: u32, found: Option<u32>) -> Result<(), String> {
    let account = crate::platform::layout::FABER_MAC_ACCOUNT;
    match found {
        Some(uid) if uid == recorded => Ok(()),
        Some(uid) => Err(format!(
            "the setup record names {account} as uid {recorded}, and this host's {account} is \
             uid {uid}; re-run `sudo faber setup`"
        )),
        None => Err(format!(
            "the setup record names {account} (uid {recorded}), and this host has no such \
             account: a setup run stopped before making it, or it was removed; re-run `sudo \
             faber setup`"
        )),
    }
}

/// How long the last definitive account check stands while the directory
/// does not answer (`dscl` failing or hanging): long enough that one slow
/// `dscl` changes nothing, short enough that a host that cannot be checked
/// stops starting jobs.
pub const ACCOUNT_CHECK_GRACE: Duration = Duration::from_secs(300);

/// The account check across probes: a lookup that got no answer keeps the
/// previous definitive verdict for [`ACCOUNT_CHECK_GRACE`], then refuses.
#[derive(Debug, Default)]
pub struct AccountChecker {
    last: Option<(Result<(), String>, Instant)>,
}

impl AccountChecker {
    /// The verdict for `recorded` given this probe's `lookup` at `now`.
    pub fn judge(
        &mut self,
        recorded: u32,
        lookup: SessionLookup,
        now: Instant,
    ) -> Result<(), String> {
        let why = match lookup {
            SessionLookup::Found(uid) => return self.keep(account_check(recorded, Some(uid)), now),
            SessionLookup::NotFound => return self.keep(account_check(recorded, None), now),
            SessionLookup::Failed(why) => why,
        };
        match &self.last {
            Some((verdict, at)) if now.saturating_duration_since(*at) < ACCOUNT_CHECK_GRACE => {
                verdict.clone()
            }
            _ => Err(format!(
                "cannot check the session account {} ({why}), so faber starts no macOS VM job \
                 until it can",
                crate::platform::layout::FABER_MAC_ACCOUNT
            )),
        }
    }

    fn keep(&mut self, verdict: Result<(), String>, now: Instant) -> Result<(), String> {
        self.last = Some((verdict.clone(), now));
        verdict
    }
}

/// Whether the last check found the recorded account to be this host's
/// `_faber_mac` ([`account_check`]). Off until the first check: the session
/// client refuses every call until then ([`super::job::ProductionSupervisor`]).
static ACCOUNT_OK: AtomicBool = AtomicBool::new(false);

/// [`ACCOUNT_OK`].
pub fn account_trusted() -> bool {
    ACCOUNT_OK.load(Ordering::SeqCst)
}

/// What the directory says of `_faber_mac` now, read without root, bounded,
/// off the runtime.
async fn lookup() -> SessionLookup {
    tokio::task::spawn_blocking(crate::setup::accounts::session_lookup_bounded)
        .await
        .unwrap_or_else(|e| SessionLookup::Failed(format!("the lookup did not finish: {e}")))
}

/// One probe, into the limiter's gate: the handshake, and, only when it
/// failed, whether account `uid` is logged in.
pub async fn probe_once<H: Handshake, P: SessionPresence>(
    target: &H,
    seen: &P,
    uid: u32,
    limiter: &Limiter,
) {
    let probe = target.reach().await;
    let presence = match probe {
        Ok(()) => Presence::LoggedIn,
        Err(_) => seen.presence().await,
    };
    limiter.observe_vm_session_probe(probe, presence, uid);
}

/// One probe given the account check: no handshake at all unless the account
/// is the recorded one, and a refusal that says why if not.
pub async fn probe_checked<H: Handshake, P: SessionPresence>(
    target: &H,
    seen: &P,
    uid: u32,
    account: Result<(), String>,
    limiter: &Limiter,
) {
    ACCOUNT_OK.store(account.is_ok(), Ordering::SeqCst);
    match account {
        Ok(()) => probe_once(target, seen, uid, limiter).await,
        Err(why) => limiter.observe_no_session_account(why),
    }
}

/// Check the account and probe every [`PROBE_INTERVAL`], forever, after the
/// first check [`start_for_this_host`] made. The account is checked every
/// time, so a setup run that makes it after the daemon started (the record is
/// written first) is seen without a restart.
async fn watch<H: Handshake>(
    target: &H,
    recorded_uid: u32,
    mut checker: AccountChecker,
    limiter: Arc<Limiter>,
) {
    loop {
        tokio::time::sleep(PROBE_INTERVAL).await;
        let account = checker.judge(recorded_uid, lookup().await, Instant::now());
        probe_checked(target, &ConsoleUsers, recorded_uid, account, &limiter).await;
    }
}

/// Gate macOS VM jobs on the session, check the account and probe it once,
/// then keep probing, where this host's record names the session account.
/// `false`, and nothing gated, where it does not. Awaited at boot on a host
/// with a `macos-vm` profile, before boot adoption, so adoption's first
/// `list` goes to a supervisor already checked.
pub async fn start_for_this_host(limiter: Arc<Limiter>) -> bool {
    let Some(record) = crate::platform::setup_record() else {
        return false;
    };
    let (Some(mode), Some(uid)) = (gated_mode(record), record.faber_mac_uid()) else {
        return false;
    };
    let Some(client) = super::job::session_client() else {
        return false;
    };
    limiter.require_vm_session(mode);
    let mut checker = AccountChecker::default();
    let account = checker.judge(uid, lookup().await, Instant::now());
    probe_checked(client, &ConsoleUsers, uid, account, &limiter).await;
    tokio::spawn(watch(client, uid, checker, limiter));
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::record::SessionAccount;

    /// #95: only a console login is a GUI session; a terminal or SSH login of
    /// the same account, or another account at the console, is not.
    #[test]
    fn the_console_users_list_is_read_for_the_account_at_the_console() {
        let who = "owner            console      Nov  9 00:48 \n\
                   _faber_mac       console      Nov  9 00:50 \n";
        assert_eq!(console_presence(who, "_faber_mac"), Presence::LoggedIn);
        let elsewhere = "owner            console      Nov  9 00:48 \n\
                         _faber_mac       ttys003      Nov  9 00:50 \n\
                         _faber_mac_x     console      Nov  9 00:50 \n";
        assert_eq!(console_presence(elsewhere, "_faber_mac"), Presence::Absent);
        assert_eq!(console_presence("", "_faber_mac"), Presence::Absent);
    }

    fn record(session: Option<SessionAccount>) -> SetupRecord {
        let mut r = SetupRecord::from_json(
            br#"{"schema": 1, "operator": {"uid": 501, "name": "owner"},
                "accounts": {"faber": {"uid": 451, "gid": 451},
                             "faber_vm": {"uid": 452, "gid": 452}},
                "installed": {"version": "0.4.0"}, "migrated_from": null,
                "artifacts": []}"#,
        )
        .unwrap();
        r.accounts.faber_mac = session;
        r
    }

    /// Only `dscl`'s "no such record" means the account is gone; a lookup
    /// that failed or timed out keeps the last definitive verdict for the
    /// grace, then refuses with its own reason.
    #[test]
    fn a_failed_lookup_keeps_the_last_verdict_for_the_grace_then_refuses() {
        let t0 = Instant::now();
        let failed = || SessionLookup::Failed("dscl: did not finish within 10s; killed".into());
        let mut c = AccountChecker::default();
        // Nothing known yet: no answer refuses at once.
        let first = c.judge(600, failed(), t0).unwrap_err();
        assert!(
            first.starts_with("cannot check the session account _faber_mac"),
            "{first}"
        );
        assert!(first.contains("did not finish"), "{first}");
        // A good check, then a failure within the grace: still good.
        assert_eq!(c.judge(600, SessionLookup::Found(600), t0), Ok(()));
        assert_eq!(c.judge(600, failed(), t0 + Duration::from_secs(60)), Ok(()));
        // Past the grace: refused, with the reason, and not "no such account".
        let late = c
            .judge(
                600,
                failed(),
                t0 + ACCOUNT_CHECK_GRACE + Duration::from_secs(1),
            )
            .unwrap_err();
        assert!(late.starts_with("cannot check"), "{late}");
        assert!(!late.contains("no such account"), "{late}");
        // A definitive "not found" is kept as such through a later failure.
        let gone = c.judge(600, SessionLookup::NotFound, t0).unwrap_err();
        assert!(gone.contains("no such account"), "{gone}");
        assert_eq!(
            c.judge(600, failed(), t0 + Duration::from_secs(1)),
            Err(gone)
        );
    }

    /// The daemon checks the account and probes the session once, awaited,
    /// before boot adoption's first `list`: otherwise every boot refuses that
    /// `list` (the client is untrusted until a check) and adoption opens a
    /// retry later.
    #[test]
    fn the_first_check_runs_before_boot_adoption() {
        let main = include_str!("../main.rs");
        let check = main
            .find("faber::vm::session::start_for_this_host(")
            .expect("the daemon starts the session check");
        let adopt = main.find("orch.adopt_vms().await").expect("boot adoption");
        assert!(check < adopt, "the first account check must come first");
        assert!(main[check..adopt].contains(".await"), "and be awaited");
    }

    #[test]
    fn the_recorded_uid_must_be_this_hosts_session_account() {
        assert_eq!(account_check(600, Some(600)), Ok(()));
        let moved = account_check(600, Some(601)).unwrap_err();
        assert!(moved.contains("is uid 601"), "{moved}");
        let gone = account_check(600, None).unwrap_err();
        assert!(gone.contains("no such account"), "{gone}");
    }

    #[test]
    fn only_a_recorded_session_account_is_gated() {
        assert_eq!(gated_mode(&record(None)), None);
        let account = |mode: Option<&str>| SessionAccount {
            uid: 600,
            gid: 20,
            reboot_mode: mode.map(str::to_string),
        };
        for (mode, want) in [
            (Some("unlock"), SessionMode::Unlock),
            (Some("auto-login"), SessionMode::AutoLogin),
            (Some("manual"), SessionMode::Manual),
            // A mode a later release added, or none: still gated, worded plainly.
            (Some("telepathy"), SessionMode::Unknown),
            (None, SessionMode::Unknown),
        ] {
            assert_eq!(
                gated_mode(&record(Some(account(mode)))),
                Some(want),
                "{mode:?}"
            );
        }
    }
}
