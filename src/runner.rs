//! The `actions/runner` release asset: where this host's runner tarball lives,
//! and how a failed download of it is rendered.
//!
//! This module used to run a JIT runner directly on the host (the native
//! backend). ADR-0026 removed that backend and ADR-0052 made a profile naming it
//! a config error, so what remains is what the macOS VM backend's tools disk
//! ([`crate::vm::tools_disk`]) and setup's image download still share.

use crate::github::{transport_kind, with_causes};

/// The only platform the runner tarball is fetched for: the macOS VM's guest.
pub(crate) const PLATFORM: &str = "osx-arm64";

/// Render a `reqwest` failure the way the GitHub client renders its own: a kind
/// tag plus the whole cause chain. A release asset is fetched over the same
/// internet as the REST API and fails the same ways, so a bare "error sending
/// request" would be exactly as undiagnosable here.
pub(crate) fn transport_detail(err: &reqwest::Error) -> String {
    format!("[{}] {}", transport_kind(err), with_causes(err))
}

/// The release-asset URL for `actions/runner` at `version` on this platform.
pub(crate) fn asset_url(version: &str) -> String {
    format!(
        "https://github.com/actions/runner/releases/download/v{version}/\
         actions-runner-{PLATFORM}-{version}.tar.gz"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_url_is_well_formed() {
        assert_eq!(
            asset_url("2.330.0"),
            "https://github.com/actions/runner/releases/download/v2.330.0/\
             actions-runner-osx-arm64-2.330.0.tar.gz"
        );
    }
}
