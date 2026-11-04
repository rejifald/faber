//! Copying an old per-user instance's state into the SYSTEM layout, and the
//! starter config for a host that has none (ADR-0025).
//!
//! Copy, never move: the originals stay where they were, as the backup, and
//! each copy's SHA-256 goes into the setup record. Root does the copying, so
//! both ends are guarded ([`super::fsx`]): the source is opened without
//! following a symlink and must be a regular file its account owns, and the
//! destination is written through a descriptor for `_faber`'s directory. An
//! existing destination is never overwritten; `--remigrate` first renames it
//! to `<name>.pre-remigrate-<time>`.
//!
//! Root never parses what it copies. A migrated config is validated by the
//! CLI copy running `check-config` as `_faber` ([`super::apply`]).

use std::fmt;
use std::io;

use super::fsx::{self, Account, Dir, SourceRefusal};

/// The starter config, seeded when there is no config to migrate. Its text
/// moved here from `deploy/install.sh`.
const STARTER: &str = include_str!("../../deploy/config.starter.toml");

/// What a copy did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copied {
    /// The SHA-256 of the bytes copied, in hex: what the record keeps.
    pub sha256: String,
    /// The name an existing destination was renamed to, under `--remigrate`.
    pub moved_aside: Option<String>,
}

/// Why a copy did not happen.
#[derive(Debug)]
pub enum CopyError {
    Source(SourceRefusal),
    /// The destination exists, and nothing said to replace it.
    DestExists(String),
    Io(io::Error),
}

impl fmt::Display for CopyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CopyError::Source(why) => write!(f, "the source {why}"),
            CopyError::DestExists(name) => write!(
                f,
                "{name} exists already and setup never overwrites; --remigrate moves it aside"
            ),
            CopyError::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for CopyError {}

impl From<io::Error> for CopyError {
    fn from(e: io::Error) -> Self {
        CopyError::Io(e)
    }
}

/// One file to copy and where it goes.
pub struct CopyRequest<'a> {
    pub src_dir: &'a Dir,
    pub src_name: &'a str,
    /// The uid that must own the source.
    pub src_owner: u32,
    pub dest_dir: &'a Dir,
    pub dest_name: &'a str,
    /// Owner and mode of the copy.
    pub owner: Account,
    pub mode: u32,
    /// `--remigrate`'s timestamp: move an existing destination aside with it
    /// instead of refusing.
    pub remigrate: Option<&'a str>,
}

/// Copy one file. See the module doc for the rules it enforces.
pub fn copy_one(req: &CopyRequest<'_>) -> Result<Copied, CopyError> {
    let (mut file, _) =
        fsx::open_source(req.src_dir, req.src_name, req.src_owner).map_err(CopyError::Source)?;
    let mut moved_aside = None;
    if req.dest_dir.exists(req.dest_name)? {
        let Some(ts) = req.remigrate else {
            return Err(CopyError::DestExists(req.dest_name.to_string()));
        };
        let aside = format!("{}.pre-remigrate-{ts}", req.dest_name);
        req.dest_dir
            .move_aside(req.dest_name, &aside)
            .map_err(|e| match e.kind() {
                std::io::ErrorKind::AlreadyExists => CopyError::DestExists(aside.clone()),
                _ => CopyError::Io(e),
            })?;
        moved_aside = Some(aside);
    }
    match req
        .dest_dir
        .write_atomic(req.dest_name, &mut file, req.owner, req.mode)
    {
        Ok(sha256) => Ok(Copied {
            sha256,
            moved_aside,
        }),
        Err(e) => {
            // Put back what was moved aside, so a failed copy changes nothing.
            if let Some(aside) = &moved_aside {
                let _ = req.dest_dir.rename(aside, req.dest_name);
            }
            Err(e.into())
        }
    }
}

/// The starter config, with the operator's autoupdate answer appended. A new
/// top-level table ends any array-of-tables before it, so appending `[update]`
/// is valid TOML whatever precedes it.
pub fn starter_config(autoupdate: bool) -> String {
    let mut text = STARTER.to_string();
    if autoupdate {
        text.push_str("\n[update]\nenabled = true\n");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::fsx::tests::{me, Scratch};
    use std::os::unix::fs::{symlink, MetadataExt};

    const CONFIG: &[u8] = b"topic = \"faber\"\n";
    /// `printf 'topic = "faber"\n' | shasum -a 256`, computed outside faber.
    const CONFIG_SHA: &str = "11f295f99c4851e3357812e3f7f40de4b96969086956673b5ce8a3fbcf73bfdb";

    struct Fixture {
        _scratch: Scratch,
        src: Dir,
        dest: Dir,
        root: std::path::PathBuf,
        me: Account,
    }

    fn fixture(name: &str) -> Fixture {
        let scratch = Scratch::new(name);
        let me = me(&scratch);
        std::fs::create_dir(scratch.path("home")).unwrap();
        std::fs::create_dir(scratch.path("state")).unwrap();
        std::fs::write(scratch.path("home/config.toml"), CONFIG).unwrap();
        Fixture {
            src: Dir::open_nofollow(&scratch.path("home")).unwrap(),
            dest: Dir::open_nofollow(&scratch.path("state")).unwrap(),
            root: scratch.0.clone(),
            me,
            _scratch: scratch,
        }
    }

    fn request<'a>(f: &'a Fixture, src: &'a str, remigrate: Option<&'a str>) -> CopyRequest<'a> {
        CopyRequest {
            src_dir: &f.src,
            src_name: src,
            src_owner: f.me.uid,
            dest_dir: &f.dest,
            dest_name: "config.toml",
            owner: f.me,
            mode: 0o640,
            remigrate,
        }
    }

    fn sha(bytes: &[u8]) -> String {
        fsx::sha256_of(&mut &bytes[..]).unwrap()
    }

    #[test]
    fn a_copy_records_the_sha256_of_the_bytes_copied() {
        let f = fixture("copy");
        let copied = copy_one(&request(&f, "config.toml", None)).unwrap();
        assert_eq!(copied.sha256, sha(CONFIG));
        assert_eq!(copied.moved_aside, None);
        let dest = f.root.join("state/config.toml");
        assert_eq!(std::fs::read(&dest).unwrap(), CONFIG);
        let meta = std::fs::symlink_metadata(&dest).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o640);
        // Copy, never move: the original stays as the backup.
        assert_eq!(
            std::fs::read(f.root.join("home/config.toml")).unwrap(),
            CONFIG
        );
    }

    /// The recorded digest is SHA-256 as anything else computes it, not only
    /// as this crate does.
    #[test]
    fn the_recorded_digest_matches_an_independent_computation() {
        let f = fixture("digest");
        let copied = copy_one(&request(&f, "config.toml", None)).unwrap();
        assert_eq!(copied.sha256, CONFIG_SHA);
    }

    #[test]
    fn a_symlinked_source_is_refused_and_nothing_is_written() {
        let f = fixture("symlink");
        std::fs::write(f.root.join("secret"), b"root-only").unwrap();
        symlink(f.root.join("secret"), f.root.join("home/overrides.json")).unwrap();
        let err = copy_one(&CopyRequest {
            dest_name: "overrides.json",
            ..request(&f, "overrides.json", None)
        })
        .unwrap_err();
        assert!(
            matches!(err, CopyError::Source(SourceRefusal::Symlink)),
            "{err}"
        );
        assert!(!f.root.join("state/overrides.json").exists());
    }

    #[test]
    fn a_source_owned_by_another_account_is_refused() {
        let f = fixture("owner");
        let err = copy_one(&CopyRequest {
            src_owner: f.me.uid + 1,
            ..request(&f, "config.toml", None)
        })
        .unwrap_err();
        assert!(
            matches!(err, CopyError::Source(SourceRefusal::WrongOwner { .. })),
            "{err}"
        );
    }

    #[test]
    fn an_existing_destination_is_never_overwritten() {
        let f = fixture("exists");
        std::fs::write(f.root.join("state/config.toml"), b"the daemon's own").unwrap();
        let err = copy_one(&request(&f, "config.toml", None)).unwrap_err();
        assert!(matches!(err, CopyError::DestExists(_)), "{err}");
        assert!(err.to_string().contains("--remigrate"));
        assert_eq!(
            std::fs::read(f.root.join("state/config.toml")).unwrap(),
            b"the daemon's own"
        );
    }

    #[test]
    fn remigrate_moves_the_destination_aside_first() {
        let f = fixture("remigrate");
        std::fs::write(f.root.join("state/config.toml"), b"the daemon's own").unwrap();
        let copied = copy_one(&request(&f, "config.toml", Some("20260928T120000Z"))).unwrap();
        assert_eq!(
            copied.moved_aside.as_deref(),
            Some("config.toml.pre-remigrate-20260928T120000Z")
        );
        assert_eq!(copied.sha256, sha(CONFIG));
        assert_eq!(
            std::fs::read(f.root.join("state/config.toml")).unwrap(),
            CONFIG
        );
        assert_eq!(
            std::fs::read(
                f.root
                    .join("state/config.toml.pre-remigrate-20260928T120000Z")
            )
            .unwrap(),
            b"the daemon's own"
        );
        // A second remigrate in the same second refuses rather than clobber
        // the first backup.
        let err = copy_one(&request(&f, "config.toml", Some("20260928T120000Z"))).unwrap_err();
        assert!(matches!(err, CopyError::DestExists(_)), "{err}");
    }

    #[test]
    fn a_missing_source_is_reported_as_missing() {
        let f = fixture("missing");
        let err = copy_one(&request(&f, "absent.json", None)).unwrap_err();
        assert!(matches!(err, CopyError::Source(SourceRefusal::Missing)));
    }

    /// The starter parses and validates as the daemon reads it, so a seeded
    /// host starts. `check-config` runs on it at setup time too; this catches
    /// template drift before a release does.
    #[test]
    fn the_starter_config_parses_and_validates() {
        for autoupdate in [false, true] {
            let text = starter_config(autoupdate);
            assert!(!text.contains("{{"), "{text}");
            let cfg: crate::config::FileConfig =
                toml::from_str(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
            assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
            assert_eq!(cfg.update.enabled, autoupdate);
            assert_eq!(cfg.profiles.len(), 1);
            // ADR-0052: a native profile would make the file it seeds a config
            // error, so the starter's one profile is a docker one.
            assert_eq!(cfg.profiles[0].backend, crate::config::BackendKind::Docker);
            assert_eq!(cfg.profiles[0].labels, ["faber", "docker"]);
            assert!(cfg.into_profiles("2.330.0").is_ok());
        }
    }

    #[test]
    fn the_starter_template_carries_no_placeholder_in_a_comment() {
        for line in STARTER.lines().filter(|l| l.trim_start().starts_with('#')) {
            assert!(!line.contains("{{"), "{line}");
        }
    }
}
