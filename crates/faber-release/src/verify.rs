//! The verification itself: fetching Sigstore's trusted root through TUF, and
//! [`check`], which compares a release's attestation against a [`Policy`].

use std::path::Path;
use std::time::Duration;

use sigstore_trust_root::{TrustedRoot, TufBootstrap, TufConfig};
use sigstore_types::{Bundle, SignatureContent};
use sigstore_verify::{Error as VerifyError, IdentityMatcher, VerificationPolicy, Verifier};

use crate::policy::{Policy, Verdict, SOURCE_COMMIT_FIELD};
use crate::sums::sha256_hex;

/// Sigstore's production TUF root, compiled in: the anchor every trusted-root
/// refresh is verified against. Byte-identical to the one `sigstore-trust-root`
/// 0.14.0 ships (`repository/tuf_root.json`); a copy here so the anchor is
/// reviewed in this repository, not inherited from a dependency bump.
const SIGSTORE_TUF_ROOT: &[u8] = include_bytes!("sigstore_tuf_root.json");
const SIGSTORE_TUF_URL: &str = "https://tuf-repo-cdn.sigstore.dev";

/// Bounds on every TUF request. Without them a malicious or wedged mirror could
/// hang a refresh indefinitely (the slow-retrieval attack), and with it the
/// update loop.
const TUF_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TUF_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Fetch Sigstore's trusted root through TUF, online, caching verified metadata
/// in `cache_dir`. An `Err` is the reason it could not be had, and means
/// [`Verdict::Unavailable`]: there is no offline or embedded fallback.
///
/// `user_agent` is what the TUF requests identify as; the caller owns it because
/// this crate has no version of its own to speak for.
pub async fn fetch_trusted_root(cache_dir: &Path, user_agent: &str) -> Result<TrustedRoot, String> {
    fetch_trusted_root_from(SIGSTORE_TUF_URL, cache_dir, user_agent).await
}

/// Create `dir` (and any missing parents) owner-only, and tighten it to `0700`
/// if it already exists. The cache is re-verified on every refresh, so this is
/// not what makes it trustworthy; it keeps other local users from reading or
/// planting files in faber's state.
pub fn ensure_private_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
}

/// [`fetch_trusted_root`] against an explicit TUF repository, so a test can aim
/// it at an unreachable one.
///
/// Public so the daemon's own tests can aim the update seam at an unreachable
/// repository; the shipping callers use [`fetch_trusted_root`], and the URL is
/// never a runtime knob.
pub async fn fetch_trusted_root_from(
    url: &str,
    cache_dir: &Path,
    user_agent: &str,
) -> Result<TrustedRoot, String> {
    ensure_private_dir(cache_dir)
        .map_err(|e| format!("creating the TUF cache {}: {e}", cache_dir.display()))?;
    let client = sigstore_trust_root::reqwest::Client::builder()
        .connect_timeout(TUF_CONNECT_TIMEOUT)
        .timeout(TUF_REQUEST_TIMEOUT)
        .user_agent(user_agent)
        .build()
        .map_err(|e| format!("building the TUF client: {e}"))?;
    let config = TufConfig::custom(url, TufBootstrap::trusted(SIGSTORE_TUF_ROOT))
        .with_cache_dir(cache_dir.to_path_buf())
        .with_http_client(client);
    TrustedRoot::from_tuf(config).await.map_err(|e| {
        format!("Sigstore's trusted root could not be refreshed through TUF ({url}): {e}")
    })
}

/// Verify `bundle_json` (the release's `SHA256SUMS.sigstore.json`) over
/// `sums` (its `SHA256SUMS`) under `policy`, where `tag_commit` is the commit the
/// release's tag points to. Pure: no network, no clock beyond what the crate
/// reads from the bundle, so it is tested on saved fixtures.
pub fn check(
    policy: &Policy,
    root: &TrustedRoot,
    sums: &[u8],
    bundle_json: &[u8],
    tag_commit: &str,
) -> Verdict {
    // Before anything else, so a binary built with the placeholder says so
    // plainly instead of reporting whichever mismatch the crate hits first.
    if policy.repository_id == 0 {
        return Verdict::Refused(
            "source repository ID (OID 1.3.6.1.4.1.57264.1.15): this binary's policy carries the \
             placeholder 0, which matches no repository; it is set when the release repository \
             exists"
                .to_string(),
        );
    }

    let bundle = match std::str::from_utf8(bundle_json)
        .map_err(|e| e.to_string())
        .and_then(|s| Bundle::from_json(s).map_err(|e| e.to_string()))
    {
        Ok(b) => b,
        Err(e) => return Verdict::Refused(format!("bundle: not a Sigstore bundle: {e}")),
    };

    let verifier = match Verifier::new(root) {
        Ok(v) => v,
        // The root came through TUF verified; one the crate cannot use is this
        // host's problem, not the release's.
        Err(e) => return Verdict::Unavailable(format!("Sigstore's trusted root is unusable: {e}")),
    };

    // Chain, SCT, transparency log, DSSE signature and subject binding, plus the
    // SAN identity and issuer, all inside the crate.
    let crate_policy = VerificationPolicy::new(
        IdentityMatcher::Uri(policy.signer_workflow.to_string()),
        policy.issuer,
    );
    let result = match verifier.verify(sums, &bundle, &crate_policy) {
        Ok(r) => r,
        Err(VerifyError::IdentityMismatch { expected, actual }) => {
            return Verdict::Refused(format!(
                "signer workflow (certificate SAN): expected {expected}, certificate has {}",
                actual.map_or_else(|| "none".to_string(), |a| a.as_str().to_string())
            ))
        }
        Err(VerifyError::IssuerMismatch { expected, actual }) => {
            return Verdict::Refused(format!(
                "issuer (OID 1.3.6.1.4.1.57264.1.8): expected {expected}, certificate has {}",
                actual.as_deref().unwrap_or("none")
            ))
        }
        Err(e) => {
            return Verdict::Refused(format!(
                "signature (certificate chain, transparency log, DSSE signature or SHA256SUMS digest): {e}"
            ))
        }
    };
    // Every stage must have run; a policy the crate skipped a stage under is
    // not one this module asked for, but the result says so either way.
    let stages = [
        ("certificate chain", result.certificate_verified()),
        ("certificate transparency (SCT)", result.sct_verified()),
        ("transparency log inclusion", result.tlog_verified()),
        ("identity policy", result.identity_policy_checked()),
    ];
    if let Some((stage, _)) = stages.iter().find(|(_, ran)| !ran) {
        return Verdict::Refused(format!("signature: the {stage} check did not run"));
    }

    let Some(cert) = result.certificate() else {
        return Verdict::Refused(
            "certificate: the bundle carries no signing certificate".to_string(),
        );
    };
    let claims = &cert.ci_claims;
    let repository_id = policy.repository_id.to_string();
    let fields: [(&str, &str, Option<&str>); 6] = [
        (
            "issuer (OID 1.3.6.1.4.1.57264.1.8)",
            policy.issuer,
            result.issuer(),
        ),
        (
            "source repository ID (OID 1.3.6.1.4.1.57264.1.15)",
            &repository_id,
            claims.source_repository_identifier.as_deref(),
        ),
        (
            "signer workflow (OID 1.3.6.1.4.1.57264.1.9)",
            policy.signer_workflow,
            claims.build_signer_uri.as_deref(),
        ),
        (
            "source ref (OID 1.3.6.1.4.1.57264.1.14)",
            policy.source_ref,
            claims.source_repository_ref.as_deref(),
        ),
        (
            SOURCE_COMMIT_FIELD,
            tag_commit,
            claims.source_repository_digest.as_deref(),
        ),
        (
            "runner (OID 1.3.6.1.4.1.57264.1.11)",
            policy.runner_environment,
            claims.runner_environment.as_deref(),
        ),
    ];
    for (field, expected, actual) in fields {
        if actual != Some(expected) {
            return Verdict::Refused(format!(
                "{field}: expected {expected}, certificate has {}",
                actual.unwrap_or("none")
            ));
        }
    }

    // The statement itself. The crate verified the envelope's signature and that
    // `SHA256SUMS` is bound to it; what kind of statement it is, is policy.
    let SignatureContent::DsseEnvelope(envelope) = &bundle.content else {
        return Verdict::Refused("statement: the bundle is not a DSSE attestation".to_string());
    };
    let statement: serde_json::Value = match serde_json::from_slice(envelope.payload.as_bytes()) {
        Ok(s) => s,
        Err(e) => return Verdict::Refused(format!("statement: not JSON: {e}")),
    };
    let predicate_type = statement["predicateType"].as_str();
    if predicate_type != Some(policy.predicate_type) {
        return Verdict::Refused(format!(
            "predicate type: expected {}, statement has {}",
            policy.predicate_type,
            predicate_type.unwrap_or("none")
        ));
    }
    // Restated as a plain comparison, so the property ADR-0031 names is visible
    // here rather than only inside the crate.
    let digest = sha256_hex(sums);
    let is_subject = statement["subject"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|s| s["digest"]["sha256"].as_str() == Some(digest.as_str()));
    if !is_subject {
        return Verdict::Refused(format!(
            "subject: SHA256SUMS's digest {digest} is not a subject of the attestation"
        ));
    }

    Verdict::Verified
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{RELEASE_POLICY, SANDBOX_POLICY};

    const FIXTURES: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../tests/fixtures/provenance"
    );

    /// The commits the sandbox's tags point to (public, from its release runs).
    const V001_COMMIT: &str = "8ef14b57c19a80f6f1adfc0cc37943d94e36c20f";
    const V002_COMMIT: &str = "f8e485b429bd328643a8d7fb1ee53910c55069a3";
    const V003_BRANCH_COMMIT: &str = "818a7c5ddd9cb64e0f06960f223c23aca42babfa";

    /// Test-only: a trusted root saved from Sigstore's TUF on 2026-10-11, so the
    /// suite runs offline. The shipping path has no way to load a root from a
    /// file; it only ever comes from [`fetch_trusted_root`].
    fn saved_root() -> TrustedRoot {
        TrustedRoot::from_file(format!("{FIXTURES}/trusted_root.json"))
            .expect("fixture root parses")
    }

    fn release(version: &str) -> (Vec<u8>, Vec<u8>) {
        let dir = format!("{FIXTURES}/{version}");
        (
            std::fs::read(format!("{dir}/SHA256SUMS")).unwrap(),
            std::fs::read(format!("{dir}/SHA256SUMS.sigstore.json")).unwrap(),
        )
    }

    fn refused(v: Verdict) -> String {
        match v {
            Verdict::Refused(r) => r,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn genuine_sandbox_releases_verify_under_the_sandbox_policy() {
        let root = saved_root();
        for (version, commit) in [("v0.0.1", V001_COMMIT), ("v0.0.2", V002_COMMIT)] {
            let (sums, bundle) = release(version);
            assert_eq!(
                check(&SANDBOX_POLICY, &root, &sums, &bundle, commit),
                Verdict::Verified,
                "{version}"
            );
        }
    }

    /// Each policy field, mutated alone, is refused, and the refusal names it.
    #[test]
    fn each_mutated_policy_field_is_refused_by_name() {
        let root = saved_root();
        let (sums, bundle) = release("v0.0.2");
        let cases: [(&str, Policy); 6] = [
            (
                "issuer",
                Policy { issuer: "https://accounts.example.com", ..SANDBOX_POLICY },
            ),
            (
                "source repository ID",
                Policy { repository_id: 1_414_155_781, ..SANDBOX_POLICY },
            ),
            (
                "signer workflow",
                Policy {
                    signer_workflow: "https://github.com/rejifald/faber-attest-sandbox/.github/workflows/other.yml@refs/heads/main",
                    ..SANDBOX_POLICY
                },
            ),
            (
                "source ref",
                Policy { source_ref: "refs/heads/release", ..SANDBOX_POLICY },
            ),
            (
                "runner",
                Policy { runner_environment: "self-hosted", ..SANDBOX_POLICY },
            ),
            (
                "predicate type",
                Policy { predicate_type: "https://slsa.dev/provenance/v0.2", ..SANDBOX_POLICY },
            ),
        ];
        for (field, policy) in cases {
            let reason = refused(check(&policy, &root, &sums, &bundle, V002_COMMIT));
            assert!(reason.starts_with(field), "{field}: refusal was {reason:?}");
        }
    }

    /// The bundle of a genuine release, checked against a commit the tag does
    /// not point to: an older attested build republished under a newer tag.
    #[test]
    fn a_bundle_under_the_wrong_commit_is_refused() {
        let (sums, bundle) = release("v0.0.2");
        let reason = refused(check(
            &SANDBOX_POLICY,
            &saved_root(),
            &sums,
            &bundle,
            V001_COMMIT,
        ));
        assert!(reason.starts_with("source commit"), "{reason}");
        // The real refusal carries the constant the self-check keys on.
        assert!(reason.starts_with(SOURCE_COMMIT_FIELD), "{reason}");
    }

    /// v0.0.3 was built and attested by the right workflow, but on a branch
    /// (`refs/heads/spike/branch-build`). Its SAN carries that ref, so it fails
    /// on the signer identity before the ref claim is even read; either way it
    /// is refused, naming a field that pins `main`.
    #[test]
    fn the_branch_build_draft_is_refused() {
        let (sums, bundle) = release("v0.0.3-branch-draft");
        let reason = refused(check(
            &SANDBOX_POLICY,
            &saved_root(),
            &sums,
            &bundle,
            V003_BRANCH_COMMIT,
        ));
        assert!(reason.starts_with("signer workflow"), "{reason}");
        assert!(reason.contains("refs/heads/spike/branch-build"), "{reason}");

        // With the identity widened to that branch, the ref claim still refuses.
        let branch_identity = Policy {
            signer_workflow: "https://github.com/rejifald/faber-attest-sandbox/.github/workflows/release.yml@refs/heads/spike/branch-build",
            ..SANDBOX_POLICY
        };
        let reason = refused(check(
            &branch_identity,
            &saved_root(),
            &sums,
            &bundle,
            V003_BRANCH_COMMIT,
        ));
        assert!(reason.starts_with("source ref"), "{reason}");
    }

    #[test]
    fn sha256sums_changed_by_one_byte_is_refused() {
        let (mut sums, bundle) = release("v0.0.2");
        sums[0] ^= 0x01;
        let reason = refused(check(
            &SANDBOX_POLICY,
            &saved_root(),
            &sums,
            &bundle,
            V002_COMMIT,
        ));
        assert!(reason.starts_with("signature"), "{reason}");
    }

    /// Flip one byte inside the DSSE envelope's signature, then its payload, in
    /// the bundle's JSON. Both must be refused by the crate's signature check.
    #[test]
    fn a_tampered_dsse_signature_or_payload_is_refused() {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD;
        let (sums, bundle) = release("v0.0.2");
        let original: serde_json::Value = serde_json::from_slice(&bundle).unwrap();

        for field in ["/dsseEnvelope/signatures/0/sig", "/dsseEnvelope/payload"] {
            let mut tampered = original.clone();
            let slot = tampered.pointer_mut(field).unwrap();
            let mut bytes = b64.decode(slot.as_str().unwrap()).unwrap();
            let last = bytes.len() - 1;
            bytes[last] ^= 0x01;
            *slot = serde_json::Value::String(b64.encode(bytes));
            let tampered = serde_json::to_vec(&tampered).unwrap();

            let reason = refused(check(
                &SANDBOX_POLICY,
                &saved_root(),
                &sums,
                &tampered,
                V002_COMMIT,
            ));
            assert!(reason.starts_with("signature"), "{field}: {reason}");
        }
    }

    #[test]
    fn a_bundle_that_is_not_json_is_refused() {
        let (sums, _) = release("v0.0.2");
        let reason = refused(check(
            &SANDBOX_POLICY,
            &saved_root(),
            &sums,
            b"not json",
            V002_COMMIT,
        ));
        assert!(reason.starts_with("bundle"), "{reason}");
    }

    /// The release policy pins the release repository's ID, never the
    /// placeholder.
    #[test]
    fn the_release_policy_pins_the_release_repository() {
        assert_eq!(RELEASE_POLICY.repository_id, 1_442_751_713);
    }

    /// A binary built with the repository-ID placeholder must refuse everything,
    /// genuine bundles included, and say why: this sandbox release passes every
    /// other check.
    #[test]
    fn the_placeholder_id_refuses_every_release() {
        let placeholder = Policy {
            repository_id: 0,
            ..SANDBOX_POLICY
        };
        let (sums, bundle) = release("v0.0.2");
        let reason = refused(check(
            &placeholder,
            &saved_root(),
            &sums,
            &bundle,
            V002_COMMIT,
        ));
        assert!(reason.starts_with("source repository ID"), "{reason}");
        assert!(reason.contains("placeholder"), "{reason}");
    }

    /// The shipping path end to end against the real TUF repository: the root
    /// it yields verifies a genuine sandbox release. Needs the network, so it is
    /// run by hand: `cargo test -p faber-release -- --ignored live_tuf`.
    #[tokio::test]
    #[ignore = "reaches tuf-repo-cdn.sigstore.dev"]
    async fn live_tuf_root_verifies_a_sandbox_release() {
        let cache = std::env::temp_dir().join(format!("faber-tuf-live-{}", std::process::id()));
        let root = fetch_trusted_root(&cache, "faber-release-test").await;
        let _ = std::fs::remove_dir_all(&cache);
        let root = root.expect("TUF reachable");
        let (sums, bundle) = release("v0.0.2");
        assert_eq!(
            check(&SANDBOX_POLICY, &root, &sums, &bundle, V002_COMMIT),
            Verdict::Verified
        );
    }

    /// A cache directory left world-readable (an older build, a hand-made one) is
    /// tightened to 0700, not just left as found.
    #[test]
    fn an_existing_tuf_cache_is_tightened_to_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("faber-tuf-perm-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        ensure_private_dir(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(mode, 0o700);
    }

    /// With TUF unreachable the fetch fails with a reason; there is no fallback.
    /// A refused local port stands in for an outage, so no network is needed.
    #[tokio::test]
    async fn an_unreachable_tuf_repository_is_an_error_not_a_fallback() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        let cache = std::env::temp_dir().join(format!("faber-tuf-test-{}", std::process::id()));
        let result = fetch_trusted_root_from(&url, &cache, "faber-release-test").await;
        let mode = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(&cache).map(|m| m.permissions().mode() & 0o777)
        };
        let _ = std::fs::remove_dir_all(&cache);
        assert_eq!(mode.ok(), Some(0o700), "the TUF cache is owner-only");
        let reason = match result {
            Ok(_) => panic!("an unreachable TUF repository must not yield a root"),
            Err(reason) => reason,
        };
        assert!(
            reason.contains("could not be refreshed through TUF"),
            "{reason}"
        );
        assert!(
            reason.contains(&url),
            "the reason names the repository: {reason}"
        );
    }
}
