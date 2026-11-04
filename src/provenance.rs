//! Release verification (ADR-0031 § What a follower verifies, #119), as the
//! daemon sees it.
//!
//! The verifier itself is the `faber-release` crate, shared with the menu-bar
//! app (ADR-0034, #172); its docs describe the design. What stays here is the
//! one daemon-only piece: the TUF user agent (which carries the daemon's
//! version) and the test-only redirection of the TUF repository.

use std::path::Path;

#[cfg(test)]
use faber_release::fetch_trusted_root_from;
use faber_release::TrustedRoot;
pub use faber_release::{
    active_policy, check, describe_active_policy, Verdict, ACTIVE_POLICY_NAME, SOURCE_COMMIT_FIELD,
};

/// What the TUF requests identify as.
const TUF_USER_AGENT: &str = concat!("faber/", env!("FABER_VERSION"));

/// Fetch Sigstore's trusted root through TUF, online, caching verified metadata
/// in `cache_dir`. An `Err` is the reason it could not be had, and means
/// [`Verdict::Unavailable`]: there is no offline or embedded fallback.
pub(crate) async fn fetch_trusted_root(cache_dir: &Path) -> Result<TrustedRoot, String> {
    #[cfg(test)]
    if let Some(url) = TEST_TUF_URL.with(|u| u.borrow().clone()) {
        return fetch_trusted_root_from(&url, cache_dir, TUF_USER_AGENT).await;
    }
    faber_release::fetch_trusted_root(cache_dir, TUF_USER_AGENT).await
}

// Test-only redirection of the TUF repository, per thread (a `#[tokio::test]`
// runs on its own current-thread runtime), so a test can drive the update seam
// against an unreachable one. Compiled out of every non-test build: the URL is
// never a runtime knob.
#[cfg(test)]
thread_local! {
    pub(crate) static TEST_TUF_URL: std::cell::RefCell<Option<String>> =
        const { std::cell::RefCell::new(None) };
}
