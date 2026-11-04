//! The release shapes the verifier and the selection read from GitHub's API.

use serde::Deserialize;

/// A GitHub release (`GET /repos/{repo}/releases/...`). Originally just
/// `tag_name` for the daemon's `Github::latest_runner_version`; broadened to `pub` and
/// given the rest of a release's fields for the autoupdate release checks
/// (ADR-0006), which also need `draft`/`prerelease` (the release list and
/// `Github::release_by_tag` include both) and
/// `assets` (to find the daemon/app/checksum files by name). Both consumers
/// deserialize the same underlying GitHub object, so this is one struct
/// rather than two: `latest_runner_version` simply ignores the fields it
/// doesn't read.
#[derive(Debug, Clone, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    /// RFC 3339 publication time. One half of the maturity clock (ADR-0031):
    /// the wait runs from the later of this and when this host first saw the
    /// release as its candidate.
    #[serde(default)]
    pub published_at: Option<String>,
    #[serde(default)]
    pub assets: Vec<ReleaseAsset>,
}

/// One release asset. the daemon's `Github::download_release_asset` fetches its bytes by
/// `id`; this is the listing entry that names and sizes it.
#[derive(Debug, Clone, Deserialize)]
pub struct ReleaseAsset {
    pub id: u64,
    pub name: String,
    pub size: u64,
}
