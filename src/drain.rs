//! `faber drain` — stand this host down for good (ADR-0011 §5).
//!
//! Since ADR-0011 a bare SIGTERM means "I'm coming back": the daemon exits at
//! once and the next process rebuilds its claims from the containers still
//! running. That is right for `kickstart -k` and for the
//! bootout/install/bootstrap redeploy — and wrong for `launchctl bootout` as a
//! *decommission*, where nothing comes back to adopt anything.
//!
//! The two are the same signal, so the process cannot tell them apart. This is
//! the other half of that decision: decommissioning is made explicit, because it
//! already is — a human running an uninstall script.
//!
//! Ask the daemon to stop claiming, then watch until the work it already
//! admitted has finished. Polling `/api/state` rather than holding one long
//! request open: the wait is bounded by the runner lifetime cap, which is hours.

use std::borrow::Cow;
use std::path::Path;
use std::time::Duration;

use crate::control::{self, ClientError, Fallback};

/// How often to re-read `/api/state` while waiting.
const POLL: Duration = Duration::from_secs(5);
/// The most of the daemon's `/api/drain` reply this prints. The real reply is a
/// few words (`draining 2`); the bound is for a daemon that says more.
const REPLY_MAX: usize = 2048;

/// What a failed control request means, in words for the operator.
///
/// A refused caller is told by [`control::refused_text`] who may use the socket.
pub(crate) fn client_error_message(e: &ClientError) -> String {
    match e {
        ClientError::NoDaemon { socket, fallback } => format!(
            "no daemon answering on {}{}.{}",
            socket.display(),
            fallback.or_tcp(),
            match fallback {
                Fallback::Tcp(_) => " If it is on another port, pass --port.",
                Fallback::Never => "",
            }
        ),
        ClientError::Refused { socket } => control::refused_text(socket),
        ClientError::Transport(why) => format!("talking to the faber daemon: {why}"),
    }
}

/// Drain the daemon behind `socket` (or, where `fallback` allows it, a daemon
/// from before the socket on its port), waiting up to `wait` for it to go idle.
///
/// Returns `Ok(())` only when the host is genuinely idle. A timeout is an error
/// on purpose: the caller is an uninstall script about to boot the daemon out,
/// and "I gave up waiting" must not read as "it is safe to proceed".
pub async fn run(socket: &Path, fallback: Fallback, wait: Duration) -> anyhow::Result<()> {
    let (status, said) =
        control::request(socket, fallback, reqwest::Method::POST, "/api/drain", None)
            .await
            .map_err(|e| anyhow::anyhow!("{} Nothing to drain.", client_error_message(&e)))?;

    // A reply is not consent. Checked because the failure it catches is silent
    // and dangerous: a daemon older than ADR-0011 has no `/api/drain` and answers
    // 404, which a bare send reports as success — so this would print nothing, poll a
    // host that was never asked to stand down, and eventually let an uninstall
    // script boot out a daemon still running jobs. Seen once in practice, after
    // the fleet updater moved a host back to a release without the endpoint.
    if !status.is_success() {
        anyhow::bail!(
            "the daemon ({}{}) refused /api/drain ({status}). \
             {} Nothing has been drained.",
            socket.display(),
            fallback.or_tcp(),
            if status.as_u16() == 404 {
                "It is older than ADR-0011 and has no drain endpoint — stop it with \
                 `launchctl bootout` and accept the drain it does on SIGTERM."
            } else {
                "Check the daemon's log."
            }
        );
    }
    println!("{}", reply_line(&said));

    let deadline = std::time::Instant::now() + wait;
    loop {
        let left = inflight(socket, fallback).await;
        match left {
            Some(0) => {
                println!("drained — no runners in flight; safe to bootout");
                return Ok(());
            }
            Some(n) => println!("waiting for {n} runner(s) to finish…"),
            // A daemon that stops answering mid-drain has exited on its own, which
            // is the outcome being waited for. Reporting an error here would send
            // an uninstall script down the failure path for a success.
            None => {
                println!("daemon no longer answering — it exited; nothing left in flight");
                return Ok(());
            }
        }
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "still draining after {:?} — did not stand down. Runners are still \
                 executing jobs; booting the daemon out now would orphan them.",
                wait
            );
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The daemon's reply as this command prints it: bounded, and with every
/// character a terminal would act on made visible ([`crate::term`]). The text
/// is the daemon's to choose, and under ADR-0025 `sudo faber setup` and
/// `sudo faber uninstall` run this as root: `_faber`'s words in root's session.
fn reply_line(said: &str) -> Cow<'_, str> {
    crate::term::sanitize_bounded(said.trim(), REPLY_MAX)
}

/// In-flight claim count, or `None` if the daemon is not answering.
async fn inflight(socket: &Path, fallback: Fallback) -> Option<usize> {
    let (status, text) =
        control::request(socket, fallback, reqwest::Method::GET, "/api/state", None)
            .await
            .ok()?;
    if !status.is_success() {
        return None;
    }
    inflight_count(&serde_json::from_str(&text).ok()?)
}

/// The one field this command reads from `GET /api/state`: how many claims are
/// in flight. A daemon in standby ([`crate::standby`]) answers with none, and
/// its `POST /api/drain` succeeds, so draining it is immediate.
pub(crate) fn inflight_count(state: &serde_json::Value) -> Option<usize> {
    Some(state.get("inflight")?.as_array()?.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::term::TRUNCATED;

    /// The daemon chooses this text, and root may be the one reading it: a
    /// clipboard write, a line erase and a carriage return that would redraw
    /// the line as a fake prompt all come out as visible text.
    #[test]
    fn the_reply_is_printed_neutralized_and_bounded() {
        assert_eq!(reply_line("draining 2\n"), "draining 2");
        let evil = "draining 0\u{1b}]52;c;cm0gLXJmIH4=\u{7}\u{1b}[2K\rPassword:";
        assert_eq!(
            reply_line(evil),
            "draining 0\\x1b]52;c;cm0gLXJmIH4=\\x07\\x1b[2K\\x0dPassword:"
        );
        let huge = "x".repeat(1 << 20);
        let long = reply_line(&huge);
        assert_eq!(long.len(), REPLY_MAX + TRUNCATED.len());
        assert!(long.ends_with(TRUNCATED));
    }

    #[test]
    fn a_refused_caller_is_told_the_socket_and_who_may_use_it() {
        let msg = client_error_message(&ClientError::Refused {
            socket: "/var/lib/faber/control.sock".into(),
        });
        assert!(msg.contains("/var/lib/faber/control.sock"), "{msg}");
        assert!(msg.contains("operator's account or root"), "{msg}");
        assert!(msg.contains("_faber") && msg.contains("login"), "{msg}");
    }

    #[test]
    fn no_daemon_names_the_socket_and_the_port() {
        let msg = client_error_message(&ClientError::NoDaemon {
            socket: "/run/faber.sock".into(),
            fallback: Fallback::Tcp(9000),
        });
        assert!(
            msg.contains("/run/faber.sock") && msg.contains("127.0.0.1:9000"),
            "{msg}"
        );
        assert!(msg.contains("--port"), "{msg}");
    }

    #[test]
    fn no_daemon_without_a_fallback_names_only_the_socket() {
        let msg = client_error_message(&ClientError::NoDaemon {
            socket: "/run/faber.sock".into(),
            fallback: Fallback::Never,
        });
        assert!(msg.contains("/run/faber.sock"), "{msg}");
        assert!(
            !msg.contains("127.0.0.1") && !msg.contains("--port"),
            "{msg}"
        );
    }
}
