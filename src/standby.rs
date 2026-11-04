//! How `faber run` boots when it must not simply run (ADR-0025).
//!
//! Before ADR-0025 the daemon had two outcomes at boot: it came up, or it
//! exited. Under launchd's `KeepAlive` and systemd's `Restart=always` an exit is
//! not an outcome but a loop, and ADR-0006's rollback reads a loop as a bad
//! release. This module holds the other outcomes, each for a condition that
//! restarting cannot fix:
//!
//! - **Refuse** ([`BootDecision::Refuse`]): exit non-zero. Only where nothing
//!   restarts the process: `faber run` as root, which no service faber installs
//!   ever starts, and `run --once`, which is a person at a terminal.
//! - **Park** ([`park`]): a service process that must not run on this host. It
//!   logs why, then sleeps, saying so again every hour. It takes no lock, binds
//!   no port and claims nothing, so the real daemon is unaffected, and it never
//!   exits, so it never loops.
//! - **Standby** ([`until_ready`]): the right daemon, missing something only
//!   GitHub can give it: a credential, the runner version, the account's login.
//!   It serves a stub of the control API on the normal port that says what is
//!   missing and what to do, retries every minute, and boots as soon as GitHub
//!   answers. It never exits either, with one exception, below.
//!
//! # Standby while an update is on probation
//!
//! Standby must not hide a bad release. A binary that has just installed itself
//! (ADR-0006) and cannot reach GitHub may be the reason it cannot: a broken
//! client, a broken TLS setup. Waiting forever would strand the host on it: no
//! exit, so no rollback, and no updater, so no fix. So while the update's boot
//! marker is on probation, each failure is judged ([`evidence_of`], [`fault`],
//! [`next_step`]).
//!
//! The rollback also quarantines the release for good, so only a failure that
//! points at the binary counts against it:
//!
//! - **The binary's**: a transport, TLS or client failure while an independent
//!   probe ([`github_reachable`]) reaches GitHub; a 2xx answer that does not
//!   decode; a status that means a malformed request to these fixed endpoints
//!   (400, 404, 405, 406, 410, 415, 422). [`PROBATION_ATTEMPTS`] of these in a
//!   row and the process exits non-zero, without the clean-exit mark, so the
//!   boot counts as failed and the rollback restores the previous release after
//!   the third.
//! - **The host's, or GitHub's**: no credential; 401, 403 or 429; any 5xx; and
//!   a transport failure while the probe cannot reach GitHub either. These wait,
//!   as they do off probation, and keep the marker. So does anything else, since
//!   waiting is the cheap mistake and a quarantined good release the dear one.
//!
//! [`decide`] is the pure choice between running, refusing and parking, taken
//! before anything else happens. [`prerequisites`] is what standby waits for;
//! its credential half is one function, [`crate::github::credential`], which
//! reads ADR-0027's token store. Standby's `/api/state` reads the store itself
//! on every request ([`crate::credential::standby_view`]), so a token that
//! `sudo faber auth set` stores shows there at once, and is taken at the next
//! attempt.
//!
//! A signal ends parking the default way: nothing is in flight and nothing was
//! taken. It ends standby deliberately ([`until_ready_or_signalled`]): the stub
//! stops and the clean-exit mark is written, exactly as a normal shutdown does,
//! so a restart during probation is not counted as a failed boot. [`Stops`]
//! listens for the signal from before standby until the process exits, so one
//! that arrives between standby and the main loop is not lost either.

use std::convert::Infallible;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::{
    extract::State,
    http::StatusCode,
    response::{Html, Json},
    routing::{get, post},
    Router,
};
use serde::Serialize;

use crate::github::{Github, GithubError};
use crate::platform::layout::{FABER_ACCOUNT, SETUP_RECORD};
use crate::platform::{Layout, Resolution};
use crate::reconcile::{
    Counts, Decision, DriftEvent, FaultEvent, InflightView, Notice, OutcomeEvent, ProfileView,
    RetryEvent,
};
use crate::update::UpdateView;

// --- who may run the daemon -------------------------------------------------

/// What `faber run` does, decided before it resolves a path, takes a lock or
/// binds a port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootDecision {
    /// Boot normally.
    Run,
    /// Say why and exit non-zero.
    Refuse(String),
    /// Say why, then [`park`]: sleep forever, never exit.
    Park(String),
}

/// Decide whether this process may run the daemon (ADR-0025).
///
/// - **LEGACY** (no setup record): run, exactly as before ADR-0025.
/// - **SYSTEM**, as `_faber` (the record's uid): run.
/// - **SYSTEM**, as root: refuse. Root never runs the daemon, and no service
///   faber installs starts it as root, so the exit is not a loop.
/// - **SYSTEM**, as anyone else: park. This is a stray per-user agent from
///   before `sudo faber setup`; exiting would only have `KeepAlive` start it
///   again.
/// - **An untrusted record**: park, whoever runs it. The host was set up, and
///   faber cannot tell from a record it will not trust which account and paths
///   are meant. Running LEGACY paths as `_faber` under `HOME=D` would be the
///   worst of both.
///
/// `once` is `run --once`: a person at a terminal, not a service, so there is
/// no loop to avoid and a park would only hang their shell. It refuses instead.
pub fn decide(resolution: &Resolution, euid: u32, once: bool) -> BootDecision {
    let park = |why: String| {
        if once {
            BootDecision::Refuse(why)
        } else {
            BootDecision::Park(why)
        }
    };
    match resolution {
        Resolution::Legacy => BootDecision::Run,
        Resolution::System(record) => {
            let faber = record.faber_uid();
            if euid == 0 {
                BootDecision::Refuse(format!(
                    "`faber run` as root is refused: this host runs faber as {FABER_ACCOUNT} \
                     (uid {faber}), started by the service `sudo faber setup` installed, and \
                     root never runs the daemon (ADR-0025). Restart that service instead."
                ))
            } else if euid == faber {
                BootDecision::Run
            } else {
                park(format!(
                    "stray per-user agent: faber runs as {FABER_ACCOUNT} now (sudo faber setup). \
                     This process runs as uid {euid}, not {FABER_ACCOUNT} (uid {faber}), so it \
                     takes no lock, binds no port and claims no job. Remove the agent or unit \
                     that started it."
                ))
            }
        }
        Resolution::Untrusted(reason) => park(format!(
            "setup record untrusted: {SETUP_RECORD} {reason}. This host was set up by \
             `sudo faber setup`, and faber will not guess which account and paths it meant, \
             so it takes no lock, binds no port and claims no job until `sudo faber setup` \
             rewrites the record."
        )),
    }
}

/// This process's effective uid. Only Unix has a SYSTEM layout, so elsewhere
/// the value decides nothing.
pub fn euid() -> u32 {
    #[cfg(unix)]
    {
        crate::platform::record::current_euid()
    }
    #[cfg(not(unix))]
    {
        u32::MAX
    }
}

/// The account name this process runs as, for messages: `$USER`, else
/// `$LOGNAME`, else the uid.
pub fn running_user() -> String {
    ["USER", "LOGNAME"]
        .iter()
        .find_map(|var| std::env::var(var).ok().filter(|v| !v.trim().is_empty()))
        .unwrap_or_else(|| format!("uid {}", euid()))
}

// --- parked -----------------------------------------------------------------

/// How often a parked process says again why it is parked.
pub const PARK_LOG_EVERY: Duration = Duration::from_secs(3600);

/// Log `why`, then sleep forever, saying it again every [`PARK_LOG_EVERY`].
///
/// Returns [`Infallible`], so the caller's `match park(..).await {}` proves in
/// the type that nothing after it runs: no lock, no port, no job.
pub async fn park(why: &str) -> Infallible {
    loop {
        tracing::warn!("parked: {why}");
        tokio::time::sleep(PARK_LOG_EVERY).await;
    }
}

// --- boot notices -----------------------------------------------------------

/// The boot warning and dashboard banner for a host still on the LEGACY
/// layout: faber runs as `user`, the operator, rather than its own account.
/// `None` under SYSTEM, and where the OS has no SYSTEM layout to move to
/// (`system_layout_exists` is false).
pub fn legacy_notice(layout: Layout, system_layout_exists: bool, user: &str) -> Option<Notice> {
    (layout == Layout::Legacy && system_layout_exists).then(|| Notice {
        kind: "legacy_layout".to_string(),
        text: format!("faber runs as {user}; run `sudo faber setup`"),
    })
}

// --- what standby waits for -------------------------------------------------

/// Why the daemon cannot boot yet, and what would change that.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("could not get {what}: {reason}. Next step: {next}")]
pub struct Blocker {
    /// What is missing, as a noun phrase: `a GitHub credential`.
    pub what: &'static str,
    /// Why, as the failure itself put it.
    pub reason: String,
    /// What to do about it.
    pub next: String,
    /// What the failure says about this binary, which matters while an update
    /// is on probation ([`fault`]).
    pub evidence: Evidence,
}

/// What a failed attempt says, on its own, about the binary that made it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Evidence {
    /// The host's or GitHub's: no credential; a 401, 403 or 429; any 5xx; and
    /// any answer not listed under [`Evidence::Binary`].
    Host,
    /// The binary's: a 2xx body it cannot decode, or a status that means it
    /// sent a malformed request to endpoints that do not change (400, 404,
    /// 405, 406, 410, 415, 422).
    Binary,
    /// No answer: a transport, TLS or DNS failure, or an HTTP client that could
    /// not be built. Whose fault that is depends on whether this host can reach
    /// GitHub at all ([`fault`]).
    Unanswered,
}

/// Whose fault a failed attempt is, as far as an update's probation is
/// concerned (ADR-0006).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The host lacks something no binary can supply, or GitHub is down or
    /// limiting it. A new release is no reason to roll back over it.
    Host,
    /// This binary is the likely cause, and the release may be rolled back.
    Binary,
}

/// The statuses that mean this binary sent a malformed request. faber only
/// calls fixed endpoints that exist, so these point at the binary, not at
/// GitHub or the host.
const MALFORMED_REQUEST: [u16; 7] = [400, 404, 405, 406, 410, 415, 422];

/// What a GitHub failure says about the binary that saw it.
pub fn evidence_of(err: &GithubError) -> Evidence {
    match err {
        GithubError::Token(_) | GithubError::StaleCache(_) => Evidence::Host,
        // A 2xx the binary cannot act on, or a request it refused to build: both
        // are this binary's own handling of fixed endpoints.
        GithubError::Decode(_) | GithubError::Unexpected(_) | GithubError::Refused(_) => {
            Evidence::Binary
        }
        GithubError::Status { status, .. } => status_evidence(*status),
        GithubError::Http(e) => match e.status() {
            Some(status) => status_evidence(status.as_u16()),
            None if e.is_decode() => Evidence::Binary,
            None => Evidence::Unanswered,
        },
    }
}

/// What an HTTP status says about the binary that got it. Anything not known
/// to be the binary's is the host's: see the module doc.
fn status_evidence(status: u16) -> Evidence {
    if MALFORMED_REQUEST.contains(&status) {
        Evidence::Binary
    } else {
        Evidence::Host
    }
}

/// Whose fault a failure is, given its [`Evidence`] and, for an unanswered
/// request only, whether this host can reach GitHub without faber's HTTP and
/// TLS stack. `reachable` is that probe ([`github_reachable`] in the daemon);
/// it is not called for an answered request.
pub fn fault(evidence: Evidence, reachable: impl FnOnce() -> bool) -> Fault {
    match evidence {
        Evidence::Host => Fault::Host,
        Evidence::Binary => Fault::Binary,
        // The network is down or GitHub unreachable from here: not the binary.
        // Reachable, yet this binary got no answer: its own client failed.
        Evidence::Unanswered if reachable() => Fault::Binary,
        Evidence::Unanswered => Fault::Host,
    }
}

/// How long the reachability probe waits for each of its steps: the name to
/// resolve, then each TCP connection.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether this host can reach GitHub at all, asked without faber's HTTP or
/// TLS stack: resolve `api.github.com` with the system resolver, then open a
/// plain TCP connection to port 443, trying at most two of its addresses. A
/// release that broke its HTTP client cannot fail this, so when a request got
/// no answer and this fails as well, the network is to blame, not the binary.
/// Each step is bounded by [`PROBE_TIMEOUT`], and a timeout reads as
/// unreachable. Blocking: run it off the async runtime.
pub fn github_reachable() -> bool {
    use std::net::{TcpStream, ToSocketAddrs};
    reachable_via(
        || {
            ("api.github.com", 443)
                .to_socket_addrs()
                .map(Iterator::collect)
        },
        |addr, timeout| TcpStream::connect_timeout(addr, timeout).is_ok(),
        PROBE_TIMEOUT,
    )
}

/// [`github_reachable`]'s two steps, injected so a test can run them without
/// the network, each bounded by `timeout`.
///
/// The system resolver has no timeout of its own, so the name is resolved on
/// a thread of its own and waited for at most `timeout`. A resolver that never
/// answers leaves that thread behind until it does: a thread, not a hung probe.
fn reachable_via<R>(
    resolve: R,
    connect: impl Fn(&SocketAddr, Duration) -> bool,
    timeout: Duration,
) -> bool
where
    R: FnOnce() -> std::io::Result<Vec<SocketAddr>> + Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(resolve());
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(addrs)) => addrs.iter().take(2).any(|addr| connect(addr, timeout)),
        // Not resolved in time, not resolved at all, or the resolver thread died.
        _ => false,
    }
}

/// The number of attempts in a row whose failure the binary may cause, after
/// which standby gives up while an update is on probation: about three minutes
/// at [`RETRY`].
pub const PROBATION_ATTEMPTS: u32 = 3;

/// What standby does after a failed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next {
    /// Retry after [`RETRY`], serving the stub meanwhile.
    Wait,
    /// Stop, so the process can exit and ADR-0006's probation count the boot
    /// as failed.
    GiveUp,
}

/// The rule, as a pure function of whether the update's boot marker is on
/// probation, the latest failure's [`Fault`], and how many attempts in a row,
/// the latest included, failed in a way the binary may cause.
///
/// Off probation standby always waits: there is nothing to roll back to, and
/// exiting would only be a crash loop. On probation a host fault waits too,
/// and keeps the marker: the binary may be fine, and giving up would roll back
/// a good release for want of a login. (ADR-0027 refines this: a 401 proves
/// the request arrived and the binary works, so then it may end probation, and
/// clear the marker, rather than only wait.)
pub fn next_step(probation: bool, fault: Fault, binary_in_a_row: u32) -> Next {
    match (probation, fault) {
        (true, Fault::Binary) if binary_in_a_row >= PROBATION_ATTEMPTS => Next::GiveUp,
        _ => Next::Wait,
    }
}

/// Standby stopped trying, while an update was on probation, over failures
/// this binary may cause.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "update probation: {attempts} attempts in a row failed in a way this binary may cause \
     (last: {blocker}). Exiting without the clean-exit mark, so ADR-0006 counts this boot as \
     failed and restores the previous release after the third"
)]
pub struct GaveUp {
    /// The last failure.
    pub blocker: Blocker,
    /// The run of failures that ended it.
    pub attempts: u32,
}

impl Blocker {
    /// `last_error` in standby's state: the reason, the next step, and that
    /// faber is retrying on its own.
    pub fn last_error(&self, retry: Duration) -> String {
        format!(
            "standby: {self}. faber retries every {} s and claims nothing until then.",
            retry.as_secs()
        )
    }
}

/// What the daemon needs from GitHub before it can do anything.
pub struct Ready {
    pub gh: Arc<Github>,
    /// The latest `actions/runner` version, for a macOS VM job that pins none.
    pub runner_version: String,
    /// The account the credential belongs to, which scopes repo discovery.
    pub owner: String,
}

/// Ask GitHub for everything [`Ready`] holds. All three were fatal at boot
/// before ADR-0025; each failure is a [`Blocker`] now.
///
/// `user` is the account this process runs as, for the next step a LEGACY
/// operator is told.
pub async fn prerequisites(layout: Layout, user: String) -> Result<Ready, Blocker> {
    let (token, store) = credential(layout, &user).await?;
    let stored_owner = token.owner.clone();
    let gh = Arc::new(Github::from_store(token, store).map_err(|e| Blocker {
        what: "an HTTP client for GitHub",
        reason: e.to_string(),
        next: "this is a fault in faber's TLS setup on this host, not GitHub's".to_string(),
        // No request was made, so the probe decides, as for any unanswered one.
        evidence: Evidence::Unanswered,
    })?);
    let runner_version = gh
        .latest_runner_version()
        .await
        .map_err(|e| api_blocker("the latest actions/runner version", &e, layout, &user))?;
    let owner = gh
        .current_login()
        .await
        .map_err(|e| api_blocker("the GitHub account's login", &e, layout, &user))?;
    // The store's owner is a label; GitHub's login is the fact. A token filed
    // under one owner that acts as another would scope discovery to the other
    // account's repositories, so it is refused here, as the running daemon
    // refuses it when the store changes (ADR-0027).
    if !owner.eq_ignore_ascii_case(&stored_owner) {
        return Err(owner_mismatch(&stored_owner, &owner));
    }
    Ok(Ready {
        gh,
        runner_version,
        owner,
    })
}

/// The stored token acts as `login`, not as the `stored` owner it is filed
/// under. Logins are not secrets; the token never appears.
fn owner_mismatch(stored: &str, login: &str) -> Blocker {
    Blocker {
        what: "a GitHub credential",
        reason: format!(
            "the token stored for {stored} acts as {login} on GitHub, so faber will not use it"
        ),
        next: format!("store a token for {stored} with `sudo faber auth set`"),
        evidence: Evidence::Host,
    }
}

/// The daemon's credential, through the one seam ADR-0027 names.
async fn credential(
    layout: Layout,
    user: &str,
) -> Result<(crate::credential::StoredToken, crate::credential::Store), Blocker> {
    // Reading the store blocks; the stub is serving on this runtime meanwhile.
    match tokio::task::spawn_blocking(crate::github::credential).await {
        Ok(found) => found.map_err(|e| credential_blocker(layout, user, &e)),
        // The lookup itself panicked: that is this binary, not the host.
        Err(e) => Err(Blocker {
            what: "a GitHub credential",
            reason: format!("the credential lookup failed: {e}"),
            next: "this is a fault in faber itself; see its log".to_string(),
            evidence: Evidence::Binary,
        }),
    }
}

/// No usable credential: nothing in the store, something in it faber refuses,
/// or, on a LEGACY host, no store at all. faber reads no token from `gh` or
/// the environment any more (ADR-0027), so the advice is the same command
/// sequence everywhere, and never "set a token in the service's environment",
/// which every account on the host can read.
fn credential_blocker(layout: Layout, user: &str, err: &GithubError) -> Blocker {
    let reason = match err {
        GithubError::Token(detail) => detail.clone(),
        other => other.to_string(),
    };
    Blocker {
        what: "a GitHub credential",
        reason,
        next: credential_next_step(layout, user),
        evidence: evidence_of(err),
    }
}

fn credential_next_step(layout: Layout, user: &str) -> String {
    legacy_or_system_next_step(layout, user, cfg!(windows))
}

/// [`credential_next_step`] with the OS passed in, so the Windows wording can be
/// tested anywhere. Windows has only the LEGACY layout and so no ADR-0027 token
/// store: it says so plainly and names no command that would not work there.
fn legacy_or_system_next_step(layout: Layout, user: &str, windows: bool) -> String {
    if windows {
        return "Windows hosts cannot hold a GitHub credential since ADR-0027: faber reads no \
                token from `gh` or the environment, and there is no token store on Windows"
            .to_string();
    }
    match layout {
        Layout::System => format!(
            "run `faber auth link`, create the token it links to on GitHub, then `sudo faber \
             auth set` to hand it to {FABER_ACCOUNT}. Never put a token in the service \
             definition: every account on this host can read its environment"
        ),
        Layout::Legacy => format!(
            "faber no longer reads a token from `gh` or the environment: run `sudo faber setup` \
             to move it off {user}'s account, then `faber auth link` and `sudo faber auth set`"
        ),
    }
}

/// A GitHub call that failed, and what to do about it by how it failed.
fn api_blocker(what: &'static str, err: &GithubError, layout: Layout, user: &str) -> Blocker {
    let next = match err {
        GithubError::Token(_) => credential_next_step(layout, user),
        GithubError::Status { status: 401, .. } => format!(
            "GitHub rejected the stored token: it has expired or was revoked. Regenerate it on \
             GitHub, or {}",
            credential_next_step(layout, user)
        ),
        GithubError::Status {
            status: 403 | 429, ..
        } => "GitHub refused or rate-limited the request: wait for the limit to reset, or check \
              the token's permissions on GitHub (`faber auth link` prints the ones faber needs)"
            .to_string(),
        GithubError::Status {
            status: 500..=599, ..
        } => "GitHub is failing on its side; faber retries until it recovers".to_string(),
        GithubError::Http(_) => "check this host's network access to api.github.com".to_string(),
        _ => "GitHub answered in a way faber did not expect; see the reason".to_string(),
    };
    Blocker {
        what,
        reason: err.to_string(),
        next,
        evidence: evidence_of(err),
    }
}

// --- standby ----------------------------------------------------------------

/// How long standby waits between attempts.
pub const RETRY: Duration = Duration::from_secs(60);

/// How long a stopping stub may take to finish in-flight requests before it
/// is dropped regardless. Loopback requests that take longer are not coming.
const STOP_GRACE: Duration = Duration::from_secs(5);

/// What standby needs to know to stand in for the daemon.
#[derive(Debug, Clone)]
pub struct StandbyOptions {
    /// The daemon's front door (ADR-0028): the stub answers on the control
    /// socket and the dashboard's port, behind the same guard, and a session
    /// opened here keeps working once the daemon is up.
    pub door: Arc<crate::control::FrontDoor>,
    /// How long to wait between attempts; [`RETRY`] in the daemon.
    pub retry: Duration,
    /// The repo topic the daemon will watch, for the panel's header.
    pub topic: String,
    /// The audit log's path, as the panel shows it.
    pub audit_path: String,
    /// This boot's standing conditions, for the dashboard banners.
    pub notices: Vec<Notice>,
    /// This binary was just installed by the updater and its boot marker is on
    /// probation (ADR-0006), so a failure it may cause ends standby
    /// ([`next_step`]).
    pub probation: bool,
    /// Whether this host can reach GitHub without faber's HTTP stack:
    /// [`github_reachable`] in the daemon, a stand-in in tests, which never
    /// touch the network. Asked only on probation, and only about a request
    /// that got no answer ([`fault`]).
    pub reachable: fn() -> bool,
}

/// How standby ended, other than with what the boot needed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// It gave up on probation ([`GaveUp`]). The caller exits non-zero,
    /// without the clean-exit mark, so the boot counts as failed.
    GaveUp(GaveUp),
    /// SIGTERM or SIGINT: a reboot, `launchctl kickstart -k`, setup booting
    /// the service out. The stub has stopped and the clean-exit mark is
    /// written, as a normal shutdown writes it; the caller exits 0.
    Signalled,
}

/// SIGTERM and SIGINT, listened for from before standby until the process
/// exits.
///
/// tokio keeps a signal handler installed once one is, and drops a signal that
/// arrives while nothing listens for it. Listening only while standby runs,
/// and again once the main loop starts, would drop one that arrives between
/// the two: the boot would carry on with its stop ignored until the service
/// manager killed it. One listener, held for the whole run, leaves no gap.
///
/// Not for `run --once`, which Ctrl-C ends the default way.
pub struct Stops(tokio::sync::mpsc::UnboundedReceiver<()>);

impl Stops {
    /// Start listening now. If the handlers cannot be installed, the signals
    /// keep their default action, ending the process, and [`Self::next`]
    /// never resolves.
    pub fn listen() -> Self {
        use tokio::signal::unix::{signal, SignalKind};
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        match (
            signal(SignalKind::terminate()),
            signal(SignalKind::interrupt()),
        ) {
            (Ok(mut term), Ok(mut int)) => {
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            _ = term.recv() => tracing::info!("received SIGTERM"),
                            _ = int.recv() => tracing::info!("received SIGINT"),
                        }
                        if tx.send(()).is_err() {
                            return;
                        }
                    }
                });
            }
            (term, int) => {
                let e = term.err().or(int.err());
                tracing::warn!(
                    "cannot listen for SIGTERM and SIGINT ({e:?}); they will end faber at once"
                );
            }
        }
        Self(rx)
    }

    /// The next SIGTERM or SIGINT, including one that arrived before this was
    /// asked.
    pub async fn next(&mut self) {
        if self.0.recv().await.is_none() {
            // Not listening (see `listen`): nothing will ever arrive.
            std::future::pending::<()>().await;
        }
    }
}

/// [`until_ready_or_signalled`] with no signal to stop it: it ends only when
/// `attempt` succeeds or, on probation, gives up.
pub async fn until_ready<T, F, Fut>(opts: StandbyOptions, attempt: F) -> Result<T, GaveUp>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Blocker>>,
{
    match until_ready_or_signalled(opts, attempt, std::future::pending(), None).await {
        Ok(ready) => Ok(ready),
        Err(Stop::GaveUp(gave_up)) => Err(gave_up),
        Err(Stop::Signalled) => unreachable!("a signal that never arrives stopped standby"),
    }
}

/// Run `attempt` until it succeeds, standing in for the daemon meanwhile.
///
/// The first attempt runs before anything is served, exactly as boot always
/// went. If it fails, the daemon is in standby: a stub of the control API
/// answers on the normal port ([`StandbyState`]), and `attempt` runs again
/// every `retry`. When one succeeds the stub is stopped, its port released,
/// and the value returned for the normal boot to carry on with. A daemon that
/// GitHub cannot reach is a daemon that says so, not a crash loop.
///
/// Two other ways out, each with the stub stopped first, for the caller to
/// exit on:
///
/// - [`Stop::GaveUp`], only on probation: after [`PROBATION_ATTEMPTS`] failures
///   in a row that the binary may cause ([`next_step`]).
/// - [`Stop::Signalled`], when `signal` resolves, at any point. The clean-exit
///   mark goes to `clean_exit`, the updater's paths, exactly as a normal
///   shutdown writes it (ADR-0011 §4): a deliberate restart during probation is
///   not a failed boot, and two of them must not quarantine a good release.
///
/// A drain (`POST /api/drain`) ends the retrying for good. There is nothing in
/// flight to wait for, and a daemon asked to stand down must not then boot and
/// start claiming because GitHub came back.
pub async fn until_ready_or_signalled<T, F, Fut, S>(
    opts: StandbyOptions,
    attempt: F,
    signal: S,
    clean_exit: Option<crate::update::UpdatePaths>,
) -> Result<T, Stop>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Blocker>>,
    S: Future<Output = ()>,
{
    let mut stub: Option<StubServer> = None;
    let outcome = tokio::select! {
        waited = wait(opts, attempt, &mut stub) => waited.map_err(Stop::GaveUp),
        () = signal => Err(Stop::Signalled),
    };
    if let Some(stub) = stub.take() {
        stub.stop().await;
    }
    if matches!(outcome, Err(Stop::Signalled)) {
        tracing::info!("standby: stopped by a signal, which is a deliberate exit");
        if let Some(paths) = &clean_exit {
            crate::update::note_clean_exit(paths);
        }
    }
    outcome
}

/// [`until_ready_or_signalled`]'s loop. Leaves whatever stub it started in
/// `stub`, for the caller to stop however the loop ends.
async fn wait<T, F, Fut>(
    opts: StandbyOptions,
    mut attempt: F,
    stub: &mut Option<StubServer>,
) -> Result<T, GaveUp>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, Blocker>>,
{
    let probation = opts.probation;
    let mut judge = Judge {
        probation,
        reachable: opts.reachable,
        binary_in_a_row: 0,
    };
    let blocker = match attempt().await {
        Ok(ready) => return Ok(ready),
        Err(blocker) => blocker,
    };
    if let (Next::GiveUp, attempts) = judge.judge(blocker.evidence).await {
        return Err(GaveUp { blocker, attempts });
    }
    let door = opts.door.clone();
    let port = door.port();
    let standby = Arc::new(Standby::new(opts, blocker));
    tracing::warn!("{}", standby.last_error());
    if probation {
        tracing::warn!(
            "standby: this binary's update is on probation (ADR-0006), so after \
             {PROBATION_ATTEMPTS} failures in a row that it may cause, it exits to be rolled back"
        );
    }
    let mut bind_reported = false;
    loop {
        if stub.is_none() {
            match StubServer::start(&door, standby.clone()).await {
                Ok(started) => {
                    tracing::info!(
                        "standby: answering on http://{} and {} until GitHub does",
                        started.addr,
                        crate::web::socket_name(&door)
                    );
                    *stub = Some(started);
                }
                Err(e) if !bind_reported => {
                    bind_reported = true;
                    tracing::warn!(
                        "standby: cannot serve 127.0.0.1:{port} and {} ({e}); retrying with each attempt",
                        crate::web::socket_name(&door)
                    );
                }
                Err(_) => {}
            }
        }
        tokio::time::sleep(standby.retry).await;
        if standby.drained() {
            continue;
        }
        match attempt().await {
            Ok(ready) => {
                tracing::info!("standby: GitHub answered — booting");
                return Ok(ready);
            }
            Err(blocker) => {
                let verdict = judge.judge(blocker.evidence).await;
                if standby.replace(blocker.clone()) {
                    tracing::warn!("{}", standby.last_error());
                }
                if let (Next::GiveUp, attempts) = verdict {
                    return Err(GaveUp { blocker, attempts });
                }
            }
        }
    }
}

/// Standby's running judgement of failed attempts, for [`until_ready`].
struct Judge {
    probation: bool,
    reachable: fn() -> bool,
    /// Failures in a row, the latest included, that were the binary's.
    binary_in_a_row: u32,
}

impl Judge {
    /// Judge one failure by its [`Evidence`]: its [`Fault`], the run of binary
    /// faults, and what to do next, which is always to wait off probation.
    /// The probe runs only on probation and only for an unanswered request,
    /// off the async runtime, since it blocks for up to [`PROBE_TIMEOUT`].
    async fn judge(&mut self, evidence: Evidence) -> (Next, u32) {
        if !self.probation {
            return (Next::Wait, 0);
        }
        let found = match evidence {
            Evidence::Unanswered => {
                let reachable = self.reachable;
                tokio::task::spawn_blocking(move || fault(evidence, reachable))
                    .await
                    // A probe that could not run proves nothing against the binary.
                    .unwrap_or(Fault::Host)
            }
            answered => fault(answered, || false),
        };
        self.binary_in_a_row = match found {
            Fault::Binary => self.binary_in_a_row + 1,
            Fault::Host => 0,
        };
        (
            next_step(true, found, self.binary_in_a_row),
            self.binary_in_a_row,
        )
    }
}

/// Standby's shared state: what the stub reports, and whether it was drained.
struct Standby {
    topic: String,
    audit_path: String,
    notices: Vec<Notice>,
    retry: Duration,
    started: Instant,
    since_ms: u64,
    blocker: Mutex<Blocker>,
    draining: AtomicBool,
}

impl Standby {
    fn new(opts: StandbyOptions, blocker: Blocker) -> Self {
        Self {
            topic: opts.topic,
            audit_path: opts.audit_path,
            notices: opts.notices,
            retry: opts.retry,
            started: Instant::now(),
            since_ms: crate::audit::now_ms(),
            blocker: Mutex::new(blocker),
            draining: AtomicBool::new(false),
        }
    }

    fn last_error(&self) -> String {
        self.blocker.lock().unwrap().last_error(self.retry)
    }

    /// Record the latest blocker. `true` when it differs from the last one,
    /// which is when it is worth logging again.
    fn replace(&self, blocker: Blocker) -> bool {
        let mut current = self.blocker.lock().unwrap();
        let changed = *current != blocker;
        *current = blocker;
        changed
    }

    fn drained(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    fn state(&self) -> StandbyState {
        let blocker = self.blocker.lock().unwrap().clone();
        StandbyState {
            owner: String::new(),
            topic: self.topic.clone(),
            profiles: Vec::new(),
            gate: None,
            concurrency: 0,
            paused: false,
            draining: self.drained(),
            uptime_s: self.started.elapsed().as_secs(),
            audit_path: self.audit_path.clone(),
            inflight: Vec::new(),
            counts: Counts::default(),
            recent: Vec::new(),
            outcomes: Vec::new(),
            faults: Vec::new(),
            drift: Vec::new(),
            retries: Vec::new(),
            healthy: false,
            last_error: Some(blocker.last_error(self.retry)),
            update: UpdateView::default(),
            notices: self.notices.clone(),
            credential: crate::credential::standby_view(crate::audit::now_ms()),
            trust_changes: Vec::new(),
            standby: StandbyView {
                since_ms: self.since_ms,
                what: blocker.what.to_string(),
                reason: blocker.reason,
                next: blocker.next,
                retry_secs: self.retry.as_secs(),
            },
        }
    }
}

/// `GET /api/state` in standby: every field the menu-bar app requires of the
/// daemon's `StateView`, and every one `faber drain` and `faber status` read,
/// with `healthy: false`, the reason and next step in `last_error`, and nothing
/// in flight. `standby` says, to anything that looks, that this is not a
/// running daemon.
#[derive(Debug, Serialize)]
pub struct StandbyState {
    /// The GitHub login, which is not known yet.
    pub owner: String,
    pub topic: String,
    pub profiles: Vec<ProfileView>,
    pub gate: Option<crate::gate::Boundary>,
    pub concurrency: usize,
    pub paused: bool,
    pub draining: bool,
    pub uptime_s: u64,
    pub audit_path: String,
    pub inflight: Vec<InflightView>,
    pub counts: Counts,
    pub recent: Vec<Decision>,
    pub outcomes: Vec<OutcomeEvent>,
    pub faults: Vec<FaultEvent>,
    pub drift: Vec<DriftEvent>,
    pub retries: Vec<RetryEvent>,
    pub healthy: bool,
    pub last_error: Option<String>,
    pub update: UpdateView,
    pub notices: Vec<Notice>,
    /// The GitHub credential (ADR-0027), read from the store on each request.
    /// Never the token.
    pub credential: crate::credential::CredentialView,
    /// The daemon's week of allowed trust-widening changes (ADR-0028 addendum
    /// B). Standby has not read the audit log, so it reports none.
    pub trust_changes: Vec<crate::audit::TrustChange>,
    pub standby: StandbyView,
}

/// Standby's own section of [`StandbyState`].
#[derive(Debug, Clone, Serialize)]
pub struct StandbyView {
    /// When standby began, epoch ms.
    pub since_ms: u64,
    pub what: String,
    pub reason: String,
    pub next: String,
    pub retry_secs: u64,
}

/// The stub's routes. Reading state, the page and draining work; everything
/// else answers `503` with the reason. Who may call them is the front door's
/// ([`crate::control`]), exactly as for the live dashboard.
fn router(standby: Arc<Standby>) -> Router {
    Router::new()
        .route("/", get(page).fallback(unavailable))
        .route("/api/state", get(state).fallback(unavailable))
        .route("/api/drain", post(drain).fallback(unavailable))
        .fallback(unavailable)
        .with_state(standby)
}

async fn state(State(standby): State<Arc<Standby>>) -> Json<StandbyState> {
    Json(standby.state())
}

async fn page() -> Html<&'static str> {
    Html(PAGE_HTML)
}

/// Nothing is in flight, so the drain is done at once, and the daemon stays
/// down: it answers in the same words as the real endpoint.
async fn drain(State(standby): State<Arc<Standby>>) -> &'static str {
    if !standby.draining.swap(true, Ordering::SeqCst) {
        tracing::info!(
            "standby: drained with nothing in flight; staying down until this process stops"
        );
    }
    "draining 0"
}

async fn unavailable(State(standby): State<Arc<Standby>>) -> (StatusCode, String) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        format!("faber is in standby. {}\n", standby.last_error()),
    )
}

/// The page at `/`: static, so a caller holding only the session cookie (the
/// page itself needs no key) learns nothing host-specific from it. Its script
/// fetches the reason from `/api/state` with the session key, as the dashboard
/// does, and shows it as text, never as markup.
const PAGE_HTML: &str = concat!(
    r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="refresh" content="30">
<title>faber · standby</title>
<style>
"#,
    include_str!("../desktop/src/tokens.css"),
    r#"  body { font: 14px/1.5 ui-monospace, SFMono-Regular, Menlo, monospace;
         background: var(--bg); color: var(--fg); margin: 0; padding: 24px; }
  h1 { font-size: 18px; margin: 0 0 12px; }
  h1 span { color: var(--amber); }
  p { max-width: 90ch; }
  .muted { color: var(--muted); }
</style>
</head>
<body>
  <h1><span>●</span> faber · standby</h1>
  <p id="reason">faber is in standby.</p>
  <p class="muted">Nothing is claimed until this clears. The dashboard replaces this page
  when it does, and this page reloads itself.</p>
<script>
fetch('/api/state', {
  headers: {'X-Faber-Session': sessionStorage.getItem('faber-key') || ''},
  cache: 'no-store',
})
  .then(r => r.ok ? r.json() : null)
  .then(s => {
    if (s && typeof s.last_error === 'string' && s.last_error) {
      document.getElementById('reason').textContent = s.last_error;
    }
  })
  .catch(() => {});
</script>
</body>
</html>
"#
);

/// The stub, serving through the front door until [`StubServer::stop`].
struct StubServer {
    addr: SocketAddr,
    serving: crate::control::Serving,
}

impl StubServer {
    /// Bind the front door's listeners and serve. Fails only on a bind, which
    /// for the port waits out one still held for a moment after its last owner
    /// let it go, as the real control panel's bind does
    /// ([`crate::web::bind_listener`]).
    async fn start(
        door: &Arc<crate::control::FrontDoor>,
        standby: Arc<Standby>,
    ) -> std::io::Result<Self> {
        let serving = door.start(router(standby)).await?;
        Ok(Self {
            addr: serving.addr,
            serving,
        })
    }

    /// Stop serving, and return only once the listeners are closed, so the
    /// real dashboard can bind them straight after. A request still open past
    /// [`STOP_GRACE`] is dropped.
    async fn stop(self) {
        self.serving.stop(STOP_GRACE).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use serde::Deserialize;

    use super::*;

    /// A token filed under one owner that GitHub says acts as another is a
    /// host-side blocker naming both logins, never a release fault.
    #[test]
    fn a_token_acting_as_another_owner_blocks_with_both_logins() {
        let b = owner_mismatch("served-owner", "other-owner");
        assert_eq!(b.evidence, Evidence::Host);
        assert!(b.reason.contains("served-owner") && b.reason.contains("other-owner"));
        assert!(b.next.contains("sudo faber auth set"), "{}", b.next);
    }
    use crate::platform::record::{Account, Accounts, Installed, Operator, SetupRecord, SCHEMA};
    use crate::platform::TrustError;

    const FABER_UID: u32 = 451;
    const FABER_VM_UID: u32 = 452;
    const OPERATOR_UID: u32 = 501;

    fn record() -> SetupRecord {
        SetupRecord {
            schema: SCHEMA,
            operator: Operator {
                uid: OPERATOR_UID,
                name: "owner".into(),
            },
            accounts: Accounts {
                faber: Account {
                    uid: FABER_UID,
                    gid: FABER_UID,
                },
                faber_vm: Account {
                    uid: FABER_VM_UID,
                    gid: FABER_VM_UID,
                },
                faber_mac: None,
                operator_group: None,
            },
            installed: Installed {
                version: "0.3.13".into(),
            },
            migrated_from: None,
            artifacts: vec![],
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Want {
        Run,
        Refuse,
        Park,
    }

    /// Every layout against every account that could start `faber run`, as a
    /// service and at a terminal (`--once`).
    #[test]
    fn decide_truth_table() {
        let system = Resolution::System(record());
        let untrusted = Resolution::Untrusted(TrustError::NotRootOwned { uid: OPERATOR_UID });
        let legacy = Resolution::Legacy;
        let stray = "stray per-user agent: faber runs as _faber now (sudo faber setup)";
        let rows: &[(&str, &Resolution, u32, bool, Want, &str)] = &[
            // LEGACY runs whoever runs it, exactly as before ADR-0025.
            (
                "legacy operator",
                &legacy,
                OPERATOR_UID,
                false,
                Want::Run,
                "",
            ),
            ("legacy root", &legacy, 0, false, Want::Run, ""),
            ("legacy --once", &legacy, OPERATOR_UID, true, Want::Run, ""),
            // SYSTEM runs only as the record's _faber.
            ("system _faber", &system, FABER_UID, false, Want::Run, ""),
            (
                "system _faber --once",
                &system,
                FABER_UID,
                true,
                Want::Run,
                "",
            ),
            (
                "system root",
                &system,
                0,
                false,
                Want::Refuse,
                "as root is refused",
            ),
            (
                "system root --once",
                &system,
                0,
                true,
                Want::Refuse,
                "as root is refused",
            ),
            (
                "system operator agent",
                &system,
                OPERATOR_UID,
                false,
                Want::Park,
                stray,
            ),
            (
                "system _faber_vm",
                &system,
                FABER_VM_UID,
                false,
                Want::Park,
                stray,
            ),
            ("system stranger", &system, 1234, false, Want::Park, stray),
            (
                "system operator --once",
                &system,
                OPERATOR_UID,
                true,
                Want::Refuse,
                stray,
            ),
            // An untrusted record parks everyone: faber won't guess what was meant.
            (
                "untrusted _faber",
                &untrusted,
                FABER_UID,
                false,
                Want::Park,
                "untrusted",
            ),
            (
                "untrusted operator",
                &untrusted,
                OPERATOR_UID,
                false,
                Want::Park,
                "untrusted",
            ),
            (
                "untrusted root",
                &untrusted,
                0,
                false,
                Want::Park,
                "untrusted",
            ),
            (
                "untrusted --once",
                &untrusted,
                FABER_UID,
                true,
                Want::Refuse,
                "untrusted",
            ),
        ];
        for &(name, resolution, euid, once, want, needle) in rows {
            let got = decide(resolution, euid, once);
            let (kind, text) = match &got {
                BootDecision::Run => (Want::Run, ""),
                BootDecision::Refuse(why) => (Want::Refuse, why.as_str()),
                BootDecision::Park(why) => (Want::Park, why.as_str()),
            };
            assert_eq!(kind, want, "{name}: {got:?}");
            assert!(text.contains(needle), "{name}: {text}");
        }
    }

    #[test]
    fn a_parked_stray_names_both_accounts_and_takes_nothing() {
        let BootDecision::Park(why) = decide(&Resolution::System(record()), OPERATOR_UID, false)
        else {
            panic!("a stray agent parks");
        };
        assert!(
            why.starts_with("stray per-user agent: faber runs as _faber now (sudo faber setup)."),
            "{why}"
        );
        assert!(
            why.contains("uid 501") && why.contains("(uid 451)"),
            "{why}"
        );
        assert!(why.contains("no lock") && why.contains("no port"), "{why}");

        let untrusted = Resolution::Untrusted(TrustError::Writable { mode: 0o100666 });
        let BootDecision::Park(why) = decide(&untrusted, FABER_UID, false) else {
            panic!("an untrusted record parks");
        };
        assert!(
            why.starts_with(
                "setup record untrusted: /etc/faber/setup.json is group- or world-writable \
                 (mode 666)."
            ),
            "{why}"
        );
    }

    #[test]
    fn only_a_legacy_host_that_could_move_gets_the_banner() {
        let notice = legacy_notice(Layout::Legacy, true, "owner").expect("LEGACY warns");
        assert_eq!(notice.kind, "legacy_layout");
        assert_eq!(notice.text, "faber runs as owner; run `sudo faber setup`");
        assert_eq!(legacy_notice(Layout::System, true, "owner"), None);
        // An OS with no SYSTEM layout has nowhere to move to.
        assert_eq!(legacy_notice(Layout::Legacy, false, "owner"), None);
    }

    #[test]
    fn windows_is_told_it_cannot_hold_a_credential() {
        let next = legacy_or_system_next_step(Layout::Legacy, "owner", true);
        assert!(next.contains("Windows hosts cannot hold"), "{next}");
        assert!(next.contains("ADR-0027"), "{next}");
        for claim in ["sudo", "faber auth", "faber setup"] {
            assert!(!next.contains(claim), "{next}");
        }
        let unix = legacy_or_system_next_step(Layout::Legacy, "owner", false);
        assert!(unix.contains("`sudo faber setup`"), "{unix}");
    }

    /// The advice for a missing token is the same command sequence on both
    /// layouts, and never `gh` or the world-readable service environment. A
    /// LEGACY host is told to set up first: it has no store to hand a token to.
    #[test]
    fn a_missing_credential_says_whose_and_what_next() {
        let err = GithubError::Token(crate::credential::LoadError::Missing.defect());
        let system = credential_blocker(Layout::System, "owner", &err);
        assert_eq!(system.what, "a GitHub credential");
        assert_eq!(system.reason, "no GitHub token is stored");
        assert!(system.next.contains("`faber auth link`"), "{}", system.next);
        assert!(
            system.next.contains("`sudo faber auth set`"),
            "{}",
            system.next
        );
        assert!(system.next.contains("_faber"), "{}", system.next);
        assert!(system.next.contains("Never put a token"), "{}", system.next);

        let no_store = GithubError::Token(crate::credential::LoadError::NoStore.defect());
        let legacy = credential_blocker(Layout::Legacy, "owner", &no_store);
        assert!(legacy.reason.contains("ADR-0027"), "{}", legacy.reason);
        assert!(
            legacy.next.contains("`sudo faber setup`"),
            "{}",
            legacy.next
        );
        assert!(legacy.next.contains("owner's account"), "{}", legacy.next);
        for blocker in [&system, &legacy] {
            for gone in ["gh auth login", "GH_TOKEN", "GITHUB_TOKEN"] {
                assert!(!blocker.next.contains(gone), "{}", blocker.next);
                assert!(!blocker.reason.contains(gone), "{}", blocker.reason);
            }
        }

        let line = system.last_error(RETRY);
        assert!(
            line.starts_with("standby: could not get a GitHub credential: "),
            "{line}"
        );
        assert!(line.contains("Next step: "), "{line}");
        assert!(line.ends_with("faber retries every 60 s and claims nothing until then."));
    }

    #[test]
    fn a_failed_api_call_is_told_apart_by_how_it_failed() {
        let status = |status| GithubError::Status {
            status,
            url: "https://api.github.com/user".into(),
            body: String::new(),
        };
        let unauthorized = api_blocker(
            "the GitHub account's login",
            &status(401),
            Layout::Legacy,
            "owner",
        );
        assert!(unauthorized
            .next
            .starts_with("GitHub rejected the stored token"));
        let limited = api_blocker("x", &status(403), Layout::System, "owner");
        assert!(limited.next.contains("rate-limited"), "{}", limited.next);
        let token = api_blocker("x", &GithubError::Token("gone".into()), Layout::System, "o");
        assert!(
            token.next.contains("`sudo faber auth set`"),
            "{}",
            token.next
        );
    }

    // --- standby while an update is on probation (ADR-0006) -------------------

    fn status(status: u16) -> GithubError {
        GithubError::Status {
            status,
            url: "https://api.github.com/user".into(),
            body: String::new(),
        }
    }

    /// A transport-level failure, built without the network: a request whose
    /// URL does not parse never leaves the client.
    fn unanswered() -> GithubError {
        GithubError::from(
            reqwest::Client::new()
                .get("not a url")
                .build()
                .expect_err("a relative URL does not build"),
        )
    }

    /// Every way a GitHub call fails, and what it says about the binary.
    #[test]
    fn evidence_of_truth_table() {
        use Evidence::{Binary, Host, Unanswered};
        let decode = GithubError::from(serde_json::from_str::<u32>("{").unwrap_err());
        let mut rows: Vec<(String, GithubError, Evidence)> = vec![
            (
                "no credential".into(),
                GithubError::Token("not logged in".into()),
                Host,
            ),
            (
                "304 with nothing cached".into(),
                GithubError::StaleCache("u".into()),
                Host,
            ),
            ("a 2xx body that does not decode".into(), decode, Binary),
            ("transport, TLS or DNS".into(), unanswered(), Unanswered),
        ];
        // The host's or GitHub's: a rejected credential, a refusal or a rate
        // limit, and GitHub failing.
        for code in [401, 403, 429, 500, 502, 503, 504, 599] {
            rows.push((format!("status {code}"), status(code), Host));
        }
        // A malformed request to endpoints that do not change: the binary's.
        for code in [400, 404, 405, 406, 410, 415, 422] {
            rows.push((format!("status {code}"), status(code), Binary));
        }
        // Anything else waits: a quarantined good release is the dear mistake.
        for code in [301, 304, 402, 407, 408, 409, 411, 413, 418, 451] {
            rows.push((format!("status {code}"), status(code), Host));
        }
        for (name, err, want) in rows {
            assert_eq!(evidence_of(&err), want, "{name}: {err}");
        }
        // The blockers carry it.
        let no_login = GithubError::Token("not logged in".into());
        assert_eq!(
            credential_blocker(Layout::Legacy, "o", &no_login).evidence,
            Host
        );
        assert_eq!(
            api_blocker("x", &status(502), Layout::Legacy, "o").evidence,
            Host
        );
        assert_eq!(
            api_blocker("x", &status(404), Layout::Legacy, "o").evidence,
            Binary
        );
        assert_eq!(
            api_blocker("x", &unanswered(), Layout::Legacy, "o").evidence,
            Unanswered
        );
    }

    /// Evidence × what the reachability probe says: only an unanswered request
    /// asks the probe, and it is the binary's only when the host can reach
    /// GitHub without faber's HTTP stack.
    #[test]
    fn fault_truth_table() {
        use std::cell::Cell;
        let rows: &[(Evidence, bool, Fault, bool)] = &[
            // (evidence, probe says reachable, fault, probe asked)
            (Evidence::Host, true, Fault::Host, false),
            (Evidence::Host, false, Fault::Host, false),
            (Evidence::Binary, true, Fault::Binary, false),
            (Evidence::Binary, false, Fault::Binary, false),
            (Evidence::Unanswered, true, Fault::Binary, true),
            (Evidence::Unanswered, false, Fault::Host, true),
        ];
        for &(evidence, reachable, want, asked) in rows {
            let calls = Cell::new(0);
            let got = fault(evidence, || {
                calls.set(calls.get() + 1);
                reachable
            });
            assert_eq!(got, want, "{evidence:?} reachable={reachable}");
            assert_eq!(calls.get(), usize::from(asked), "{evidence:?}: probe calls");
        }
    }

    /// Probation × fault × how many binary faults in a row: only a run of
    /// [`PROBATION_ATTEMPTS`] binary faults on probation gives up.
    #[test]
    fn next_step_truth_table() {
        use Fault::{Binary, Host};
        use Next::{GiveUp, Wait};
        let rows: &[(bool, Fault, u32, Next)] = &[
            // Off probation: never, whatever failed and however often.
            (false, Binary, 1, Wait),
            (false, Binary, PROBATION_ATTEMPTS, Wait),
            (false, Binary, 1_000, Wait),
            (false, Host, 0, Wait),
            (false, Host, 1_000, Wait),
            // On probation, the host's fault: wait, and keep the marker.
            (true, Host, 0, Wait),
            (true, Host, PROBATION_ATTEMPTS, Wait),
            (true, Host, 1_000, Wait),
            // On probation, the binary's: a bounded number of tries.
            (true, Binary, 1, Wait),
            (true, Binary, PROBATION_ATTEMPTS - 1, Wait),
            (true, Binary, PROBATION_ATTEMPTS, GiveUp),
            (true, Binary, PROBATION_ATTEMPTS + 1, GiveUp),
        ];
        for &(probation, fault, in_a_row, want) in rows {
            assert_eq!(
                next_step(probation, fault, in_a_row),
                want,
                "probation={probation} fault={fault:?} in_a_row={in_a_row}"
            );
        }
    }

    fn reachable() -> bool {
        true
    }

    fn unreachable() -> bool {
        false
    }

    fn on_probation(retry: Duration, probe: fn() -> bool) -> StandbyOptions {
        StandbyOptions {
            probation: true,
            reachable: probe,
            ..opts(0, retry)
        }
    }

    fn failing(evidence: Evidence, reason: &str) -> Blocker {
        Blocker {
            evidence,
            ..blocker(reason)
        }
    }

    /// Run `until_ready` against an attempt that always fails with
    /// `evidence`, and say whether it gave up within `tries` attempts.
    async fn gives_up(opts: StandbyOptions, evidence: Evidence, tries: usize) -> Option<GaveUp> {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let standby = tokio::spawn(until_ready(opts, move || {
            let count = count.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(failing(evidence, "no answer"))
            }
        }));
        while !standby.is_finished() && calls.load(Ordering::SeqCst) < tries {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        if standby.is_finished() {
            return Some(standby.await.unwrap().expect_err("it never succeeds"));
        }
        standby.abort();
        None
    }

    /// On probation, a binary that keeps failing the way a broken release
    /// would gives up after [`PROBATION_ATTEMPTS`] tries and says why: a bad
    /// answer, or no answer while the host can reach GitHub.
    #[tokio::test]
    async fn on_probation_the_binarys_faults_give_up() {
        for (evidence, probe) in [
            (Evidence::Binary, unreachable as fn() -> bool),
            (Evidence::Unanswered, reachable),
        ] {
            let gave_up = gives_up(on_probation(Duration::from_millis(1), probe), evidence, 50)
                .await
                .unwrap_or_else(|| panic!("{evidence:?} must not wait forever"));
            assert_eq!(gave_up.attempts, PROBATION_ATTEMPTS, "{evidence:?}");
            let said = gave_up.to_string();
            assert!(said.contains("no answer"), "{said}");
            assert!(said.contains("without the clean-exit mark"), "{said}");
        }
    }

    /// On probation, the host's and GitHub's faults wait as they always do,
    /// and a request that got no answer waits too when the host cannot reach
    /// GitHub either: a network outage must not quarantine a good release.
    #[tokio::test]
    async fn on_probation_the_hosts_faults_wait() {
        for (evidence, probe) in [
            (Evidence::Host, reachable as fn() -> bool),
            (Evidence::Unanswered, unreachable),
        ] {
            let opts = on_probation(Duration::from_millis(1), probe);
            assert!(
                gives_up(opts, evidence, 20).await.is_none(),
                "{evidence:?} must wait"
            );
        }
    }

    /// A host fault ends a run of binary faults: never three in a row, so it
    /// never gives up.
    #[tokio::test]
    async fn on_probation_a_host_fault_resets_the_run() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let standby = tokio::spawn(until_ready(
            on_probation(Duration::from_millis(1), unreachable),
            move || {
                let count = count.clone();
                async move {
                    let n = count.fetch_add(1, Ordering::SeqCst) + 1;
                    let evidence = if n.is_multiple_of(3) {
                        Evidence::Host
                    } else {
                        Evidence::Binary
                    };
                    Err::<(), _>(failing(evidence, "flaky"))
                }
            },
        ));
        while calls.load(Ordering::SeqCst) < 20 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(!standby.is_finished(), "a host fault must reset the run");
        standby.abort();
    }

    static OFF_PROBATION_PROBES: AtomicUsize = AtomicUsize::new(0);

    fn counted_probe() -> bool {
        OFF_PROBATION_PROBES.fetch_add(1, Ordering::SeqCst);
        true
    }

    /// Off probation nothing changes: however many failures of any kind,
    /// standby waits, never asks the probe, and boots when GitHub answers.
    #[tokio::test]
    async fn off_probation_nothing_gives_up_and_nothing_is_probed() {
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let opts = StandbyOptions {
            reachable: counted_probe,
            ..opts(0, Duration::from_millis(1))
        };
        let outcome = until_ready(opts, move || {
            let count = count.clone();
            async move {
                let n = count.fetch_add(1, Ordering::SeqCst);
                match n % 3 {
                    _ if n >= 12 => Ok("booted"),
                    0 => Err(failing(Evidence::Binary, "bad answer")),
                    1 => Err(failing(Evidence::Unanswered, "no answer")),
                    _ => Err(failing(Evidence::Host, "not logged in")),
                }
            }
        })
        .await;
        assert_eq!(outcome, Ok("booted"));
        assert_eq!(calls.load(Ordering::SeqCst), 13);
        assert_eq!(OFF_PROBATION_PROBES.load(Ordering::SeqCst), 0);
    }

    // --- the reachability probe, without the network --------------------------

    fn loopback() -> std::io::Result<Vec<SocketAddr>> {
        Ok(vec![SocketAddr::from(([127, 0, 0, 1], 443))])
    }

    /// The system resolver has no timeout of its own: one that hangs reads as
    /// unreachable once the probe's bound has passed, not when it returns.
    #[test]
    fn a_resolver_that_hangs_reads_as_unreachable_within_the_bound() {
        let started = Instant::now();
        let reachable = reachable_via(
            || {
                std::thread::sleep(Duration::from_secs(2));
                loopback()
            },
            |_, _| panic!("nothing resolved, so nothing to connect to"),
            Duration::from_millis(50),
        );
        assert!(!reachable);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
    }

    /// What resolved is tried, at most two addresses, each with the bound; a
    /// name that does not resolve is unreachable.
    #[test]
    fn the_probe_connects_to_at_most_two_resolved_addresses() {
        use std::cell::Cell;
        let bound = Duration::from_secs(1);
        let tried = Cell::new(0);
        let five = || {
            Ok((1..=5u8)
                .map(|i| SocketAddr::from(([10, 0, 0, i], 443)))
                .collect())
        };
        let refused = |_: &SocketAddr, timeout: Duration| {
            assert_eq!(timeout, bound);
            tried.set(tried.get() + 1);
            false
        };
        assert!(!reachable_via(five, refused, bound));
        assert_eq!(tried.get(), 2);
        assert!(reachable_via(loopback, |addr, _| addr.port() == 443, bound));
        let no_such_host = || Err(std::io::Error::other("no such host"));
        assert!(!reachable_via(no_such_host, |_, _| true, bound));
    }

    // --- a signal while standing by (ADR-0011 §4) -------------------------------

    /// The updater's directory for one test, removed on drop.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let dir = std::env::temp_dir().join(format!(
                "faber-standby-{name}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn paths(&self) -> crate::update::UpdatePaths {
            crate::update::UpdatePaths::with_root(self.0.clone())
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A signal while standby waits out a host fault on probation is a
    /// deliberate exit: the stub stops and frees its port, and the clean-exit
    /// mark is written, so the next boot spends no probation attempt.
    #[tokio::test]
    async fn a_signal_in_standby_stops_it_and_leaves_the_clean_exit_mark() {
        let scratch = Scratch::new("signalled");
        let paths = scratch.paths();
        // The updater's marker for this boot, already counted once.
        let marker = r#"{"from":"0.1.0","to":"0.2.0","attempts":1,"written_ms":1}"#;
        std::fs::create_dir_all(paths.marker().parent().unwrap()).unwrap();
        std::fs::write(paths.marker(), marker).unwrap();

        let port = free_port().await;
        let door = test_door(port);
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let (signal, signalled) = tokio::sync::oneshot::channel::<()>();
        let standby = tokio::spawn(until_ready_or_signalled(
            StandbyOptions {
                door: door.clone(),
                ..on_probation(Duration::from_millis(5), unreachable)
            },
            move || {
                let count = count.clone();
                async move {
                    count.fetch_add(1, Ordering::SeqCst);
                    Err::<(), _>(failing(Evidence::Host, "not logged in"))
                }
            },
            async {
                let _ = signalled.await;
            },
            Some(paths.clone()),
        ));
        while calls.load(Ordering::SeqCst) < 3 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(
            get_state(&signed_in(&door), &format!("http://127.0.0.1:{port}")).await["healthy"],
            false
        );
        assert!(!paths.clean_exit().exists());

        signal.send(()).unwrap();
        assert_eq!(standby.await.unwrap(), Err(Stop::Signalled));
        assert!(
            paths.clean_exit().exists(),
            "a signalled standby leaves the mark"
        );
        if let Err(why) = rebind(port).await {
            panic!("a signalled standby must free its port: {why}");
        }

        // The next boot: excused, so the attempt is not spent.
        let config = scratch.0.join("config.toml");
        let report = crate::update::rollback_if_needed(&paths, "0.2.0", &config).unwrap();
        assert!(!report.restored);
        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(paths.marker()).unwrap()).unwrap();
        assert_eq!(
            after["attempts"], 1,
            "a deliberate stop is not a failed boot"
        );
    }

    /// Giving up on probation is the exit probation exists to count: it writes
    /// no clean-exit mark.
    #[tokio::test]
    async fn giving_up_on_probation_leaves_no_clean_exit_mark() {
        let scratch = Scratch::new("gave-up");
        let paths = scratch.paths();
        let outcome = until_ready_or_signalled(
            on_probation(Duration::from_millis(1), unreachable),
            || async { Err::<(), _>(failing(Evidence::Binary, "bad answer")) },
            std::future::pending(),
            Some(paths.clone()),
        )
        .await;
        assert!(
            matches!(&outcome, Err(Stop::GaveUp(g)) if g.attempts == PROBATION_ATTEMPTS),
            "{outcome:?}"
        );
        assert!(
            !paths.clean_exit().exists(),
            "a failed probation boot must count"
        );
    }

    // --- the stub's state, read by everything that reads the real one --------

    /// The daemon's front door on `port`, loopback only: these tests drive
    /// the stub over TCP, and `control`'s own tests drive the socket.
    fn test_door(port: u16) -> Arc<crate::control::FrontDoor> {
        crate::control::FrontDoor::new(
            port,
            None,
            0o600,
            None,
            euid(),
            Arc::new(crate::control::Sessions::system()),
        )
    }

    fn opts(port: u16, retry: Duration) -> StandbyOptions {
        StandbyOptions {
            door: test_door(port),
            retry,
            topic: "faber".into(),
            audit_path: "/Library/Logs/faber/audit.jsonl".into(),
            notices: vec![Notice {
                kind: "legacy_layout".into(),
                text: "faber runs as owner; run `sudo faber setup`".into(),
            }],
            probation: false,
            // Off probation the probe is never asked; this one would say the
            // network is fine, which is the answer that could give up.
            reachable,
        }
    }

    /// A failure that is the binary's: the kind probation gives up on, and
    /// the kind standby off probation must still wait out.
    fn blocker(reason: &str) -> Blocker {
        Blocker {
            what: "a GitHub credential",
            reason: reason.to_string(),
            next: "log in".to_string(),
            evidence: Evidence::Binary,
        }
    }

    fn default_true() -> bool {
        true
    }

    // The menu-bar app's `StateView` and the types it nests, copied field for
    // field from `desktop/src-tauri/src/lib.rs`. The app is deployed apart from
    // the daemon and deserializes this strictly: a required field missing from
    // the stub would show the whole tray as offline instead of unhealthy.

    #[derive(Debug, Deserialize, Default)]
    #[serde(default)]
    #[allow(dead_code)]
    struct TrayCounts {
        provisioned: u64,
        completed: u64,
        faults: u64,
        refused: u64,
        retried: u64,
    }

    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    struct TrayProfileView {
        name: String,
        labels: Vec<String>,
        backend: String,
    }

    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    struct TrayInflightView {
        job_id: u64,
        profile: String,
        #[serde(default)]
        repo: String,
        #[serde(default)]
        admitted: bool,
    }

    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    struct TrayDecision {
        ts_ms: u64,
        repo: String,
        job_id: u64,
        event: String,
        actor: String,
        verdict: String,
        #[serde(default)]
        profile: String,
    }

    #[derive(Debug, Deserialize, Default)]
    #[serde(default)]
    #[allow(dead_code)]
    struct TrayUpdateView {
        current: String,
        enabled: bool,
        available: Option<String>,
        phase: serde_json::Value,
        last_check_ms: Option<u64>,
        last_error: Option<String>,
        quarantined: Vec<String>,
        rolled_back_from: Option<String>,
        pin: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    #[allow(dead_code)]
    struct TrayStateView {
        owner: String,
        topic: String,
        profiles: Vec<TrayProfileView>,
        concurrency: u32,
        paused: bool,
        draining: bool,
        uptime_s: u64,
        audit_path: String,
        inflight: Vec<TrayInflightView>,
        counts: TrayCounts,
        recent: Vec<TrayDecision>,
        #[serde(default)]
        outcomes: Vec<serde_json::Value>,
        #[serde(default)]
        faults: Vec<serde_json::Value>,
        #[serde(default = "default_true")]
        healthy: bool,
        #[serde(default)]
        last_error: Option<String>,
        #[serde(default)]
        budget: serde_json::Value,
        #[serde(default)]
        update: TrayUpdateView,
    }

    /// The fields the tray requires, which must be present rather than
    /// defaulted: the keys of every `TrayStateView` field without
    /// `#[serde(default)]`.
    const TRAY_REQUIRED: &[&str] = &[
        "owner",
        "topic",
        "profiles",
        "concurrency",
        "paused",
        "draining",
        "uptime_s",
        "audit_path",
        "inflight",
        "counts",
        "recent",
    ];

    #[test]
    fn the_stub_state_parses_as_the_tray_drain_and_status_read_it() {
        let standby = Standby::new(opts(8787, RETRY), blocker("gh: not logged in"));
        let json = serde_json::to_value(standby.state()).unwrap();
        for key in TRAY_REQUIRED {
            assert!(json.get(key).is_some(), "the tray requires {key:?}: {json}");
        }

        // The menu-bar app: unhealthy, with the reason and the next step.
        let tray: TrayStateView = serde_json::from_value(json.clone()).expect("the tray parses it");
        assert!(!tray.healthy);
        assert!(tray.inflight.is_empty() && tray.profiles.is_empty());
        let last = tray.last_error.expect("a reason");
        assert!(
            last.contains("gh: not logged in") && last.contains("Next step: log in"),
            "{last}"
        );
        assert_eq!(tray.topic, "faber");
        assert_eq!(tray.update.current, env!("FABER_VERSION"));

        // `faber status`: the real `Live`, which knows it is looking at standby.
        let live: crate::status::Live =
            serde_json::from_value(json.clone()).expect("status parses it");
        assert!(!live.healthy && !live.paused && !live.draining);
        assert!(live.inflight.is_empty());
        assert!(live.standby.is_some());
        assert_eq!(live.last_error.as_deref(), Some(last.as_str()));

        // `faber drain`: nothing in flight.
        assert_eq!(crate::drain::inflight_count(&json), Some(0));

        // The dashboard's banners ride along.
        assert_eq!(json["notices"][0]["kind"], "legacy_layout");
        assert_eq!(json["standby"]["retry_secs"], 60);

        // So does the credential (ADR-0027), whatever this host's store holds,
        // and never a token.
        let credential = json["credential"].as_object().expect("a credential object");
        assert!(credential["status"].is_string(), "{json}");
        assert!(!credential.contains_key("token"), "{json}");
        assert_eq!(json["trust_changes"], serde_json::json!([]));
    }

    // --- the stub on a real socket ------------------------------------------

    fn client() -> reqwest::Client {
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap()
    }

    /// A port nothing is listening on right now.
    async fn free_port() -> u16 {
        let probe = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        probe.local_addr().unwrap().port()
    }

    /// Bind `127.0.0.1:port` exactly as the real control panel does, or say
    /// exactly why not: which error, which port.
    ///
    /// Through the panel's own bind ([`crate::web::bind_listener`]), not a bare
    /// one, and the difference matters here. While another test in this binary
    /// spawns a process (`update`'s `ssh-keygen`, `id -u`, ...), the child holds a
    /// copy of every open descriptor until it calls `exec`. A listener closed in
    /// that window keeps its port for 20 µs to 2 ms, so a bare bind straight
    /// after `stop()` failed with `AddrInUse` in about 15% of full parallel runs,
    /// and in none with the standby tests alone or the suite run serially. The
    /// panel's bind waits that out, as the daemon must, for at most two seconds;
    /// `web`'s `the_panel_gives_up_on_a_port_that_stays_in_use` proves the wait
    /// ends and reports, so a stub that never let go still fails here.
    async fn rebind(port: u16) -> Result<tokio::net::TcpListener, String> {
        crate::web::bind_listener(
            SocketAddr::from(([127, 0, 0, 1], port)),
            crate::web::BIND_ATTEMPTS,
            crate::web::BIND_PAUSE,
        )
        .await
        .map_err(|e| format!("127.0.0.1:{port}: {:?}, {e}", e.kind()))
    }

    /// A client holding a dashboard session opened through `door`, as the
    /// browser would after `faber dashboard`: the cookie, the key, and the
    /// dashboard's own origin.
    fn signed_in(door: &crate::control::FrontDoor) -> reqwest::Client {
        let token = door.sessions().mint_link().unwrap();
        let session = door.sessions().redeem(&token).unwrap().unwrap();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::COOKIE,
            format!("faber_session={}", session.cookie).parse().unwrap(),
        );
        headers.insert("x-faber-session", session.key.parse().unwrap());
        headers.insert(
            reqwest::header::ORIGIN,
            format!("http://127.0.0.1:{}", door.port()).parse().unwrap(),
        );
        reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(5))
            .default_headers(headers)
            .build()
            .unwrap()
    }

    async fn get_state(client: &reqwest::Client, base: &str) -> serde_json::Value {
        client
            .get(format!("{base}/api/state"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_stub_releases_its_port_when_it_stops() {
        let port = free_port().await;
        let options = opts(port, RETRY);
        let door = options.door.clone();
        let standby = Arc::new(Standby::new(options, blocker("offline")));
        let stub = StubServer::start(&door, standby).await.unwrap();
        assert_eq!(stub.addr.port(), port);
        let base = format!("http://127.0.0.1:{port}");
        assert_eq!(get_state(&signed_in(&door), &base).await["healthy"], false);
        // While it serves, nothing else can have the port.
        assert!(tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .is_err());

        stub.stop().await;
        // Straight after: free for the real dashboard.
        if let Err(why) = rebind(port).await {
            panic!("the stub still holds its port: {why}");
        }
    }

    /// The whole standby: the first attempt fails, the stub answers between
    /// attempts with the latest reason, and once an attempt succeeds the port
    /// is free before `until_ready` returns.
    #[tokio::test]
    async fn standby_answers_until_ready_then_frees_the_port() {
        let port = free_port().await;
        let options = opts(port, Duration::from_millis(50));
        let client = signed_in(&options.door);
        let calls = Arc::new(AtomicUsize::new(0));
        let seen = Arc::new(Mutex::new(None::<serde_json::Value>));
        let (count, record) = (calls.clone(), seen.clone());
        let ready = until_ready(options, move || {
            let (count, record, client) = (count.clone(), record.clone(), client.clone());
            async move {
                match count.fetch_add(1, Ordering::SeqCst) + 1 {
                    1 => Err(blocker("GitHub unreachable")),
                    2 => {
                        // Between attempts the stub is up: read it as the tray would.
                        let state = get_state(&client, &format!("http://127.0.0.1:{port}")).await;
                        *record.lock().unwrap() = Some(state);
                        Err(blocker("still unreachable"))
                    }
                    _ => Ok("ready"),
                }
            }
        })
        .await;
        assert_eq!(ready, Ok("ready"));
        assert_eq!(calls.load(Ordering::SeqCst), 3);

        let state = seen.lock().unwrap().take().expect("the stub answered");
        assert_eq!(state["healthy"], false);
        assert!(
            state["last_error"]
                .as_str()
                .is_some_and(|e| e.contains("GitHub unreachable")),
            "{state}"
        );
        assert_eq!(state["inflight"], serde_json::json!([]));

        if let Err(why) = rebind(port).await {
            panic!("the stub still holds its port: {why}");
        }
    }

    /// No first failure, no stub: a daemon GitHub answers boots as it always
    /// did, without serving anything first.
    #[tokio::test]
    async fn a_first_success_serves_nothing() {
        let port = free_port().await;
        let ready = until_ready(opts(port, Duration::from_millis(10)), || async {
            Ok::<_, Blocker>(7)
        })
        .await;
        assert_eq!(ready, Ok(7));
        if let Err(why) = rebind(port).await {
            panic!("nothing should hold the port: {why}");
        }
    }

    /// The static page shows the reason as text, fetched with the session key.
    #[test]
    fn the_standby_page_fetches_the_reason_as_text() {
        assert!(PAGE_HTML.contains("fetch('/api/state'"));
        assert!(PAGE_HTML.contains("'X-Faber-Session': sessionStorage.getItem('faber-key')"));
        assert!(PAGE_HTML.contains(".textContent = s.last_error"));
        assert!(!PAGE_HTML.contains("innerHTML"));
    }

    /// Only state, the page and drain work in standby; the rest says why not.
    /// A drain is immediate, guarded like the real one, and final: no further
    /// attempt is made, so a daemon asked to stand down never boots.
    #[tokio::test]
    async fn standby_serves_state_and_drain_and_refuses_the_rest() {
        let port = free_port().await;
        let options = opts(port, Duration::from_millis(20));
        let session = signed_in(&options.door);
        let calls = Arc::new(AtomicUsize::new(0));
        let count = calls.clone();
        let standby = tokio::spawn(until_ready(options, move || {
            let count = count.clone();
            async move {
                count.fetch_add(1, Ordering::SeqCst);
                Err::<(), _>(blocker("<gh> & friends"))
            }
        }));
        let base = format!("http://127.0.0.1:{port}");
        let mut up = false;
        for _ in 0..200 {
            if session
                .clone()
                .get(format!("{base}/api/state"))
                .send()
                .await
                .is_ok()
            {
                up = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert!(up, "the stub never answered");

        // Without a session, the stub says nothing but how to open one.
        let anonymous = client()
            .get(format!("{base}/api/state"))
            .send()
            .await
            .unwrap();
        assert_eq!(anonymous.status(), 401);
        assert!(!anonymous.text().await.unwrap().contains("friends"));

        // The page is static: it carries nothing about this host, and its
        // script asks the state API, with the session key, for the reason.
        let page = session
            .clone()
            .get(format!("{base}/"))
            .send()
            .await
            .unwrap();
        assert_eq!(page.status(), 200);
        let html = page.text().await.unwrap();
        assert_eq!(html, PAGE_HTML);
        assert!(!html.contains("friends"), "{html}");
        // The reason is in the state, for a caller holding the key.
        let state = get_state(&session, &base).await;
        assert!(
            state["last_error"]
                .as_str()
                .is_some_and(|e| e.contains("<gh> & friends")),
            "{state}"
        );

        // Everything else: 503, with the reason.
        for (method, path) in [
            ("GET", "/api/settings"),
            ("POST", "/api/pause"),
            ("POST", "/api/state"),
            ("GET", "/api/drain"),
            ("POST", "/api/update/check"),
        ] {
            let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
            let resp = session
                .clone()
                .request(method.clone(), format!("{base}{path}"))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), 503, "{method} {path}");
            assert!(resp.text().await.unwrap().contains("<gh> & friends"));
        }

        // A cross-origin drain is refused, like the real one.
        let forged = session
            .clone()
            .post(format!("{base}/api/drain"))
            .header("Origin", "https://evil.example")
            .send()
            .await
            .unwrap();
        assert_eq!(forged.status(), 403);
        assert_eq!(get_state(&session, &base).await["draining"], false);

        // The real thing: done at once, with nothing in flight.
        let drained = session
            .clone()
            .post(format!("{base}/api/drain"))
            .header("Origin", &base)
            .send()
            .await
            .unwrap();
        assert!(drained.status().is_success());
        assert_eq!(drained.text().await.unwrap(), "draining 0");
        let state = get_state(&session, &base).await;
        assert_eq!(state["draining"], true);
        assert_eq!(crate::drain::inflight_count(&state), Some(0));

        // And final: at most the attempt already under way, then none.
        let after = calls.load(Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(calls.load(Ordering::SeqCst) <= after + 1);
        assert!(!standby.is_finished(), "a drained standby must not boot");
        standby.abort();
    }
}
