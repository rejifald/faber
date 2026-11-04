//! Where the wall's root-owned files live, on both OSes.
//!
//! All under `/etc/faber`, the directory setup creates root-owned for its
//! record, and never under `_faber`'s state directory: `_faber` owns that, so
//! it could rename a file there away and put its own in its place, and a file
//! is only as trustworthy as every directory above it (ADR-0025). ADR-0026
//! once put the grants file in `_faber`'s home on macOS; the owner moved it
//! here for that reason.
//!
//! - [`GRANTS`]: the operator's grants, written only by `sudo faber grant`.
//! - [`PLAN`]: the network plan. `sudo faber setup` writes it
//!   ([`crate::setup::vm_runtime`]); before setup has, there is none, and
//!   `sudo faber grant` stores grants but renders nothing.
//! - [`STORED`]: the rule set rendered from the plan and the grants (narrowed
//!   by `config.toml`), written by setup and by `sudo faber grant` whenever a
//!   plan exists. The root checker, `/usr/local/bin/faber wall-daemon` under
//!   its own LaunchDaemon, enforces it.
//! - [`VERDICT`]: the checker's verdict, in [`VERDICT_DIR`], which setup
//!   creates root-owned. Under `/var/db/faber`, root's, since the checker
//!   rewrites it every few seconds. Root writes it; `_faber` reads it.
//!
//! On macOS `/etc` is a symlink to `/private/etc`, and `/var` to
//! `/private/var`, both root's own. Writers open `/etc/faber` through it
//! ([`crate::setup::fsx::open_leaf_dir`]) and refuse a symlink below it; the
//! checker, which takes canonical paths only, is given the `/private` forms:
//! [`STORED_CANONICAL`] and [`VERDICT_CANONICAL`]. So is the supervisor's
//! `--network-plan`, [`PLAN_CANONICAL`].

/// `/etc/faber/grants.toml`.
pub const GRANTS: &str = "/etc/faber/grants.toml";
/// [`GRANTS`]'s name in [`crate::platform::layout::ETC_DIR`].
pub const GRANTS_NAME: &str = "grants.toml";

/// `/etc/faber/wall`: the plan and the stored rules.
pub const WALL_DIR: &str = "/etc/faber/wall";
/// [`WALL_DIR`]'s name in [`crate::platform::layout::ETC_DIR`].
pub const WALL_DIR_NAME: &str = "wall";

/// `/etc/faber/wall/plan.json`: the network plan setup writes.
pub const PLAN: &str = "/etc/faber/wall/plan.json";
/// [`PLAN`]'s name in [`WALL_DIR`].
pub const PLAN_NAME: &str = "plan.json";

/// `/etc/faber/wall/stored.json`: the rules the checker enforces.
pub const STORED: &str = "/etc/faber/wall/stored.json";
/// [`STORED`]'s name in [`WALL_DIR`].
pub const STORED_NAME: &str = "stored.json";

/// `/private/etc/faber/wall/plan.json`: [`PLAN`] as macOS resolves it.
pub const PLAN_CANONICAL: &str = "/private/etc/faber/wall/plan.json";

/// `/private/etc/faber/wall/stored.json`: [`STORED`] as macOS resolves it,
/// the form the checker takes.
pub const STORED_CANONICAL: &str = "/private/etc/faber/wall/stored.json";

/// `/var/db/faber/wall`: the checker's verdict directory. Root's, mode 0755,
/// and so is every directory above it, so only root can put a verdict there.
pub const VERDICT_DIR: &str = "/var/db/faber/wall";
/// [`VERDICT`]'s name in [`VERDICT_DIR`].
pub const VERDICT_NAME: &str = "verdict.json";
/// [`VERDICT_DIR`] with macOS's `/var` symlink resolved: the form setup walks
/// without following a symlink.
pub const VERDICT_DIR_CANONICAL: &str = "/private/var/db/faber/wall";

/// `/var/db/faber/wall/verdict.json`: the root checker's verdict, which
/// `_faber` reads before it admits a job and setup reads before it boots the
/// Linux image's build VM.
pub const VERDICT: &str = "/var/db/faber/wall/verdict.json";
/// [`VERDICT`] with macOS's `/var` symlink resolved.
pub const VERDICT_CANONICAL: &str = "/private/var/db/faber/wall/verdict.json";

/// The most a grants file or a plan is read: either is a few hundred bytes,
/// and a larger one is refused rather than buffered.
pub const READ_CAP: u64 = 64 * 1024;

/// The most the stored rules ([`STORED`]) or the checker's verdict
/// ([`VERDICT`]) is read. Both grow with the networks in the plan: with all 16
/// networks the stored rules come to about 100 KB, past [`READ_CAP`]. A larger
/// file is refused, never read truncated.
pub const STORED_READ_CAP: u64 = 1 << 20;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::layout::ETC_DIR;

    #[test]
    fn every_path_is_its_directory_and_its_name() {
        assert_eq!(GRANTS, format!("{ETC_DIR}/{GRANTS_NAME}"));
        assert_eq!(WALL_DIR, format!("{ETC_DIR}/{WALL_DIR_NAME}"));
        assert_eq!(PLAN, format!("{WALL_DIR}/{PLAN_NAME}"));
        assert_eq!(STORED, format!("{WALL_DIR}/{STORED_NAME}"));
        assert_eq!(VERDICT, format!("{VERDICT_DIR}/{VERDICT_NAME}"));
    }

    /// The canonical forms are the plain ones under `/private`, where macOS
    /// keeps `/etc` and `/var`.
    #[test]
    fn the_canonical_forms_are_the_private_ones() {
        for (plain, canonical) in [
            (PLAN, PLAN_CANONICAL),
            (STORED, STORED_CANONICAL),
            (VERDICT_DIR, VERDICT_DIR_CANONICAL),
            (VERDICT, VERDICT_CANONICAL),
        ] {
            assert_eq!(format!("/private{plain}"), canonical);
        }
    }

    /// On this Mac, the canonical forms are what the system resolves the
    /// plain ones' existing prefix to.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_resolves_etc_and_var_under_private() {
        for (plain, canonical) in [("/etc", "/private/etc"), ("/var/db", "/private/var/db")] {
            assert_eq!(
                std::fs::canonicalize(plain).unwrap(),
                std::path::PathBuf::from(canonical)
            );
        }
    }
}
