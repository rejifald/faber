//! A package the root workspace excludes is the root of a workspace of its own.
//!
//! Cargo does not stop at a root that excludes a package: it skips that root and
//! keeps looking in the directories above. In a checkout nested inside another
//! (a git worktree made inside the primary checkout) the search reaches the outer
//! checkout's `Cargo.toml`, which lists the package under neither path, and every
//! cargo command on the package fails with "current package believes it's in a
//! workspace when it's not". An empty `[workspace]` table in the package's own
//! manifest ends the search there. CI cannot see the failure: its checkout has
//! nothing above it, so a package without the table builds there and fails only
//! when nested.

use std::path::PathBuf;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The manifest at `rel` (relative to the repository root), if the file exists.
fn manifest(rel: &str) -> Option<toml::Table> {
    let text = std::fs::read_to_string(root().join(rel)).ok()?;
    Some(
        text.parse()
            .unwrap_or_else(|e| panic!("{rel} is not valid TOML: {e}")),
    )
}

#[test]
fn every_excluded_package_declares_a_workspace_of_its_own() {
    let root_manifest = manifest("Cargo.toml").expect("the root Cargo.toml");
    let excluded: Vec<&str> = root_manifest
        .get("workspace")
        .and_then(|w| w.get("exclude"))
        .and_then(|e| e.as_array())
        .map(|dirs| dirs.iter().filter_map(|d| d.as_str()).collect())
        .unwrap_or_default();

    let without: Vec<&str> = excluded
        .iter()
        .copied()
        .filter(|dir| {
            manifest(&format!("{dir}/Cargo.toml")).is_some_and(|m| !m.contains_key("workspace"))
        })
        .collect();

    assert!(
        without.is_empty(),
        "excluded from the root workspace, but with no `[workspace]` table of their own: \
         {without:?}. Add an empty `[workspace]` to each manifest, or cargo fails in a \
         checkout nested inside another."
    );
}
