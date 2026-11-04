//! ADR-0025's ancestor rule for the wall's own files.
//!
//! The stored wall decides the rules root loads, and the verdict decides whether
//! `_faber` admits work. A file is only as trustworthy as every directory
//! above it: whoever can write one of them can rename the file away and put
//! another in its place. So both are trusted only when root alone could have
//! written the file and every directory up to `/` — the rule
//! [`crate::setup::exe`] applies to the binary running as root, here without
//! any account exception: any owner but root, or a group- or world-writable
//! entry, is refused. A sticky directory such as `/tmp` is the one exception,
//! since there only an entry's owner may rename it, which the owner rule
//! already covers.

use crate::setup::exe::{writable_by, PathFacts, Writer};

/// `Ok` when root alone could have written everything on `chain` (a path and
/// each ancestor up to `/`, as [`crate::setup::exe::chain_of`] gathers them),
/// else why not.
pub fn root_only(chain: &[PathFacts]) -> Result<(), String> {
    if chain.is_empty() {
        return Err("no path facts to judge".into());
    }
    match writable_by(chain) {
        None => Ok(()),
        Some(writer) => {
            let at = chain
                .iter()
                .find(|f| writable_by(std::slice::from_ref(*f)).is_some())
                .map(|f| f.path.display().to_string())
                .unwrap_or_default();
            Err(match writer {
                Writer::Account(uid) => format!("{at} is owned by uid {uid}, not root"),
                Writer::Group(gid) => {
                    format!("{at} is writable by group {gid} or by everyone, not only root")
                }
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    const FILE: u32 = 0o100_000;
    const DIR: u32 = 0o040_000;

    fn f(path: &str, uid: u32, gid: u32, mode: u32) -> PathFacts {
        PathFacts {
            path: PathBuf::from(path),
            uid,
            gid,
            mode,
        }
    }

    fn chain(file: PathFacts) -> Vec<PathFacts> {
        vec![
            file,
            f("/Library/Application Support/faber", 0, 0, DIR | 0o755),
            f("/Library/Application Support", 0, 80, DIR | 0o755),
            f("/Library", 0, 0, DIR | 0o755),
            f("/", 0, 0, DIR | 0o755),
        ]
    }

    const PLAN: &str = "/Library/Application Support/faber/plan.json";

    #[test]
    fn an_all_root_chain_is_trusted() {
        assert_eq!(root_only(&chain(f(PLAN, 0, 0, FILE | 0o644))), Ok(()));
    }

    #[test]
    fn a_file_owned_by_faber_is_refused() {
        let e = root_only(&chain(f(PLAN, 451, 451, FILE | 0o644))).unwrap_err();
        assert!(e.contains("uid 451") && e.contains("plan.json"), "{e}");
    }

    #[test]
    fn a_directory_owned_by_anyone_else_is_refused() {
        let mut c = chain(f(PLAN, 0, 0, FILE | 0o644));
        c[1].uid = 451;
        let e = root_only(&c).unwrap_err();
        assert!(e.contains("uid 451") && e.ends_with("not root"), "{e}");
        assert!(e.starts_with("/Library/Application Support/faber "), "{e}");
    }

    #[test]
    fn group_or_world_writable_is_refused() {
        let e = root_only(&chain(f(PLAN, 0, 451, FILE | 0o664))).unwrap_err();
        assert!(e.contains("group 451"), "{e}");
        let mut c = chain(f(PLAN, 0, 0, FILE | 0o644));
        c[2].mode = DIR | 0o775;
        assert!(root_only(&c).is_err());
        let mut c = chain(f(PLAN, 0, 0, FILE | 0o644));
        c[3].mode = DIR | 0o757;
        assert!(root_only(&c).is_err());
        // A world-writable *file* with the sticky bit is still writable.
        assert!(root_only(&chain(f(PLAN, 0, 0, FILE | 0o1666))).is_err());
    }

    #[test]
    fn a_sticky_tmp_like_directory_is_allowed_for_a_root_file() {
        let c = vec![
            f("/private/tmp/plan.json", 0, 0, FILE | 0o644),
            f("/private/tmp", 0, 0, DIR | 0o1777),
            f("/private", 0, 0, DIR | 0o755),
            f("/", 0, 0, DIR | 0o755),
        ];
        assert_eq!(root_only(&c), Ok(()));
        // But not a file someone else put there.
        let mut c = c;
        c[0].uid = 501;
        assert!(root_only(&c).is_err());
    }

    #[test]
    fn an_empty_chain_is_refused() {
        assert!(root_only(&[]).is_err());
    }
}
