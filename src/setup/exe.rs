//! The root invariant, applied to the binary running as root (ADR-0025):
//! `sudo faber setup` and `sudo faber uninstall` refuse to run a faber that
//! `_faber` or `_faber_vm` could have written.
//!
//! A binary is only as trustworthy as every directory above it: whoever can
//! write one of them can rename the binary away and put another in its place.
//! So the check reads the owner and mode of the resolved executable and of
//! each ancestor up to `/`, and refuses when any of them is
//!
//! - owned by one of faber's accounts, or
//! - writable by one of them: world-writable, or group-writable by a group
//!   either account belongs to.
//!
//! A world-writable directory with the sticky bit, such as `/tmp`, is the one
//! exception: there, only an entry's owner may rename or remove it, which the
//! ownership rule already covers. A binary the operator built or downloaded is
//! fine; the check is not about the operator.
//!
//! [`verdict`] is the rule as a pure function of those facts, so its truth
//! table is tested without any real accounts. [`chain_of`] gathers the facts.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// Owner and mode of one path on the chain from the executable up to `/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathFacts {
    pub path: PathBuf,
    pub uid: u32,
    pub gid: u32,
    /// The full `st_mode`, file type bits included.
    pub mode: u32,
}

/// The accounts root must not trust: `_faber` and `_faber_vm`'s uids, and
/// every group either belongs to. Includes ids setup is about to create, so a
/// file left behind by a deleted account that once held the same id counts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Untrusted {
    pub uids: Vec<u32>,
    pub gids: Vec<u32>,
}

const S_IFMT: u32 = 0o170_000;
const S_IFDIR: u32 = 0o040_000;
const STICKY: u32 = 0o1000;
const GROUP_WRITE: u32 = 0o020;
const WORLD_WRITE: u32 = 0o002;

/// `Ok` if nothing on `chain` is owned by or writable to an untrusted
/// account, else the first reason it is.
pub fn verdict(chain: &[PathFacts], untrusted: &Untrusted) -> Result<(), String> {
    for f in chain {
        let shown = f.path.display();
        if untrusted.uids.contains(&f.uid) {
            return Err(format!(
                "{shown} is owned by uid {}, one of faber's own accounts",
                f.uid
            ));
        }
        let sticky_dir = f.mode & S_IFMT == S_IFDIR && f.mode & STICKY != 0;
        if sticky_dir {
            continue;
        }
        if f.mode & WORLD_WRITE != 0 {
            return Err(format!(
                "{shown} is world-writable (mode {:o})",
                f.mode & 0o7777
            ));
        }
        if f.mode & GROUP_WRITE != 0 && untrusted.gids.contains(&f.gid) {
            return Err(format!(
                "{shown} is writable by group {}, which one of faber's accounts belongs to",
                f.gid
            ));
        }
    }
    Ok(())
}

/// Who other than root could replace the binary: the first account that owns
/// something on the chain, or a group that can write to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Writer {
    Account(u32),
    Group(u32),
}

/// The first non-root account that could replace the binary on `chain`, if
/// any: the owner of the file or of a directory above it, or a group allowed
/// to write one (a sticky directory excepted, as in [`verdict`]). Not a
/// refusal: an operator's own build is fine to run once. It is why the plan
/// suggests running the root-owned copy from then on.
pub fn writable_by(chain: &[PathFacts]) -> Option<Writer> {
    chain.iter().find_map(|f| {
        let sticky_dir = f.mode & S_IFMT == S_IFDIR && f.mode & STICKY != 0;
        if f.uid != 0 {
            Some(Writer::Account(f.uid))
        } else if !sticky_dir && f.mode & (GROUP_WRITE | WORLD_WRITE) != 0 {
            Some(Writer::Group(f.gid))
        } else {
            None
        }
    })
}

/// The facts for `exe`, resolved through every symlink, and each ancestor of
/// the resolved path up to `/`.
pub fn chain_of(exe: &Path) -> std::io::Result<Vec<PathFacts>> {
    let resolved = std::fs::canonicalize(exe)?;
    let mut chain = Vec::new();
    let mut at: Option<&Path> = Some(&resolved);
    while let Some(path) = at {
        let meta = std::fs::symlink_metadata(path)?;
        chain.push(PathFacts {
            path: path.to_path_buf(),
            uid: meta.uid(),
            gid: meta.gid(),
            mode: meta.mode(),
        });
        at = path.parent();
    }
    Ok(chain)
}

/// `exe` with every symlink resolved, or an error. Never the unresolved path:
/// a symlink in it would be judged by where the link sits, not by the binary
/// it points at.
pub fn resolved(exe: &Path) -> anyhow::Result<PathBuf> {
    use anyhow::Context;
    std::fs::canonicalize(exe)
        .with_context(|| format!("cannot resolve this binary, {}", exe.display()))
}

/// Refuse to go on, as root, from a binary `_faber` or `_faber_vm` could have
/// replaced: the one check every root command makes before anything else
/// (`sudo faber upgrade-cli`, `sudo faber grant`, the wall's root checker).
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn trust_running_binary() -> anyhow::Result<()> {
    use anyhow::Context;
    let exe = resolved(&std::env::current_exe()?)?;
    let chain =
        chain_of(&exe).with_context(|| format!("cannot resolve this binary, {}", exe.display()))?;
    let accounts = super::accounts::probe_faber_accounts(
        crate::platform::layout::Os::HOST,
        &mut super::tools::SystemRunner,
    )?;
    verdict(&chain, &accounts.untrusted).map_err(|reason| anyhow::anyhow!(refusal(&reason)))
}

/// The message setup and uninstall refuse with.
pub fn refusal(reason: &str) -> String {
    format!(
        "refusing to run as root: {reason}. Root must never execute a binary faber's own \
         accounts could have replaced (ADR-0025). Run a faber you built or downloaded \
         yourself, for example /usr/local/bin/faber"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unresolvable_binary_is_refused_not_trusted_as_is() {
        let missing = Path::new("/nonexistent-faber-dir/faber");
        let err = resolved(missing).unwrap_err().to_string();
        assert!(err.contains("cannot resolve this binary"), "{err}");
        let here = std::env::current_exe().unwrap();
        assert_eq!(
            resolved(&here).unwrap(),
            std::fs::canonicalize(&here).unwrap()
        );
    }

    const FILE: u32 = 0o100_000;
    const DIR: u32 = 0o040_000;

    fn facts(path: &str, uid: u32, gid: u32, mode: u32) -> PathFacts {
        PathFacts {
            path: PathBuf::from(path),
            uid,
            gid,
            mode,
        }
    }

    /// `_faber` is 451 in group 451, `_faber_vm` 452 in 452; `_faber` also
    /// belongs to 1 (as on Linux, where it joins `docker`).
    fn faber_accounts() -> Untrusted {
        Untrusted {
            uids: vec![451, 452],
            gids: vec![451, 452, 1],
        }
    }

    /// A chain from a binary in `dir` up to `/`, all root-owned 0755 but the
    /// binary and directory given.
    fn chain(bin: PathFacts, dir: PathFacts) -> Vec<PathFacts> {
        vec![
            bin,
            dir,
            facts("/usr/local", 0, 0, DIR | 0o755),
            facts("/usr", 0, 0, DIR | 0o755),
            facts("/", 0, 0, DIR | 0o755),
        ]
    }

    #[test]
    fn exe_trust_truth_table() {
        let root_bin = facts("/usr/local/bin/faber", 0, 0, FILE | 0o755);
        let root_dir = facts("/usr/local/bin", 0, 0, DIR | 0o755);
        let rows: Vec<(&str, Vec<PathFacts>, bool)> = vec![
            (
                "root-owned install",
                chain(root_bin.clone(), root_dir.clone()),
                true,
            ),
            (
                "an operator's own build",
                chain(
                    facts("/usr/local/bin/faber", 501, 20, FILE | 0o755),
                    facts("/usr/local/bin", 501, 20, DIR | 0o755),
                ),
                true,
            ),
            (
                "group-writable by a group faber is not in",
                chain(
                    root_bin.clone(),
                    facts("/usr/local/bin", 0, 80, DIR | 0o775),
                ),
                true,
            ),
            (
                "a sticky world-writable directory, like /tmp",
                chain(
                    facts("/usr/local/bin/faber", 501, 0, FILE | 0o755),
                    facts("/usr/local/bin", 0, 0, DIR | 0o1777),
                ),
                true,
            ),
            (
                "binary owned by _faber (its own daemon copy)",
                chain(
                    facts("/usr/local/bin/faber", 451, 451, FILE | 0o755),
                    root_dir.clone(),
                ),
                false,
            ),
            (
                "binary owned by _faber_vm",
                chain(
                    facts("/usr/local/bin/faber", 452, 452, FILE | 0o755),
                    root_dir.clone(),
                ),
                false,
            ),
            (
                "directory owned by _faber",
                chain(
                    root_bin.clone(),
                    facts("/usr/local/bin", 451, 451, DIR | 0o755),
                ),
                false,
            ),
            (
                "an ancestor owned by _faber",
                vec![
                    root_bin.clone(),
                    root_dir.clone(),
                    facts("/usr/local", 451, 0, DIR | 0o755),
                    facts("/", 0, 0, DIR | 0o755),
                ],
                false,
            ),
            (
                "group-writable by _faber's group",
                chain(
                    root_bin.clone(),
                    facts("/usr/local/bin", 0, 451, DIR | 0o775),
                ),
                false,
            ),
            (
                "group-writable by a supplementary group of _faber",
                chain(
                    facts("/usr/local/bin/faber", 0, 1, FILE | 0o775),
                    root_dir.clone(),
                ),
                false,
            ),
            (
                "world-writable binary",
                chain(
                    facts("/usr/local/bin/faber", 0, 0, FILE | 0o757),
                    root_dir.clone(),
                ),
                false,
            ),
            (
                "world-writable directory without the sticky bit",
                chain(root_bin.clone(), facts("/usr/local/bin", 0, 0, DIR | 0o777)),
                false,
            ),
            (
                "the sticky bit does not excuse a file",
                chain(
                    facts("/usr/local/bin/faber", 0, 0, FILE | 0o1777),
                    root_dir.clone(),
                ),
                false,
            ),
            (
                "the sticky bit does not excuse _faber's ownership",
                chain(
                    root_bin.clone(),
                    facts("/usr/local/bin", 451, 0, DIR | 0o1777),
                ),
                false,
            ),
        ];
        for (name, chain, trusted) in rows {
            assert_eq!(
                verdict(&chain, &faber_accounts()).is_ok(),
                trusted,
                "{name}: {:?}",
                verdict(&chain, &faber_accounts())
            );
        }
    }

    /// Before the accounts exist nothing is untrusted by ownership, but a
    /// world-writable binary is still refused: root never runs one.
    #[test]
    fn with_no_accounts_only_world_writability_refuses() {
        let none = Untrusted::default();
        let owned = chain(
            facts("/usr/local/bin/faber", 451, 451, FILE | 0o755),
            facts("/usr/local/bin", 0, 451, DIR | 0o775),
        );
        assert!(verdict(&owned, &none).is_ok());
        let open = chain(
            facts("/usr/local/bin/faber", 0, 0, FILE | 0o777),
            facts("/usr/local/bin", 0, 0, DIR | 0o755),
        );
        assert!(verdict(&open, &none).is_err());
    }

    /// The nudge: whoever besides root could replace the binary is named, so
    /// the plan can suggest the root-owned copy for next time.
    #[test]
    fn writable_by_names_the_first_non_root_writer() {
        let root_only = chain(
            facts("/usr/local/bin/faber", 0, 0, FILE | 0o755),
            facts("/usr/local/bin", 0, 0, DIR | 0o755),
        );
        assert_eq!(writable_by(&root_only), None);
        let own_build = chain(
            facts("/usr/local/bin/faber", 501, 20, FILE | 0o755),
            facts("/usr/local/bin", 501, 20, DIR | 0o755),
        );
        assert_eq!(writable_by(&own_build), Some(Writer::Account(501)));
        let group = chain(
            facts("/usr/local/bin/faber", 0, 0, FILE | 0o755),
            facts("/usr/local/bin", 0, 80, DIR | 0o775),
        );
        assert_eq!(writable_by(&group), Some(Writer::Group(80)));
        let sticky = chain(
            facts("/usr/local/bin/faber", 0, 0, FILE | 0o755),
            facts("/usr/local/bin", 0, 0, DIR | 0o1777),
        );
        assert_eq!(writable_by(&sticky), None);
    }

    #[test]
    fn the_reason_names_the_path() {
        let c = chain(
            facts("/usr/local/bin/faber", 0, 0, FILE | 0o755),
            facts("/usr/local/bin", 451, 451, DIR | 0o755),
        );
        let reason = verdict(&c, &faber_accounts()).unwrap_err();
        assert!(
            reason.starts_with("/usr/local/bin is owned by uid 451"),
            "{reason}"
        );
        assert!(refusal(&reason).contains("ADR-0025"));
    }

    /// The chain is the resolved path and every ancestor, ending at `/`.
    #[test]
    fn chain_of_walks_up_to_the_root() {
        let exe = std::env::current_exe().unwrap();
        let chain = chain_of(&exe).unwrap();
        assert_eq!(chain[0].path, std::fs::canonicalize(&exe).unwrap());
        assert_eq!(chain.last().unwrap().path, PathBuf::from("/"));
        for pair in chain.windows(2) {
            assert_eq!(pair[0].path.parent(), Some(pair[1].path.as_path()));
        }
    }
}
