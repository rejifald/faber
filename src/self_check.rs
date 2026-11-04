//! The boot self-check (#92, under ADR-0031): before boot probation ends, the
//! binary that an update just installed proves its own updater still works by
//! re-verifying the release it came from, end to end.
//!
//! Probation was designed for crash loops (ADR-0006). A binary that runs happily
//! but can no longer update is invisible to it, and every host would be stranded
//! on it, because the only way out is that same broken updater. v0.2.0 did exactly
//! that: its asset download sent no `User-Agent` and got a 403.
//!
//! So, with the very code a real update runs:
//!
//! 1. fetch the release for this binary's own tag with **the updater's own
//!    client** (the one `main` hands the `Updater`): whatever credential, or none,
//!    a real update uses is what is proven. ADR-0031's updater carries none, and
//!    this follows it with no change;
//! 2. download `SHA256SUMS` and `SHA256SUMS.sigstore.json`;
//! 3. run the real provenance verifier over them
//!    ([`crate::update::verify_provenance_at`]);
//! 4. check the digest of this process's own executable against `SHA256SUMS`.
//!
//! The outcome has three shapes, and the line between the first two is the whole
//! design:
//!
//! - **Refused** is deliberately narrow. Only three things count: the provenance
//!   verifier refused on the bytes and bundle (including a release that has no
//!   bundle asset, but not a refusal that rests on the tag's state in GitHub, see
//!   [`crate::update::verify_provenance_detail`]), this
//!   binary's digest differs from the one `SHA256SUMS` lists, or `SHA256SUMS` has
//!   no entry for this binary, all on a release that was fetched successfully.
//!   Those are answers about the bytes. Probation fails, and the existing rollback
//!   path runs
//!   ([`crate::update::force_rollback`] raises the marker to the crash-loop
//!   threshold; the next boot restores, quarantines and leaves the breadcrumb).
//! - **Unavailable** is everything else: **every HTTP status error (401, 403,
//!   404, 429, 5xx)**, network, Sigstore TUF. A missing release is not evidence
//!   against the binary: ADR-0031's spike showed a workflow token can delete a
//!   published release, so a deleted or yanked one must never roll a host back.
//!   Never a rollback. The marker stays and the check is
//!   retried with backoff for [`WINDOW`].
//! - **Unproven**: the window ended still Unavailable. The release is kept and the
//!   marker is cleared (a marker that outlives the window would roll the host back
//!   on two unrelated restarts days later), but never silently: `/api/state`
//!   reports `self_check.status = "unproven"` with the last reason, at every boot,
//!   until an update proves itself.
//!
//! A hand-installed binary has no marker and skips all of this, as it always has.

use std::future::Future;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{error, info, warn};

use crate::audit::now_ms;
use crate::github::{Github, GithubError};
use crate::provenance::Verdict;
use crate::update::{
    clear_marker, force_rollback, parse_sha256sums, parse_version, write_atomic, UpdateHandle,
    UpdatePaths, DAEMON_ASSET_NAME, SUMS_ASSET_NAME, SUMS_BUNDLE_ASSET_NAME,
};

/// How long an Unavailable check keeps being retried before the release is
/// reported as unproven. Two hours rides out a GitHub or Sigstore incident of the
/// usual length, while bounding how long a restart-sensitive marker lingers.
pub const WINDOW: Duration = Duration::from_secs(2 * 60 * 60);

/// First retry delay; doubles each time up to [`MAX_BACKOFF`].
const FIRST_BACKOFF: Duration = Duration::from_secs(2 * 60);

/// Backoff ceiling. One attempt costs about five API requests. Attempts land at
/// 0, 2, 6, 14, 30 and 50 minutes, so the first hour costs six attempts, about 30
/// requests, under the 60 an hour an unauthenticated address gets and leaving
/// room for everything else on it.
const MAX_BACKOFF: Duration = Duration::from_secs(20 * 60);

/// Where the self-check stands, for `/api/state`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SelfCheckStatus {
    /// Unavailable so far; still retrying inside the window.
    Pending,
    /// The release verified end to end under this binary's own updater.
    Verified,
    /// The window ended still Unavailable. The release is kept, the updater is
    /// not proven.
    Unproven,
    /// A definite refusal; this version is being rolled back.
    Refused,
}

/// The self-check as `/api/state` reports it, and as it is persisted to
/// `update/self_check.json`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SelfCheckView {
    /// The version that ran the check.
    pub version: String,
    pub status: SelfCheckStatus,
    /// The last reason, for everything but `verified`.
    #[serde(default)]
    pub detail: Option<String>,
    /// Checks run so far.
    #[serde(default)]
    pub attempts: u32,
    pub at_ms: u64,
}

/// The saved result for `current_version`, or `None` when there is none or it is
/// about another version (a hand install replaced the binary since).
pub(crate) fn read_record(paths: &UpdatePaths, current_version: &str) -> Option<SelfCheckView> {
    let text = std::fs::read_to_string(paths.self_check()).ok()?;
    let view: SelfCheckView = serde_json::from_str(&text).ok()?;
    let same = parse_version(&view.version)? == parse_version(current_version)?;
    // A `pending` record is a snapshot from a process that is gone; its retry
    // loop restarts on this boot if the marker is still there, and says so then.
    (same && view.status != SelfCheckStatus::Pending).then_some(view)
}

/// Written by temp file and rename: a torn record does not parse, and
/// [`read_record`] reads that as no result, which would drop an `unproven`
/// verdict the dashboard must keep showing.
fn write_record(paths: &UpdatePaths, view: &SelfCheckView) {
    let path = paths.self_check();
    match serde_json::to_string_pretty(view) {
        Ok(text) => {
            if let Err(e) = write_atomic(&path, text.as_bytes()) {
                warn!("self-check: could not save {}: {e}", path.display());
            }
        }
        Err(e) => warn!("self-check: could not encode its result: {e}"),
    }
}

/// One attempt's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    Verified,
    Refused(String),
    Unavailable(String),
}

/// A failed GitHub request as an outcome. Always unavailability, whatever the
/// status: a 404 is a release someone deleted or yanked (a workflow token can
/// delete one, ADR-0031), a 401/403 is a credential or rate limit, a 5xx an
/// outage. None of them is evidence about this binary's bytes, so none of them
/// rolls a host back; a persistent one ends as Unproven, loudly.
fn unavailable(what: &str, e: &GithubError) -> Outcome {
    Outcome::Unavailable(format!("{what}: {e}"))
}

/// One end-to-end check. `verify` is the provenance step, given the downloaded
/// `SHA256SUMS` and bundle bytes: production passes the real verifier, and the
/// tests pass a stand-in so they need neither Sigstore nor a signed fixture of
/// their own binary. Everything else is the update path's own code.
pub(crate) async fn check_once<F, Fut>(
    gh: &Github,
    repo: &str,
    tag: &str,
    own_digest: &str,
    verify: F,
) -> Outcome
where
    F: FnOnce(Vec<u8>, Option<Vec<u8>>) -> Fut,
    Fut: Future<Output = (Verdict, bool)>,
{
    let release = match gh.release_by_tag(repo, tag).await {
        Ok(Some(r)) => r,
        Ok(None) => {
            return Outcome::Unavailable(format!(
                "release {tag} does not exist in {repo} (404); it may have been deleted"
            ))
        }
        Err(e) => return unavailable(&format!("fetching release {tag}"), &e),
    };
    let Some(sums_asset) = crate::update::find_asset(&release, SUMS_ASSET_NAME) else {
        return Outcome::Unavailable(format!("release {tag} has no {SUMS_ASSET_NAME} asset"));
    };
    let sums = match gh.download_release_asset(repo, sums_asset.id).await {
        Ok(b) => b,
        Err(e) => return unavailable(&format!("downloading {SUMS_ASSET_NAME}"), &e),
    };
    let bundle = match crate::update::find_asset(&release, SUMS_BUNDLE_ASSET_NAME) {
        Some(a) => match gh.download_release_asset(repo, a.id).await {
            Ok(b) => Some(b),
            Err(e) => return unavailable(&format!("downloading {SUMS_BUNDLE_ASSET_NAME}"), &e),
        },
        None => None,
    };

    let parsed = parse_sha256sums(&String::from_utf8_lossy(&sums));
    match verify(sums, bundle).await {
        (Verdict::Verified, _) => {}
        // A refusal that rests on GitHub's tag state (tag deleted, a transient
        // 404 on the ref, a moved tag) is not about this binary's bytes.
        (Verdict::Refused(r), true) => {
            return Outcome::Unavailable(format!("provenance (depends on the tag's state): {r}"))
        }
        (Verdict::Refused(r), false) => return Outcome::Refused(format!("provenance: {r}")),
        (Verdict::Unavailable(r), _) => return Outcome::Unavailable(format!("provenance: {r}")),
    }

    match parsed.get(DAEMON_ASSET_NAME) {
        None => Outcome::Refused(format!(
            "{SUMS_ASSET_NAME} of {tag} has no entry for {DAEMON_ASSET_NAME}"
        )),
        Some(expected) if expected.eq_ignore_ascii_case(own_digest) => Outcome::Verified,
        Some(expected) => Outcome::Refused(format!(
            "this binary's sha256 is {own_digest} but {SUMS_ASSET_NAME} of {tag} lists \
             {expected} for {DAEMON_ASSET_NAME}"
        )),
    }
}

/// The production provenance step: the real verifier, plus whether a refusal
/// rests on GitHub's tag state (see [`crate::update::verify_provenance_detail`]).
pub(crate) async fn real_verify(
    gh: &Github,
    repo: &str,
    tuf_cache: &std::path::Path,
    tag: &str,
    sums: Vec<u8>,
    bundle: Option<Vec<u8>>,
) -> (Verdict, bool) {
    let (verdict, _, tag_state) =
        crate::update::verify_provenance_detail(gh, repo, tuf_cache, &sums, bundle.as_deref(), tag)
            .await;
    (verdict, tag_state)
}

/// What the retry loop settled on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settled {
    Verified,
    /// Roll back. The string is the refusal.
    Refused(String),
    /// The window ended Unavailable. The string is the last reason.
    Unproven(String),
}

/// Delay before attempt `n + 1` (`n` attempts made): 30s, 1m, 2m, ... up to 10m.
fn backoff(n: u32) -> Duration {
    FIRST_BACKOFF
        .saturating_mul(1u32 << n.saturating_sub(1).min(10))
        .min(MAX_BACKOFF)
}

/// Run `check` until it is definite or `window` is spent. `pending` hears about
/// each Unavailable attempt (attempts so far, reason) so the dashboard can show
/// a retry in progress. Sleeps on tokio's clock, so a test can pause it.
pub(crate) async fn settle<C, CF, P>(mut check: C, window: Duration, mut pending: P) -> Settled
where
    C: FnMut() -> CF,
    CF: Future<Output = Outcome>,
    P: FnMut(u32, &str),
{
    let start = tokio::time::Instant::now();
    let mut attempts = 0u32;
    loop {
        attempts += 1;
        let reason = match check().await {
            Outcome::Verified => return Settled::Verified,
            Outcome::Refused(r) => return Settled::Refused(r),
            Outcome::Unavailable(r) => r,
        };
        let wait = backoff(attempts);
        if start.elapsed() + wait > window {
            return Settled::Unproven(reason);
        }
        pending(attempts, &reason);
        tokio::time::sleep(wait).await;
    }
}

/// The digest of this process's own executable, which is the file that was
/// installed from the release asset.
async fn own_digest() -> Result<String, String> {
    let exe = std::env::current_exe().map_err(|e| format!("locating this executable: {e}"))?;
    let bytes = tokio::fs::read(&exe)
        .await
        .map_err(|e| format!("reading {}: {e}", exe.display()))?;
    Ok(crate::update::sha256_hex(&bytes))
}

/// Run the self-check for the update that installed this binary and act on the
/// result: record it for `/api/state`, and either clear the marker (verified, or
/// unproven after the window) or arm the rollback (refused). Returns the result;
/// on `Refused` the caller must exit so the supervisor restarts into the
/// restored binary. Restarting is cheap: running containers are adopted across
/// it (ADR-0011).
pub async fn prove_updater(
    paths: &UpdatePaths,
    handle: &UpdateHandle,
    gh: &Github,
    repo: &str,
    version: &str,
) -> Settled {
    let Some((major, minor, patch)) = parse_version(version) else {
        return conclude(
            paths,
            handle,
            version,
            0,
            Settled::Unproven(format!(
                "{version} is not a release version; nothing to verify"
            )),
        );
    };
    let tag = format!("v{major}.{minor}.{patch}");
    let tuf_cache = paths.tuf_cache();

    let attempts = std::sync::atomic::AtomicU32::new(0);
    let settled = settle(
        || {
            attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let (tuf_cache, tag) = (&tuf_cache, &tag);
            async move {
                let digest = match own_digest().await {
                    Ok(d) => d,
                    Err(e) => return Outcome::Unavailable(e),
                };
                check_once(gh, repo, tag, &digest, |sums, bundle| {
                    real_verify(gh, repo, tuf_cache, tag, sums, bundle)
                })
                .await
            }
        },
        WINDOW,
        |n, reason| {
            warn!(
                "self-check of {tag} unavailable (attempt {n}), retrying; the update stays \
                 on probation and is not rolled back: {reason}"
            );
            handle.set_self_check(SelfCheckView {
                version: version.to_string(),
                status: SelfCheckStatus::Pending,
                detail: Some(reason.to_string()),
                attempts: n,
                at_ms: now_ms(),
            });
        },
    )
    .await;
    conclude(
        paths,
        handle,
        version,
        attempts.load(std::sync::atomic::Ordering::SeqCst),
        settled,
    )
}

/// Record, publish and act on a settled result.
fn conclude(
    paths: &UpdatePaths,
    handle: &UpdateHandle,
    version: &str,
    attempts: u32,
    settled: Settled,
) -> Settled {
    let (status, detail) = match &settled {
        Settled::Verified => (SelfCheckStatus::Verified, None),
        Settled::Refused(r) => (SelfCheckStatus::Refused, Some(r.clone())),
        Settled::Unproven(r) => (SelfCheckStatus::Unproven, Some(r.clone())),
    };
    let view = SelfCheckView {
        version: version.to_string(),
        status,
        detail,
        attempts,
        at_ms: now_ms(),
    };
    write_record(paths, &view);
    handle.set_self_check(view);
    match &settled {
        Settled::Verified => {
            info!("self-check: {version} re-verified the release it was installed from");
            clear_marker(paths);
        }
        Settled::Unproven(reason) => {
            error!(
                "self-check: {version} could not prove its updater within {}s and is kept, \
                 UNPROVEN: {reason}",
                WINDOW.as_secs()
            );
            clear_marker(paths);
        }
        Settled::Refused(reason) => {
            error!(
                "self-check: {version} failed to verify its own release, rolling back: {reason}"
            );
            if !force_rollback(paths) {
                warn!("self-check: no boot marker to roll back from; staying on {version}");
            }
        }
    }
    settled
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    const TAG: &str = "v0.4.0";
    const OWN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    /// A GitHub API stand-in: `(path-suffix, status, body)` rules, first match
    /// wins, 404 otherwise.
    async fn github(rules: Vec<(&'static str, u16, Vec<u8>)>) -> Github {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = Vec::new();
                let mut chunk = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    match sock.read(&mut chunk).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => buf.extend_from_slice(&chunk[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&buf).to_string();
                let path = head
                    .lines()
                    .next()
                    .and_then(|l| l.split_whitespace().nth(1))
                    .unwrap_or("")
                    .to_string();
                let (status, body) = rules
                    .iter()
                    .find(|(suffix, _, _)| path.ends_with(suffix))
                    .map(|(_, s, b)| (*s, b.clone()))
                    .unwrap_or((404, b"{}".to_vec()));
                let mut resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                resp.extend_from_slice(&body);
                let _ = sock.write_all(&resp).await;
                let _ = sock.flush().await;
            }
        });
        Github::at(base)
    }

    fn release_json(with_bundle: bool) -> Vec<u8> {
        let bundle = if with_bundle {
            r#",{"id":12,"name":"SHA256SUMS.sigstore.json","size":2}"#
        } else {
            ""
        };
        format!(
            r#"{{"tag_name":"{TAG}","assets":[{{"id":11,"name":"SHA256SUMS","size":2}}{bundle}]}}"#
        )
        .into_bytes()
    }

    fn sums_for(digest: &str) -> Vec<u8> {
        format!("{digest}  {DAEMON_ASSET_NAME}\n").into_bytes()
    }

    /// A healthy release: JSON, sums listing `digest`, a bundle.
    async fn healthy(digest: &str) -> Github {
        github(vec![
            ("/releases/tags/v0.4.0", 200, release_json(true)),
            ("/releases/assets/11", 200, sums_for(digest)),
            ("/releases/assets/12", 200, b"{}".to_vec()),
        ])
        .await
    }

    async fn run(gh: &Github, verdict: Verdict) -> Outcome {
        check_once(gh, "owner/demo", TAG, OWN, |_, _| async move {
            (verdict, false)
        })
        .await
    }

    #[tokio::test]
    async fn a_verified_release_whose_digest_matches_verifies() {
        let gh = healthy(OWN).await;
        assert_eq!(run(&gh, Verdict::Verified).await, Outcome::Verified);
    }

    #[tokio::test]
    async fn the_verifier_receives_the_downloaded_sums_and_bundle() {
        let gh = healthy(OWN).await;
        let seen = Arc::new(Mutex::new(None));
        let sink = seen.clone();
        let out = check_once(&gh, "owner/demo", TAG, OWN, |sums, bundle| async move {
            *sink.lock().unwrap() = Some((sums, bundle));
            (Verdict::Verified, false)
        })
        .await;
        assert_eq!(out, Outcome::Verified);
        let (sums, bundle) = seen.lock().unwrap().clone().unwrap();
        assert_eq!(sums, sums_for(OWN));
        assert_eq!(bundle, Some(b"{}".to_vec()));
    }

    #[tokio::test]
    async fn a_verifier_refusal_is_refused() {
        let gh = healthy(OWN).await;
        let out = run(&gh, Verdict::Refused("signer workflow".into())).await;
        let Outcome::Refused(r) = out else {
            panic!("{out:?}")
        };
        assert!(r.contains("signer workflow"), "{r}");
    }

    #[tokio::test]
    async fn a_digest_mismatch_is_refused() {
        let other = "b".repeat(64);
        let gh = healthy(&other).await;
        let out = run(&gh, Verdict::Verified).await;
        let Outcome::Refused(r) = out else {
            panic!("{out:?}")
        };
        assert!(r.contains(&other) && r.contains(OWN), "{r}");
    }

    #[tokio::test]
    async fn a_sums_file_without_this_binarys_entry_is_refused() {
        let gh = github(vec![
            ("/releases/tags/v0.4.0", 200, release_json(true)),
            ("/releases/assets/11", 200, b"".to_vec()),
            ("/releases/assets/12", 200, b"{}".to_vec()),
        ])
        .await;
        assert!(matches!(
            run(&gh, Verdict::Verified).await,
            Outcome::Refused(_)
        ));
    }

    /// A deleted or yanked release (a workflow token can delete one, ADR-0031)
    /// is not evidence against this binary.
    #[tokio::test]
    async fn a_missing_release_is_unavailable_not_refused() {
        let gh = github(vec![("/releases/tags/v0.4.0", 404, b"{}".to_vec())]).await;
        assert!(matches!(
            run(&gh, Verdict::Verified).await,
            Outcome::Unavailable(_)
        ));
    }

    #[tokio::test]
    async fn a_release_without_sums_is_unavailable() {
        let body = format!(r#"{{"tag_name":"{TAG}","assets":[]}}"#).into_bytes();
        let gh = github(vec![("/releases/tags/v0.4.0", 200, body)]).await;
        assert!(matches!(
            run(&gh, Verdict::Verified).await,
            Outcome::Unavailable(_)
        ));
    }

    #[tokio::test]
    async fn a_release_without_a_bundle_reaches_the_verifier_as_none() {
        let gh = github(vec![
            ("/releases/tags/v0.4.0", 200, release_json(false)),
            ("/releases/assets/11", 200, sums_for(OWN)),
        ])
        .await;
        let got = Arc::new(Mutex::new(Some(Some(vec![]))));
        let sink = got.clone();
        let _ = check_once(&gh, "owner/demo", TAG, OWN, |_, bundle| async move {
            *sink.lock().unwrap() = Some(bundle);
            (Verdict::Refused("release has no bundle".into()), false)
        })
        .await;
        assert_eq!(*got.lock().unwrap(), Some(None));
    }

    #[tokio::test]
    async fn a_verifier_unavailable_is_unavailable() {
        let gh = healthy(OWN).await;
        let out = run(&gh, Verdict::Unavailable("TUF down".into())).await;
        let Outcome::Unavailable(r) = out else {
            panic!("{out:?}")
        };
        assert!(r.contains("TUF down"), "{r}");
    }

    #[tokio::test]
    async fn every_http_status_error_is_unavailable_never_refused() {
        for status in [401u16, 403, 404, 429, 500, 502, 503] {
            for body in ["{}", r#"{"message":"API rate limit exceeded"}"#] {
                let gh = github(vec![(
                    "/releases/tags/v0.4.0",
                    status,
                    body.as_bytes().to_vec(),
                )])
                .await;
                let out = run(&gh, Verdict::Verified).await;
                assert!(
                    matches!(out, Outcome::Unavailable(_)),
                    "{status} {body}: {out:?}"
                );
            }
        }
    }

    /// A status error on an asset download, after the release itself was fine.
    #[tokio::test]
    async fn an_asset_download_error_is_unavailable() {
        let gh = github(vec![
            ("/releases/tags/v0.4.0", 200, release_json(true)),
            ("/releases/assets/11", 404, b"{}".to_vec()),
        ])
        .await;
        assert!(matches!(
            run(&gh, Verdict::Verified).await,
            Outcome::Unavailable(_)
        ));
    }

    #[tokio::test]
    async fn an_unreachable_api_is_unavailable() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        drop(l);
        let gh = Github::at(format!("http://{addr}"));
        assert!(matches!(
            run(&gh, Verdict::Verified).await,
            Outcome::Unavailable(_)
        ));
    }

    /// The REAL verifier, end to end through `check_once`, with the tag lookup
    /// answering 404: a deleted tag (or a transient 404 on the ref) must be
    /// Unavailable, never a rollback.
    #[tokio::test]
    async fn the_real_verifier_with_a_missing_tag_is_unavailable() {
        let gh = github(vec![
            ("/releases/tags/v0.4.0", 200, release_json(true)),
            ("/releases/assets/11", 200, sums_for(OWN)),
            ("/releases/assets/12", 200, b"{}".to_vec()),
            (
                "/git/ref/tags/v0.4.0",
                404,
                br#"{"message":"Not Found"}"#.to_vec(),
            ),
        ])
        .await;
        let dir = std::env::temp_dir().join(format!("faber-sc-tuf-{}", std::process::id()));
        let out = check_once(&gh, "owner/demo", TAG, OWN, |sums, bundle| {
            real_verify(&gh, "owner/demo", &dir, TAG, sums, bundle)
        })
        .await;
        let Outcome::Unavailable(r) = out else {
            panic!("expected Unavailable, got {out:?}");
        };
        assert!(r.contains("does not exist"), "{r}");
    }

    /// A refusal not about tag state still refuses: no bundle asset.
    #[tokio::test]
    async fn the_real_verifier_with_no_bundle_still_refuses() {
        let gh = github(vec![
            ("/releases/tags/v0.4.0", 200, release_json(false)),
            ("/releases/assets/11", 200, sums_for(OWN)),
        ])
        .await;
        let dir = std::env::temp_dir().join(format!("faber-sc-tuf2-{}", std::process::id()));
        let out = check_once(&gh, "owner/demo", TAG, OWN, |sums, bundle| {
            real_verify(&gh, "owner/demo", &dir, TAG, sums, bundle)
        })
        .await;
        assert!(matches!(out, Outcome::Refused(_)), "{out:?}");
    }

    /// The self-check goes through the client it is handed (the one the Updater
    /// holds), credential included, rather than building its own.
    #[tokio::test]
    async fn the_self_check_uses_the_clients_credential() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = sock.read(&mut buf).await.unwrap();
            let _ = sock
                .write_all(b"HTTP/1.1 404 X\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}")
                .await;
            String::from_utf8_lossy(&buf[..n]).to_ascii_lowercase()
        });
        let gh = Github::at(format!("http://{addr}"));
        let _ = run(&gh, Verdict::Verified).await;
        let head = seen.await.unwrap();
        assert!(head.contains("authorization: bearer test-token"), "{head}");
    }

    // -- the retry loop ------------------------------------------------------

    fn scripted(
        outcomes: Vec<Outcome>,
    ) -> (impl FnMut() -> std::future::Ready<Outcome>, Arc<AtomicU32>) {
        let calls = Arc::new(AtomicU32::new(0));
        let c = calls.clone();
        let mut it = outcomes.into_iter();
        (
            move || {
                c.fetch_add(1, Ordering::SeqCst);
                std::future::ready(
                    it.next()
                        .unwrap_or(Outcome::Unavailable("still down".into())),
                )
            },
            calls,
        )
    }

    #[test]
    fn backoff_doubles_to_its_ceiling() {
        let secs: Vec<u64> = (1..=7).map(|n| backoff(n).as_secs()).collect();
        assert_eq!(secs, [120, 240, 480, 960, 1200, 1200, 1200]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_refusal_settles_at_once_without_retrying() {
        let (check, calls) = scripted(vec![Outcome::Refused("no".into())]);
        let s = settle(check, WINDOW, |_, _| {}).await;
        assert_eq!(s, Settled::Refused("no".into()));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn unavailable_is_retried_until_it_verifies() {
        let (check, calls) = scripted(vec![
            Outcome::Unavailable("a".into()),
            Outcome::Unavailable("b".into()),
            Outcome::Verified,
        ]);
        let mut heard = Vec::new();
        let s = settle(check, WINDOW, |n, r| heard.push((n, r.to_string()))).await;
        assert_eq!(s, Settled::Verified);
        assert_eq!(calls.load(Ordering::SeqCst), 3);
        assert_eq!(heard, [(1, "a".to_string()), (2, "b".to_string())]);
    }

    #[tokio::test(start_paused = true)]
    async fn unavailable_for_the_whole_window_ends_unproven_and_stops_retrying() {
        let (check, calls) = scripted(vec![]);
        let s = settle(check, WINDOW, |_, _| {}).await;
        assert_eq!(s, Settled::Unproven("still down".into()));
        let n = calls.load(Ordering::SeqCst);
        // 2, 4, 8, 16 then 20-minute steps: bounded, not endless.
        assert!((6..=12).contains(&n), "{n} attempts");
    }

    #[tokio::test(start_paused = true)]
    async fn a_refusal_after_an_outage_still_rolls_back() {
        let (check, _) = scripted(vec![
            Outcome::Unavailable("x".into()),
            Outcome::Refused("y".into()),
        ]);
        assert_eq!(
            settle(check, WINDOW, |_, _| {}).await,
            Settled::Refused("y".into())
        );
    }

    // -- acting on the result ------------------------------------------------

    fn temp_paths(tag: &str) -> UpdatePaths {
        let dir = std::env::temp_dir().join(format!(
            "faber-selfcheck-{tag}-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        UpdatePaths::with_root(dir)
    }

    fn handle_for(paths: &UpdatePaths, version: &str) -> Arc<UpdateHandle> {
        crate::update::test_handle(paths.clone(), version)
    }

    fn arm_marker(paths: &UpdatePaths, version: &str) {
        std::fs::create_dir_all(paths.root.join("bin")).unwrap();
        std::fs::create_dir_all(paths.root.join("update")).unwrap();
        std::fs::write(paths.bin(), b"new").unwrap();
        std::fs::write(paths.prev_bin(), b"old").unwrap();
        std::fs::write(
            paths.marker(),
            format!(r#"{{"from":"0.3.0","to":"{version}","attempts":1,"written_ms":1}}"#),
        )
        .unwrap();
    }

    #[test]
    fn verified_clears_the_marker_and_records_it() {
        let paths = temp_paths("verified");
        let handle = handle_for(&paths, "0.4.0");
        arm_marker(&paths, "0.4.0");
        let s = conclude(&paths, &handle, "0.4.0", 1, Settled::Verified);
        assert_eq!(s, Settled::Verified);
        assert!(!paths.marker().exists());
        let v = handle.view().self_check.expect("reported");
        assert_eq!(v.status, SelfCheckStatus::Verified);
        assert_eq!(
            read_record(&paths, "0.4.0+gabc").unwrap().status,
            SelfCheckStatus::Verified
        );
        assert!(
            read_record(&paths, "0.5.0").is_none(),
            "another version's record is not ours"
        );
    }

    #[test]
    fn refused_arms_the_existing_rollback_and_the_next_boot_restores() {
        let paths = temp_paths("refused");
        let handle = handle_for(&paths, "0.4.0");
        arm_marker(&paths, "0.4.0");
        let cfg = paths.root.join("config.toml");
        conclude(
            &paths,
            &handle,
            "0.4.0",
            1,
            Settled::Refused("digest".into()),
        );
        assert!(
            paths.marker().exists(),
            "marker stays for the next boot to act on"
        );
        assert_eq!(
            handle.view().self_check.unwrap().status,
            SelfCheckStatus::Refused
        );

        let report = crate::update::rollback_if_needed(&paths, "0.4.0+gabc", &cfg).expect("report");
        assert!(report.restored, "{report:?}");
        assert_eq!(std::fs::read(paths.bin()).unwrap(), b"old");
        assert!(!paths.marker().exists());
        assert!(
            std::fs::read_to_string(paths.quarantine())
                .unwrap()
                .contains("0.4.0"),
            "the refused version is quarantined"
        );
    }

    #[test]
    fn unavailable_for_the_window_keeps_the_release_and_says_so_loudly() {
        let paths = temp_paths("unproven");
        let handle = handle_for(&paths, "0.4.0");
        arm_marker(&paths, "0.4.0");
        conclude(
            &paths,
            &handle,
            "0.4.0",
            14,
            Settled::Unproven("TUF down".into()),
        );
        assert_eq!(std::fs::read(paths.bin()).unwrap(), b"new", "no rollback");
        assert!(!paths.prev_bin().with_extension("gone").exists());
        let v = handle.view().self_check.unwrap();
        assert_eq!(v.status, SelfCheckStatus::Unproven);
        assert_eq!(v.detail.as_deref(), Some("TUF down"));
        // ...and a restart still reports it, rather than the state vanishing.
        let again = crate::update::test_handle(paths.clone(), "0.4.0+gdef");
        assert_eq!(
            again.view().self_check.unwrap().status,
            SelfCheckStatus::Unproven
        );
    }

    #[test]
    fn a_pending_check_leaves_the_marker_and_the_binary_alone() {
        let paths = temp_paths("pending");
        let handle = handle_for(&paths, "0.4.0");
        arm_marker(&paths, "0.4.0");
        handle.set_self_check(SelfCheckView {
            version: "0.4.0".into(),
            status: SelfCheckStatus::Pending,
            detail: Some("TUF down".into()),
            attempts: 1,
            at_ms: 1,
        });
        assert!(paths.marker().exists());
        assert_eq!(std::fs::read(paths.bin()).unwrap(), b"new");
        assert_eq!(
            handle.view().self_check.unwrap().status,
            SelfCheckStatus::Pending
        );
    }

    /// A torn record would read as no result, so the record is replaced by
    /// rename, never rewritten in place.
    #[test]
    fn the_record_is_replaced_by_rename() {
        use std::os::unix::fs::MetadataExt;
        let paths = temp_paths("record-rename");
        let handle = handle_for(&paths, "0.4.0");
        conclude(
            &paths,
            &handle,
            "0.4.0",
            14,
            Settled::Unproven("TUF down".into()),
        );
        let before = std::fs::metadata(paths.self_check()).unwrap().ino();

        conclude(&paths, &handle, "0.4.0", 15, Settled::Verified);

        let after = std::fs::metadata(paths.self_check()).unwrap().ino();
        assert_ne!(after, before, "rewritten in place");
        assert_eq!(
            read_record(&paths, "0.4.0").unwrap().status,
            SelfCheckStatus::Verified
        );
    }

    #[test]
    fn a_hand_installed_binary_with_no_marker_has_nothing_to_roll_back() {
        let paths = temp_paths("handinstall");
        assert!(!force_rollback(&paths));
        assert!(!paths.self_check().exists());
    }
}
