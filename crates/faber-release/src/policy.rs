//! The attestation policy a release must satisfy, and the verdict of checking
//! one against it (ADR-0031 § What a follower verifies, #119).

/// The provenance seam's verdict on a release's `SHA256SUMS`.
///
/// Deliberately not a `Result<(), String>`: collapsing "the check said no"
/// and "the check could not run" into one error type is exactly the
/// mistake the first draft of this design made (see `update.rs`'s provenance
/// section). Keeping them as named variants makes "which one happened" a match
/// arm instead of a string an operator has to read and interpret by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Provenance established. The only verdict that lets an update proceed.
    Verified,
    /// The check ran to completion and said no (the attack case). The reason
    /// names the field that failed.
    Refused(String),
    /// The check could not run. The release may be fine; this host cannot
    /// currently prove it.
    Unavailable(String),
}

/// What a release's attestation must say. Every field is compared as an exact
/// string against a claim `sigstore-verify` has already verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Policy {
    /// Which policy this is, for `faber provenance-policy` and `release.yml`'s
    /// assertion that a published binary carries the release one.
    pub name: &'static str,
    /// The OIDC issuer (OID 1.3.6.1.4.1.57264.1.8).
    pub issuer: &'static str,
    /// The source repository's **numeric ID** (OID .1.15). Pinned rather than
    /// the name because names move: a renamed or re-created repository must not
    /// inherit the trust. `0` is no repository's ID and refuses everything.
    pub repository_id: u64,
    /// The signing workflow, ref included: the certificate's SAN identity, and
    /// the build-signer URI claim (OID .1.9).
    pub signer_workflow: &'static str,
    /// The ref the release was built from (OID .1.14).
    pub source_ref: &'static str,
    /// Where the signing job ran (OID .1.11).
    pub runner_environment: &'static str,
    /// The in-toto statement's `predicateType`.
    pub predicate_type: &'static str,
}

const GITHUB_OIDC_ISSUER: &str = "https://token.actions.githubusercontent.com";
const SLSA_PROVENANCE_V1: &str = "https://slsa.dev/provenance/v1";

/// faber's own releases, from `rejifald/faber`.
///
/// `repository_id` is that repository's ID, set at the cut once the repository
/// was final (ADR-0031). Binaries built before then carry the placeholder `0`,
/// which matches no repository, so they refuse every release.
pub const RELEASE_POLICY: Policy = Policy {
    name: "release",
    issuer: GITHUB_OIDC_ISSUER,
    repository_id: 1_442_751_713,
    signer_workflow:
        "https://github.com/rejifald/faber/.github/workflows/release.yml@refs/heads/main",
    source_ref: "refs/heads/main",
    runner_environment: "github-hosted",
    predicate_type: SLSA_PROVENANCE_V1,
};

/// The throwaway sandbox repository's releases (ADR-0031 § What the spikes
/// found), for end-to-end tests only (#122).
///
/// Always compiled, so a test in any crate can build against it; it is *active*
/// only under the `sandbox-policy` feature (see [`active_policy`]).
pub const SANDBOX_POLICY: Policy = Policy {
    name: "sandbox",
    issuer: GITHUB_OIDC_ISSUER,
    repository_id: 1_414_155_780,
    signer_workflow:
        "https://github.com/rejifald/faber-attest-sandbox/.github/workflows/release.yml@refs/heads/main",
    source_ref: "refs/heads/main",
    runner_environment: "github-hosted",
    predicate_type: SLSA_PROVENANCE_V1,
};

/// The policy this binary enforces.
#[cfg(not(feature = "sandbox-policy"))]
pub const ACTIVE_POLICY_NAME: &str = RELEASE_POLICY.name;
#[cfg(feature = "sandbox-policy")]
pub const ACTIVE_POLICY_NAME: &str = SANDBOX_POLICY.name;

pub fn active_policy() -> &'static Policy {
    #[cfg(not(feature = "sandbox-policy"))]
    {
        &RELEASE_POLICY
    }
    #[cfg(feature = "sandbox-policy")]
    {
        &SANDBOX_POLICY
    }
}

/// The active policy, one `key: value` per line, for `faber provenance-policy`.
/// The first line is `policy: <name>`, which `release.yml` asserts is `release`.
pub fn describe_active_policy() -> String {
    let p = active_policy();
    format!(
        "policy: {}\nissuer: {}\nrepository_id: {}{}\nsigner_workflow: {}\nsource_ref: {}\nrunner_environment: {}\npredicate_type: {}\n",
        p.name,
        p.issuer,
        p.repository_id,
        if p.repository_id == 0 { " (placeholder: refuses every release)" } else { "" },
        p.signer_workflow,
        p.source_ref,
        p.runner_environment,
        p.predicate_type,
    )
}

/// The name of the certificate field pinned to the commit the release's tag
/// points to. A refusal on it rests on the tag's state in GitHub, not on the
/// bundle's bytes, so the boot self-check (#92) recognises it by this constant
/// ([`Verdict::Refused`] reasons start with it) instead of a copied string.
pub const SOURCE_COMMIT_FIELD: &str =
    "source commit (OID 1.3.6.1.4.1.57264.1.13), the commit the release's tag points to";

#[cfg(test)]
mod tests {
    use super::*;

    /// The release policy's fixed fields are ADR-0031's table, verbatim.
    #[test]
    fn the_release_policy_is_adr_0031s_table() {
        assert_eq!(RELEASE_POLICY.name, "release");
        assert_eq!(
            RELEASE_POLICY.issuer,
            "https://token.actions.githubusercontent.com"
        );
        assert_eq!(
            RELEASE_POLICY.signer_workflow,
            "https://github.com/rejifald/faber/.github/workflows/release.yml@refs/heads/main"
        );
        assert_eq!(RELEASE_POLICY.source_ref, "refs/heads/main");
        assert_eq!(RELEASE_POLICY.runner_environment, "github-hosted");
        assert_eq!(
            RELEASE_POLICY.predicate_type,
            "https://slsa.dev/provenance/v1"
        );
    }

    /// A default build enforces the release policy; only the feature swaps it.
    #[test]
    fn the_active_policy_follows_the_feature() {
        let expected = if cfg!(feature = "sandbox-policy") {
            "sandbox"
        } else {
            "release"
        };
        assert_eq!(ACTIVE_POLICY_NAME, expected);
        assert_eq!(active_policy().name, expected);
        assert!(describe_active_policy().starts_with(&format!("policy: {expected}\n")));
    }
}
