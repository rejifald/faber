//! Restarting the VM supervisor onto a new launcher binary (ADR-0026, "The
//! supervisor updates without stopping a VM", steps 3 and 4).
//!
//! After a release swaps the launcher binary and checks its `--version` and
//! `validate` (slice 2's job, not this module's), faber asks the running
//! supervisor to `exit`. launchd starts it again on the new binary, and the new
//! supervisor adopts the running per-VM launcher children by their claim markers.
//! `exit` stops no VM: the LaunchDaemon sets `AbandonProcessGroup`.
//!
//! [`SupervisorRestart`] is the seam slice 2's updater calls once the swap is in
//! place and boot probation has settled (`update::restart_supervisor_after_proof`). [`ClientRestart`] is the real implementation over
//! the process's [`SupervisorClient`].
//!
//! It is deliberately conservative, since a release reaches hosts nobody is
//! watching (AGENTS.md): it sends one `exit` per call, never a second to a
//! supervisor that comes back on the old binary, and it refuses to run again
//! within [`RestartPolicy::min_interval`], so a release that keeps failing its
//! restart cannot bounce the supervisor in a loop.

use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use tokio::time::{sleep, Instant};

use super::client::{ClientError, SupervisorClient};
use super::proto::SupervisorVersion;

/// How a restart ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartOutcome {
    /// A supervisor at the expected version answered the handshake.
    Restarted { version: SupervisorVersion },
    /// No supervisor completed a handshake before the deadline.
    TimedOut,
    /// A supervisor answered, but at the deadline it still reported this
    /// version: launchd restarted the old binary, or it never exited.
    WrongVersion { seen: SupervisorVersion },
}

/// Why a restart was not attempted.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RestartError {
    /// A restart ran too recently.
    #[error(
        "the VM supervisor was restarted {ago:?} ago; the next restart is allowed in {wait:?}"
    )]
    TooSoon { ago: Duration, wait: Duration },
    /// The `exit` could not be delivered: the supervisor was unreachable or
    /// refused it, so it is still running whatever it ran.
    #[error("could not ask the VM supervisor to exit: {0}")]
    NotAsked(ClientError),
}

/// The seam the updater calls after it has installed and checked a new
/// launcher binary.
pub trait SupervisorRestart: Send + Sync {
    /// Ask the supervisor to exit, then wait for one reporting
    /// `expected_version`. Never stops a VM.
    fn restart_supervisor(
        &self,
        expected_version: &str,
    ) -> impl Future<Output = Result<RestartOutcome, RestartError>> + Send;
}

/// How patiently, and how rarely, a restart runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    /// How long to wait for the new supervisor after the `exit`. launchd's own
    /// throttle restarts a job no sooner than 10 s after its last start, so this
    /// leaves several of those.
    pub deadline: Duration,
    /// The first pause between reconnects; it doubles up to `max_backoff`.
    pub first_backoff: Duration,
    pub max_backoff: Duration,
    /// The least time between two restarts, counted from the start of the last
    /// attempt whatever its outcome.
    pub min_interval: Duration,
}

/// [`RestartPolicy::deadline`]'s default: how long a restarted supervisor may
/// take to answer again.
pub const RESTART_DEADLINE: Duration = Duration::from_secs(60);

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            deadline: RESTART_DEADLINE,
            first_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(5),
            min_interval: Duration::from_secs(10 * 60),
        }
    }
}

/// [`SupervisorRestart`] over the socket client.
pub struct ClientRestart<'a> {
    client: &'a SupervisorClient,
    policy: RestartPolicy,
    last: Mutex<Option<Instant>>,
}

impl<'a> ClientRestart<'a> {
    pub fn new(client: &'a SupervisorClient, policy: RestartPolicy) -> Self {
        Self {
            client,
            policy,
            last: Mutex::new(None),
        }
    }

    /// Claim the right to restart now, or say how long until it is allowed.
    fn admit(&self) -> Result<(), RestartError> {
        let mut last = self.last.lock().unwrap();
        let now = Instant::now();
        if let Some(at) = *last {
            let ago = now.saturating_duration_since(at);
            if ago < self.policy.min_interval {
                return Err(RestartError::TooSoon {
                    ago,
                    wait: self.policy.min_interval - ago,
                });
            }
        }
        *last = Some(now);
        Ok(())
    }
}

impl SupervisorRestart for ClientRestart<'_> {
    async fn restart_supervisor(
        &self,
        expected_version: &str,
    ) -> Result<RestartOutcome, RestartError> {
        self.admit()?;

        // A fresh connection, so the `exit` goes to whatever is listening now.
        // The client sends it whether the handshake succeeds or reports
        // `version_mismatch`: `exit` is in the frozen core.
        self.client.disconnect().await;
        match self.client.exit().await {
            Ok(()) => {}
            // Sent, but no `done`: the supervisor may well be exiting. Waiting
            // for its successor is the only way to find out.
            Err(ClientError::NoReply | ClientError::Lost) => {
                tracing::warn!("the VM supervisor did not confirm its exit; waiting for it anyway");
            }
            Err(e) => return Err(RestartError::NotAsked(e)),
        }

        let deadline = Instant::now() + self.policy.deadline;
        let mut backoff = self.policy.first_backoff;
        let mut seen = None;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            sleep(backoff.min(left)).await;
            backoff = (backoff * 2).min(self.policy.max_backoff);
            // The old supervisor may still answer for a moment after `done`; only
            // the expected version ends the wait.
            match self.client.handshake().await {
                Ok(v) if v.as_str() == expected_version => {
                    return Ok(RestartOutcome::Restarted { version: v });
                }
                Ok(v) => seen = Some(v),
                Err(_) => {}
            }
        }
        Ok(match seen {
            Some(seen) => RestartOutcome::WrongVersion { seen },
            None => RestartOutcome::TimedOut,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::client::fake::*;
    use super::super::client::{Endpoint, Timeouts};
    use super::super::proto::{Empty, Reply, Request};
    use super::*;

    fn client(dir: &SockDir) -> SupervisorClient {
        SupervisorClient::with(
            Endpoint {
                path: dir.sock(),
                peer_uid: dir.euid(),
            },
            Timeouts {
                connect: Duration::from_secs(1),
                request: Duration::from_secs(1),
                start: Duration::from_secs(1),
                idle_reuse: Duration::from_secs(20),
            },
            "0.5.0",
        )
    }

    fn policy(deadline_ms: u64) -> RestartPolicy {
        RestartPolicy {
            deadline: Duration::from_millis(deadline_ms),
            first_backoff: Duration::from_millis(20),
            max_backoff: Duration::from_millis(100),
            min_interval: Duration::from_secs(600),
        }
    }

    /// Once `old` has exited, wait a beat and bring up a supervisor at `version`
    /// on the same path, the way launchd would.
    fn relaunch(dir: &SockDir, old: &Fake, version: &'static str) -> tokio::task::JoinHandle<Fake> {
        let path = dir.sock();
        let exited = old.exited.clone();
        tokio::spawn(async move {
            exited.notified().await;
            sleep(Duration::from_millis(150)).await;
            serve(&path, plain(version, |_| Act::Hang))
        })
    }

    #[tokio::test]
    async fn exit_then_the_new_version_answers() {
        let dir = SockDir::new();
        let old = serve(&dir.sock(), plain("0.4.0", |_| Act::Hang));
        let new = relaunch(&dir, &old, "0.5.0");
        let c = client(&dir);
        let r = ClientRestart::new(&c, policy(5_000));
        let outcome = r.restart_supervisor("0.5.0").await.unwrap();
        assert_eq!(
            outcome,
            RestartOutcome::Restarted {
                version: SupervisorVersion::try_from("0.5.0").unwrap()
            }
        );
        assert_eq!(
            old.stats.ops(),
            ["hello", "exit"],
            "one exit, nothing stopped"
        );
        assert_eq!(new.await.unwrap().stats.ops(), ["hello"]);
        assert_eq!(c.supervisor_version().unwrap().as_str(), "0.5.0");
    }

    #[tokio::test]
    async fn an_old_protocol_supervisor_is_retired_through_the_frozen_core() {
        let dir = SockDir::new();
        let old = serve(
            &dir.sock(),
            Arc::new(|_, req| match req {
                Request::Hello(_) => Act::Reply(mismatch(1)),
                Request::Exit(_) => Act::ReplyThenExit(Reply::Done(Empty {})),
                _ => Act::Hang,
            }),
        );
        let _new = relaunch(&dir, &old, "0.5.0");
        let c = client(&dir);
        let outcome = ClientRestart::new(&c, policy(5_000))
            .restart_supervisor("0.5.0")
            .await
            .unwrap();
        assert!(
            matches!(outcome, RestartOutcome::Restarted { .. }),
            "{outcome:?}"
        );
        assert_eq!(old.stats.ops(), ["hello", "exit"]);
    }

    #[tokio::test]
    async fn a_supervisor_still_on_the_old_binary_is_wrong_version() {
        let dir = SockDir::new();
        // Says `done` but keeps running the old version.
        let old = serve(
            &dir.sock(),
            Arc::new(|_, req| match req {
                Request::Hello(_) => Act::Reply(hello("0.4.0")),
                _ => Act::Reply(Reply::Done(Empty {})),
            }),
        );
        let c = client(&dir);
        let outcome = ClientRestart::new(&c, policy(400))
            .restart_supervisor("0.5.0")
            .await
            .unwrap();
        assert_eq!(
            outcome,
            RestartOutcome::WrongVersion {
                seen: SupervisorVersion::try_from("0.4.0").unwrap()
            }
        );
        let ops = old.stats.ops();
        assert_eq!(
            ops.iter().filter(|o| *o == "exit").count(),
            1,
            "never a second exit"
        );
    }

    #[tokio::test]
    async fn nothing_coming_back_times_out() {
        let dir = SockDir::new();
        let _old = serve(&dir.sock(), plain("0.4.0", |_| Act::Hang));
        let c = client(&dir);
        let outcome = ClientRestart::new(&c, policy(300))
            .restart_supervisor("0.5.0")
            .await
            .unwrap();
        assert_eq!(outcome, RestartOutcome::TimedOut);
    }

    #[tokio::test]
    async fn an_unreachable_supervisor_is_not_asked_and_a_second_restart_waits() {
        let dir = SockDir::new();
        let c = client(&dir);
        let r = ClientRestart::new(&c, policy(300));
        let err = r.restart_supervisor("0.5.0").await.unwrap_err();
        assert!(
            matches!(err, RestartError::NotAsked(ClientError::Unreachable(_))),
            "{err:?}"
        );
        // Even a failed attempt counts against the interval.
        let err = r.restart_supervisor("0.5.0").await.unwrap_err();
        assert!(matches!(err, RestartError::TooSoon { .. }), "{err:?}");
    }
}
