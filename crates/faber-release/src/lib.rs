//! faber's release verification (ADR-0031 § What a follower verifies, #119).
//!
//! A follower installs a release only when `SHA256SUMS` carries a GitHub
//! attestation that `sigstore-verify` accepts under the policy compiled into this
//! binary. This crate is that check and the pure helpers around it; the update path calls it
//! through the daemon's `update::verify_provenance`, its single seam.
//!
//! **faber writes no cryptographic check here.** The certificate chain to
//! Sigstore's root, the SCT, transparency-log inclusion, the DSSE signature and
//! the binding of `SHA256SUMS`'s digest to the statement are all
//! `sigstore-verify`'s. What this crate adds is policy: string comparisons of
//! the claims the crate has already chain-verified, each refusal naming the field
//! that failed.
//!
//! **The policy is compiled in, never configured.** There is no runtime override:
//! a knob that could widen who a follower trusts is the knob an attacker would
//! turn. The one alternative, the `sandbox-policy` Cargo feature, swaps in the
//! throwaway sandbox repository's constants for end-to-end tests (#122), and
//! `release.yml` refuses to publish a binary built with it.
//!
//! **The trusted root comes from Sigstore's TUF repository, online only.** It is
//! anchored on the TUF root compiled into this binary and cached under the
//! caller's own state directory. If TUF cannot be reached the update pauses with the reason
//! ([`Verdict::Unavailable`]): neither the cache nor the crate's embedded snapshot
//! is used to ride out an outage, because the maturity wait makes a delay cost
//! nothing while a stale root could hide a revocation.
//!
//! **Why a crate.** The daemon and the menu-bar app verify releases by the same
//! rules (ADR-0034); the policy, the `SHA256SUMS` checks and the version binding
//! live here so neither can drift from the other. Everything daemon-shaped (the
//! GitHub client, the install mechanics, the state files) stays in `faber`.

mod policy;
mod release;
mod sums;
mod verify;
mod version;

pub use policy::{
    active_policy, describe_active_policy, Policy, Verdict, ACTIVE_POLICY_NAME, RELEASE_POLICY,
    SANDBOX_POLICY, SOURCE_COMMIT_FIELD,
};
pub use release::{Release, ReleaseAsset};
pub use sums::{parse_sha256sums, sha256_hex, verify_digest};
pub use verify::{check, ensure_private_dir, fetch_trusted_root, fetch_trusted_root_from};
pub use version::{
    check_bundle_version, check_reported_agent_version, check_reported_version,
    check_reported_version_for, is_newer, parse_version, pin_problem, select_candidate, Candidate,
};
// The trusted root is part of `check`'s signature; re-exported so callers need
// not depend on (or pin) a sigstore crate themselves.
pub use sigstore_trust_root::TrustedRoot;
