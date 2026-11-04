//! The setup record, `/etc/faber/setup.json`: the root-owned file whose
//! presence makes a host SYSTEM (ADR-0025).
//!
//! `sudo faber setup` writes it last, once the accounts, directories, binaries
//! and service exist, and `sudo faber uninstall` removes it last. In between it
//! is what the daemon knows about how it was installed:
//!
//! - that the host is SYSTEM at all
//! - the uids it checks itself against, and later its callers (ADR-0028 reads
//!   the operator's uid from here)
//! - the `artifacts` uninstall removes. Later setup steps append their own
//!   entries: a VM's plist, a socket unit, a group.
//!
//! # The trust rule
//!
//! faber acts on the record only if nobody but root could have written it:
//!
//! - the file is a regular file, and not a symlink (`symlink_metadata` never
//!   follows one)
//! - it is owned by uid 0, and not group- or world-writable
//! - its directory is a real directory, owned by uid 0, and not group- or
//!   world-writable
//!
//! Anything else is a reason, not an error. So is a record that passes the rule
//! but doesn't parse. faber falls back to LEGACY and says why at every boot
//! ([`super::Resolution::warning`]).
//!
//! The rule is this strict for a file the daemon only reads because root acts
//! on what the record says: uninstall deletes its `artifacts` as root. A record
//! `_faber` could write would let the daemon's own account choose what root
//! removes, which is exactly what ADR-0025's root invariant forbids. Root never
//! trusts anything `_faber` or `_faber_vm` can write.
//!
//! [`trust_verdict`] is the rule as a pure function of the metadata, so its
//! truth table is tested without a root-owned file. [`load_trusted`] gathers
//! the metadata and applies it. No test reads the real `/etc/faber`.
//!
//! # Reading a record a later release wrote
//!
//! Trust comes from who could write the file, not from how strictly it is
//! parsed, so the reader is forward-compatible. It ignores fields it doesn't
//! know, and it keeps artifact kinds it doesn't know ([`Artifact::Other`]). An
//! ADR-0006 rollback therefore finds the record its successor wrote still
//! readable, instead of dropping a set-up host to LEGACY. [`SCHEMA`] says which
//! changes may rely on that and which must bump the version.

#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The schema this faber writes, and the only one it reads.
///
/// The number is reserved for breaking changes. Readers ignore unknown fields
/// and keep unknown artifact kinds, so additive changes keep schema 1:
///
/// - a new optional field, which older readers skip and newer readers default
///   when it is absent
/// - a new artifact kind, which older readers keep and write back unchanged
///
/// Anything an older reader would misread bumps it: a field that changes
/// meaning or shape, a field removed, a new field that must not be skipped.
/// Every older reader then refuses the record, which leaves its host LEGACY
/// rather than acting on a misreading.
///
/// Whatever uninstall must remove goes in `artifacts`, never in a new field.
/// An older release that rewrites the record drops fields it doesn't know, and
/// keeps only unknown artifacts.
pub const SCHEMA: u32 = 1;

/// The contents of `/etc/faber/setup.json`.
///
/// Unknown fields are ignored at every level, so a release can read a record
/// a later one wrote. They are not kept: a release that rewrites the record
/// drops them. Only unknown artifact kinds survive a rewrite
/// ([`Artifact::Other`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupRecord {
    /// Always [`SCHEMA`] in a record this build accepts.
    pub schema: u32,
    /// The person who ran `sudo faber setup`, from `SUDO_UID` and `SUDO_USER`.
    pub operator: Operator,
    /// The two role accounts setup created or adopted.
    pub accounts: Accounts,
    /// What setup installed.
    pub installed: Installed,
    /// Where migrated state came from. `None` when there was nothing to migrate.
    pub migrated_from: Option<MigratedFrom>,
    /// Everything setup created, in creation order, for uninstall to remove.
    pub artifacts: Vec<Artifact>,
}

/// The operator: a member of the operator group
/// ([`super::layout::FABER_OPERATOR_GROUP`]), never of `_faber`'s, and the only
/// caller ADR-0028's peer check lets steer the daemon.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Operator {
    pub uid: u32,
    pub name: String,
}

/// The uids and gids setup allocated for faber's two role accounts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accounts {
    /// `_faber`, which runs the daemon.
    pub faber: Account,
    /// `_faber_vm`, which runs the host side of faber's VMs (ADR-0026).
    pub faber_vm: Account,
    /// ADR-0051's session account, `_faber_mac`, whose login session runs the
    /// macOS VM supervisor. Absent in a record from before it, or on a host
    /// setup made no such account for: the daemon then keeps reaching macOS
    /// VMs through `_faber_vm`'s supervisor, as before.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub faber_mac: Option<SessionAccount>,
    /// The operator group, [`super::layout::FABER_OPERATOR_GROUP`]: what the
    /// operator reads faber's files through instead of `_faber`'s group.
    /// Absent in a record from a setup before it, whose operator is still in
    /// `_faber`'s group: the daemon then keeps the control socket's group as it
    /// finds it, and warns about the token rather than refusing it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_group: Option<Group>,
}

/// A group setup made, by its id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Group {
    pub gid: u32,
}

/// The session account's ids, and the reboot mode setup gave it (ADR-0051).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionAccount {
    pub uid: u32,
    pub gid: u32,
    /// `unlock`, `auto-login` or `manual` (`setup::session::RebootMode::name`),
    /// kept as text so a mode a later release adds never makes the record
    /// unreadable: the daemon only words its "no VM session" refusal with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reboot_mode: Option<String>,
}

/// One role account's ids. Setup picks the lowest id free as both a uid and a
/// gid, so the two are usually equal. Nothing may assume they are.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub uid: u32,
    pub gid: u32,
}

/// The release setup installed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Installed {
    pub version: String,
}

/// The old per-user instance setup migrated from. The originals are left in
/// place as the backup, so this is provenance, not a move log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigratedFrom {
    /// The account the old instance ran as.
    pub user: String,
    /// Each file copied.
    pub files: Vec<MigratedFile>,
}

/// One migrated file: where it was copied from, and the sha256 of the bytes
/// copied.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigratedFile {
    pub path: PathBuf,
    pub sha256: String,
}

/// Something setup created, for uninstall to remove.
///
/// Serialized as `{kind, path|label|name}`: the `kind` tag, plus the one
/// locator that kind needs. Uninstall runs as root, so each variant names
/// exactly one thing and carries no instruction beyond that.
///
/// A known kind's unknown fields are ignored, and dropped if this release
/// rewrites the record. An entry of an unknown kind is kept whole instead
/// ([`Artifact::Other`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Artifact {
    /// A file: a binary copy, a service definition, a VM's plist.
    File { path: PathBuf },
    /// A directory: the state, log and cache trees, [`super::layout::ETC_DIR`],
    /// a VM home.
    Dir { path: PathBuf },
    /// A launchd job in the system domain, by label.
    LaunchdJob { label: String },
    /// A systemd unit, by name, such as `faber.service` or a socket unit.
    SystemdUnit { name: String },
    /// A user account.
    User { name: String },
    /// A group.
    Group { name: String },
    /// An entry this release can't read as any kind above: usually a kind a
    /// later release added, but also a known kind in a shape this release
    /// doesn't recognize. Kept as its raw JSON and written back unchanged, so
    /// a release that rewrites a newer record never drops what another
    /// release added. Uninstall can't know what such an entry names, so it
    /// must leave it alone rather than guess.
    #[serde(untagged)]
    Other(serde_json::Value),
}

impl SetupRecord {
    /// The operator's uid.
    pub fn operator_uid(&self) -> u32 {
        self.operator.uid
    }

    /// The operator's account name.
    pub fn operator_name(&self) -> &str {
        &self.operator.name
    }

    /// `_faber`'s uid.
    pub fn faber_uid(&self) -> u32 {
        self.accounts.faber.uid
    }

    /// `_faber`'s primary gid: the group that reads the token.
    pub fn faber_gid(&self) -> u32 {
        self.accounts.faber.gid
    }

    /// The operator group's gid, which the operator reads faber's files
    /// through; `None` in a record from before setup made it.
    pub fn operator_gid(&self) -> Option<u32> {
        self.accounts.operator_group.map(|g| g.gid)
    }

    /// `_faber_vm`'s uid.
    pub fn faber_vm_uid(&self) -> u32 {
        self.accounts.faber_vm.uid
    }

    /// `_faber_vm`'s primary gid.
    pub fn faber_vm_gid(&self) -> u32 {
        self.accounts.faber_vm.gid
    }

    /// The session account (ADR-0051), when setup recorded one.
    pub fn session_account(&self) -> Option<&SessionAccount> {
        self.accounts.faber_mac.as_ref()
    }

    /// `_faber_mac`'s uid, when setup recorded the account.
    pub fn faber_mac_uid(&self) -> Option<u32> {
        self.session_account().map(|a| a.uid)
    }

    /// Parse a record, refusing any schema but [`SCHEMA`].
    ///
    /// The schema is read on its own first. Unknown fields are ignored, so a
    /// record from a breaking schema could otherwise parse cleanly and be
    /// misread; its version is what refuses it.
    pub fn from_json(bytes: &[u8]) -> Result<Self, TrustError> {
        #[derive(Deserialize)]
        struct Version {
            schema: u32,
        }

        let malformed = |e: serde_json::Error| TrustError::Malformed(e.to_string());
        let Version { schema } = serde_json::from_slice(bytes).map_err(malformed)?;
        if schema != SCHEMA {
            return Err(TrustError::UnsupportedSchema { found: schema });
        }
        serde_json::from_slice(bytes).map_err(malformed)
    }

    /// The record as setup writes it: pretty-printed, with a trailing newline.
    /// Fails only on a path that is not valid UTF-8.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        let mut json = serde_json::to_string_pretty(self)?;
        json.push('\n');
        Ok(json)
    }
}

/// Why faber will not act on a setup record that exists.
///
/// Each message is a predicate of the file ("is a symlink"), so a caller can
/// write it straight after the path, or after "it" as
/// [`super::Resolution::warning`] does.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TrustError {
    #[error("is a symlink")]
    Symlink,
    #[error("is not a regular file")]
    NotRegular,
    #[error("is owned by uid {uid}, not root")]
    NotRootOwned { uid: u32 },
    #[error("is group- or world-writable (mode {:o})", .mode & 0o7777)]
    Writable { mode: u32 },
    #[error("is not in a real directory (its parent is missing or a symlink)")]
    ParentNotDir,
    #[error("is in a directory owned by uid {uid}, not root")]
    ParentNotRootOwned { uid: u32 },
    #[error("is in a group- or world-writable directory (mode {:o})", .mode & 0o7777)]
    ParentWritable { mode: u32 },
    #[error("could not be read: {0}")]
    Unreadable(String),
    #[error("is not a valid setup record: {0}")]
    Malformed(String),
    #[error("has schema {found}, and this faber reads only schema {schema}", schema = SCHEMA)]
    UnsupportedSchema { found: u32 },
}

/// The metadata the trust rule reads. [`load_trusted`] gathers it with
/// `symlink_metadata`, which reports on the path itself and never follows a
/// symlink. The struct exists so that a test can state the facts directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MetaFacts {
    /// The record's path is itself a symlink.
    pub is_symlink: bool,
    /// The record's path is a regular file.
    pub is_regular: bool,
    /// The record's owner.
    pub uid: u32,
    /// The record's `st_mode`.
    pub mode: u32,
    /// The record's parent is a directory, and not a symlink to one.
    pub parent_is_dir: bool,
    /// The parent directory's owner.
    pub parent_uid: u32,
    /// The parent directory's `st_mode`.
    pub parent_mode: u32,
}

/// Group-write and world-write.
const GROUP_OR_WORLD_WRITABLE: u32 = 0o022;

/// The trust rule in the module doc, as a pure function: `Ok` only if nobody
/// but root could have written the record, else the first defect found.
///
/// The file is checked before its directory, so each defect of the file itself
/// is reported whoever runs the check.
pub fn trust_verdict(facts: &MetaFacts) -> Result<(), TrustError> {
    if facts.is_symlink {
        return Err(TrustError::Symlink);
    }
    if !facts.is_regular {
        return Err(TrustError::NotRegular);
    }
    if facts.uid != 0 {
        return Err(TrustError::NotRootOwned { uid: facts.uid });
    }
    if facts.mode & GROUP_OR_WORLD_WRITABLE != 0 {
        return Err(TrustError::Writable { mode: facts.mode });
    }
    if !facts.parent_is_dir {
        return Err(TrustError::ParentNotDir);
    }
    if facts.parent_uid != 0 {
        return Err(TrustError::ParentNotRootOwned {
            uid: facts.parent_uid,
        });
    }
    if facts.parent_mode & GROUP_OR_WORLD_WRITABLE != 0 {
        return Err(TrustError::ParentWritable {
            mode: facts.parent_mode,
        });
    }
    Ok(())
}

/// Read the setup record at `path` if it is there and trusted.
///
/// - `Ok(None)`: there is no record, so the host is LEGACY.
/// - `Ok(Some(_))`: the record passed [`trust_verdict`] and parsed, so the host
///   is SYSTEM.
/// - `Err(_)`: a record exists but faber will not act on it.
///
/// The file is read after the check, not through a descriptor opened before it.
/// That leaves no window an unprivileged process can use: once the check
/// passes, only root can replace the file or change its directory.
#[cfg(unix)]
pub fn load_trusted(path: &Path) -> Result<Option<SetupRecord>, TrustError> {
    let Some(facts) = gather_facts(path)? else {
        return Ok(None);
    };
    trust_verdict(&facts)?;
    let bytes = std::fs::read(path).map_err(|e| TrustError::Unreadable(e.to_string()))?;
    SetupRecord::from_json(&bytes).map(Some)
}

/// The [`MetaFacts`] for `path` and its parent directory, or `None` when
/// nothing is at `path`.
#[cfg(unix)]
fn gather_facts(path: &Path) -> Result<Option<MetaFacts>, TrustError> {
    use std::os::unix::fs::MetadataExt;

    let file = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(TrustError::Unreadable(e.to_string())),
    };
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or(TrustError::ParentNotDir)?;
    let dir = std::fs::symlink_metadata(parent)
        .map_err(|e| TrustError::Unreadable(format!("its directory: {e}")))?;
    Ok(Some(MetaFacts {
        is_symlink: file.file_type().is_symlink(),
        is_regular: file.file_type().is_file(),
        uid: file.uid(),
        mode: file.mode(),
        parent_is_dir: dir.file_type().is_dir(),
        parent_uid: dir.uid(),
        parent_mode: dir.mode(),
    }))
}

#[cfg(unix)]
extern "C" {
    /// POSIX. Takes nothing, cannot fail, and returns a `uid_t`, which is a
    /// 32-bit unsigned integer on both macOS and Linux.
    fn geteuid() -> std::os::raw::c_uint;
}

/// This process's effective uid, for the daemon's check of which account it
/// runs as. Declared by hand, as `crate::lock` declares `flock`, so that one
/// call does not bring in a `libc` dependency.
#[cfg(unix)]
pub fn current_euid() -> u32 {
    // SAFETY: `geteuid` takes no arguments, always succeeds, and reads and
    // writes no memory the caller owns.
    unsafe { geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    /// A record with every field set and every known artifact kind present.
    fn full_record() -> SetupRecord {
        SetupRecord {
            schema: SCHEMA,
            operator: Operator {
                uid: 501,
                name: "owner".into(),
            },
            accounts: Accounts {
                faber: Account { uid: 451, gid: 451 },
                faber_vm: Account { uid: 452, gid: 453 },
                faber_mac: None,
                operator_group: None,
            },
            installed: Installed {
                version: "0.4.0".into(),
            },
            migrated_from: Some(MigratedFrom {
                user: "owner".into(),
                files: vec![MigratedFile {
                    path: "/Users/owner/Library/Application Support/faber/config.toml".into(),
                    sha256: EMPTY_SHA256.into(),
                }],
            }),
            artifacts: vec![
                Artifact::User {
                    name: "_faber".into(),
                },
                Artifact::Group {
                    name: "_faber".into(),
                },
                Artifact::Dir {
                    path: "/etc/faber".into(),
                },
                Artifact::File {
                    path: "/usr/local/bin/faber".into(),
                },
                Artifact::LaunchdJob {
                    label: "io.github.rejifald.faber".into(),
                },
                Artifact::SystemdUnit {
                    name: "faber.service".into(),
                },
            ],
        }
    }

    // --- the trust rule ------------------------------------------------------

    /// A record exactly as setup leaves it: root:wheel 0644 in root:wheel 0755.
    fn as_setup_leaves_it() -> MetaFacts {
        MetaFacts {
            is_symlink: false,
            is_regular: true,
            uid: 0,
            mode: 0o100644,
            parent_is_dir: true,
            parent_uid: 0,
            parent_mode: 0o040755,
        }
    }

    /// The truth table, row by row: each defect alone, and what it is
    /// reported as.
    #[test]
    fn trust_verdict_truth_table() {
        let ok = as_setup_leaves_it();
        let rows: Vec<(&str, MetaFacts, Result<(), TrustError>)> = vec![
            ("as setup leaves it", ok, Ok(())),
            (
                "owner-only file",
                MetaFacts {
                    mode: 0o100600,
                    ..ok
                },
                Ok(()),
            ),
            (
                "read-only file",
                MetaFacts {
                    mode: 0o100444,
                    ..ok
                },
                Ok(()),
            ),
            (
                "traversable-only dir",
                MetaFacts {
                    parent_mode: 0o040711,
                    ..ok
                },
                Ok(()),
            ),
            (
                "root-only dir",
                MetaFacts {
                    parent_mode: 0o040700,
                    ..ok
                },
                Ok(()),
            ),
            (
                "symlink",
                MetaFacts {
                    is_symlink: true,
                    is_regular: false,
                    ..ok
                },
                Err(TrustError::Symlink),
            ),
            (
                "directory at the path",
                MetaFacts {
                    is_regular: false,
                    ..ok
                },
                Err(TrustError::NotRegular),
            ),
            (
                "owned by the operator",
                MetaFacts { uid: 501, ..ok },
                Err(TrustError::NotRootOwned { uid: 501 }),
            ),
            (
                "owned by _faber",
                MetaFacts { uid: 451, ..ok },
                Err(TrustError::NotRootOwned { uid: 451 }),
            ),
            (
                "group-writable",
                MetaFacts {
                    mode: 0o100664,
                    ..ok
                },
                Err(TrustError::Writable { mode: 0o100664 }),
            ),
            (
                "world-writable",
                MetaFacts {
                    mode: 0o100646,
                    ..ok
                },
                Err(TrustError::Writable { mode: 0o100646 }),
            ),
            (
                "symlinked directory",
                MetaFacts {
                    parent_is_dir: false,
                    ..ok
                },
                Err(TrustError::ParentNotDir),
            ),
            (
                "directory owned by the operator",
                MetaFacts {
                    parent_uid: 501,
                    ..ok
                },
                Err(TrustError::ParentNotRootOwned { uid: 501 }),
            ),
            (
                "group-writable directory",
                MetaFacts {
                    parent_mode: 0o040775,
                    ..ok
                },
                Err(TrustError::ParentWritable { mode: 0o040775 }),
            ),
            (
                "world-writable directory",
                MetaFacts {
                    parent_mode: 0o040757,
                    ..ok
                },
                Err(TrustError::ParentWritable { mode: 0o040757 }),
            ),
            (
                "sticky world-writable directory, like /tmp",
                MetaFacts {
                    parent_mode: 0o041777,
                    ..ok
                },
                Err(TrustError::ParentWritable { mode: 0o041777 }),
            ),
            // Several defects at once: the file's own come first.
            (
                "operator-owned symlink in an operator dir",
                MetaFacts {
                    is_symlink: true,
                    is_regular: false,
                    uid: 501,
                    parent_uid: 501,
                    ..ok
                },
                Err(TrustError::Symlink),
            ),
            (
                "writable file in a writable dir",
                MetaFacts {
                    mode: 0o100666,
                    parent_mode: 0o040777,
                    ..ok
                },
                Err(TrustError::Writable { mode: 0o100666 }),
            ),
        ];
        for (name, facts, want) in rows {
            assert_eq!(trust_verdict(&facts), want, "{name}");
        }
    }

    /// Every combination of the seven facts: trusted if and only if each holds.
    /// The table above pins the reasons; this pins that no combination slips
    /// through.
    #[test]
    fn trust_verdict_is_ok_exactly_when_every_fact_holds() {
        let mut trusted = 0;
        for is_symlink in [false, true] {
            for is_regular in [true, false] {
                for uid in [0, 451, 501] {
                    for mode in [0o100644, 0o100600, 0o100664, 0o100646, 0o104755] {
                        for parent_is_dir in [true, false] {
                            for parent_uid in [0, 451, 501] {
                                for parent_mode in [0o040755, 0o040775, 0o040757, 0o041777] {
                                    let facts = MetaFacts {
                                        is_symlink,
                                        is_regular,
                                        uid,
                                        mode,
                                        parent_is_dir,
                                        parent_uid,
                                        parent_mode,
                                    };
                                    let expected = !is_symlink
                                        && is_regular
                                        && uid == 0
                                        && mode & 0o022 == 0
                                        && parent_is_dir
                                        && parent_uid == 0
                                        && parent_mode & 0o022 == 0;
                                    assert_eq!(
                                        trust_verdict(&facts).is_ok(),
                                        expected,
                                        "{facts:?}"
                                    );
                                    trusted += usize::from(expected);
                                }
                            }
                        }
                    }
                }
            }
        }
        // 0644, 0600 and setuid 4755 are the modes with no group or world write.
        assert_eq!(trusted, 3);
    }

    #[test]
    fn a_reason_names_the_defect_and_the_mode_in_octal() {
        assert_eq!(
            TrustError::Writable { mode: 0o100664 }.to_string(),
            "is group- or world-writable (mode 664)"
        );
        assert_eq!(
            TrustError::ParentWritable { mode: 0o041777 }.to_string(),
            "is in a group- or world-writable directory (mode 1777)"
        );
        assert_eq!(
            TrustError::UnsupportedSchema { found: 2 }.to_string(),
            "has schema 2, and this faber reads only schema 1"
        );
    }

    // --- the record's wire format ---------------------------------------------

    #[test]
    fn a_record_round_trips_through_its_own_json() {
        let record = full_record();
        let json = record.to_json().expect("serializes");
        assert!(json.ends_with("}\n"), "{json}");
        assert_eq!(SetupRecord::from_json(json.as_bytes()), Ok(record));
    }

    /// The wire format is a contract between the setup that writes a record and
    /// every later release that reads it, so it is pinned as literal JSON.
    #[test]
    fn the_wire_format_is_pinned() {
        let json = format!(
            r#"{{
                "schema": 1,
                "operator": {{"uid": 501, "name": "owner"}},
                "accounts": {{
                    "faber": {{"uid": 451, "gid": 451}},
                    "faber_vm": {{"uid": 452, "gid": 453}}
                }},
                "installed": {{"version": "0.4.0"}},
                "migrated_from": {{
                    "user": "owner",
                    "files": [{{
                        "path": "/Users/owner/Library/Application Support/faber/config.toml",
                        "sha256": "{EMPTY_SHA256}"
                    }}]
                }},
                "artifacts": [
                    {{"kind": "user", "name": "_faber"}},
                    {{"kind": "group", "name": "_faber"}},
                    {{"kind": "dir", "path": "/etc/faber"}},
                    {{"kind": "file", "path": "/usr/local/bin/faber"}},
                    {{"kind": "launchd_job", "label": "io.github.rejifald.faber"}},
                    {{"kind": "systemd_unit", "name": "faber.service"}}
                ]
            }}"#
        );
        assert_eq!(SetupRecord::from_json(json.as_bytes()), Ok(full_record()));

        let written: serde_json::Value =
            serde_json::from_str(&full_record().to_json().unwrap()).unwrap();
        assert_eq!(
            written["artifacts"][4],
            serde_json::json!({"kind": "launchd_job", "label": "io.github.rejifald.faber"})
        );
    }

    #[test]
    fn the_accessors_read_the_ids() {
        let r = full_record();
        assert_eq!(r.operator_uid(), 501);
        assert_eq!(r.operator_name(), "owner");
        assert_eq!((r.faber_uid(), r.faber_gid()), (451, 451));
        assert_eq!((r.faber_vm_uid(), r.faber_vm_gid()), (452, 453));
    }

    /// ADR-0051's session account: absent in a record from before it, and
    /// absent from what setup writes without one; a mode this release does
    /// not know is kept as text rather than refusing the record.
    #[test]
    fn the_session_account_is_optional_and_its_mode_is_text() {
        let r = full_record();
        assert_eq!(r.faber_mac_uid(), None);
        let json = r.to_json().unwrap();
        assert!(!json.contains("faber_mac"), "{json}");

        let mut with = full_record();
        with.accounts.faber_mac = Some(SessionAccount {
            uid: 600,
            gid: 20,
            reboot_mode: Some("unlock".into()),
        });
        let json = with.to_json().unwrap();
        assert_eq!(SetupRecord::from_json(json.as_bytes()), Ok(with.clone()));
        assert_eq!(with.faber_mac_uid(), Some(600));

        let later = json.replace("\"unlock\"", "\"some-later-mode\"");
        let read = SetupRecord::from_json(later.as_bytes()).unwrap();
        assert_eq!(
            read.session_account().unwrap().reboot_mode.as_deref(),
            Some("some-later-mode")
        );
    }

    /// The operator group: absent in a record from before it, and left out of
    /// what setup writes without one, so such a record reads back unchanged;
    /// with one, its gid round-trips.
    #[test]
    fn the_operator_group_is_optional() {
        let r = full_record();
        assert_eq!(r.operator_gid(), None);
        let json = r.to_json().unwrap();
        assert!(!json.contains("operator_group"), "{json}");

        let mut with = full_record();
        with.accounts.operator_group = Some(Group { gid: 454 });
        let json = with.to_json().unwrap();
        assert!(json.contains("\"operator_group\""), "{json}");
        assert_eq!(SetupRecord::from_json(json.as_bytes()), Ok(with.clone()));
        assert_eq!(with.operator_gid(), Some(454));
        assert_eq!(with.faber_gid(), 451, "the token's group is still _faber's");
    }

    /// A host with nothing to migrate writes `null`, and a record that leaves
    /// the key out reads the same way.
    #[test]
    fn nothing_migrated_is_null_or_absent() {
        let record = SetupRecord {
            migrated_from: None,
            ..full_record()
        };
        let json = record.to_json().unwrap();
        assert!(json.contains("\"migrated_from\": null"), "{json}");
        assert_eq!(SetupRecord::from_json(json.as_bytes()), Ok(record.clone()));

        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value.as_object_mut().unwrap().remove("migrated_from");
        let absent = serde_json::to_vec(&value).unwrap();
        assert_eq!(SetupRecord::from_json(&absent), Ok(record));
    }

    /// Apply `edit` to the full record's JSON and parse the result.
    fn parse_edited(edit: impl FnOnce(&mut serde_json::Value)) -> Result<SetupRecord, TrustError> {
        let mut value = serde_json::to_value(full_record()).unwrap();
        edit(&mut value);
        SetupRecord::from_json(&serde_json::to_vec(&value).unwrap())
    }

    fn assert_malformed(result: Result<SetupRecord, TrustError>, needle: &str) {
        match result {
            Err(TrustError::Malformed(msg)) => assert!(msg.contains(needle), "{msg}"),
            other => panic!("expected Malformed mentioning {needle:?}, got {other:?}"),
        }
    }

    /// A record a later release wrote, with new fields at the top level and
    /// inside every nested object, including a known artifact kind, reads as
    /// the record this release knows.
    #[test]
    fn unknown_fields_are_ignored_at_every_level() {
        let newer = parse_edited(|v| {
            v["issued_by"] = serde_json::json!({"release": "0.5.0"});
            v["operator"]["gid"] = serde_json::json!(20);
            v["accounts"]["faber"]["home"] = serde_json::json!("/var/lib/faber");
            v["accounts"]["ci"] = serde_json::json!({"uid": 1, "gid": 1});
            v["installed"]["channel"] = serde_json::json!("stable");
            v["migrated_from"]["files"][0]["mode"] = serde_json::json!(420);
            v["artifacts"][3]["mode"] = serde_json::json!(493);
        });
        assert_eq!(newer, Ok(full_record()));
    }

    /// Unknown fields are ignored, not kept: a rewrite drops them. Pinned so
    /// that the loss is a documented choice, and so that a later release
    /// knows to put whatever uninstall needs in `artifacts` (see [`SCHEMA`]).
    #[test]
    fn a_rewrite_drops_unknown_fields() {
        let record = parse_edited(|v| {
            v["issued_by"] = serde_json::json!("0.5.0");
            v["artifacts"][3]["mode"] = serde_json::json!(493);
        })
        .unwrap();
        let rewritten: serde_json::Value =
            serde_json::from_str(&record.to_json().unwrap()).unwrap();
        assert_eq!(rewritten, serde_json::to_value(full_record()).unwrap());
    }

    /// An entry of a kind this release doesn't know is kept whole, in its
    /// place, and written back as the same JSON value. So is the rest of the
    /// record around it.
    #[test]
    fn an_unknown_artifact_kind_round_trips_unchanged() {
        let entry = serde_json::json!({
            "kind": "vm_socket",
            "path": "/var/db/faber-vm/vm.sock",
            "owner": {"uid": 452, "mode": 384},
            "ports": [22, 2375],
        });
        let mut original = serde_json::to_value(full_record()).unwrap();
        original["artifacts"]
            .as_array_mut()
            .unwrap()
            .insert(2, entry.clone());

        let record = SetupRecord::from_json(&serde_json::to_vec(&original).unwrap()).unwrap();
        assert_eq!(record.artifacts.len(), 7);
        assert_eq!(record.artifacts[2], Artifact::Other(entry));
        assert_eq!(
            record.artifacts[3],
            Artifact::Dir {
                path: "/etc/faber".into()
            }
        );

        let rewritten: serde_json::Value =
            serde_json::from_str(&record.to_json().unwrap()).unwrap();
        assert_eq!(rewritten, original);
    }

    /// A known kind in a shape this release doesn't recognize is kept the same
    /// way. It still names something another release created, so losing it,
    /// or refusing the whole record over it, would be worse.
    #[test]
    fn a_known_kind_in_an_unknown_shape_is_kept_too() {
        let odd = serde_json::json!({"kind": "file", "path": {"under": "/var/lib/faber"}});
        let record = parse_edited(|v| v["artifacts"] = serde_json::json!([odd.clone()])).unwrap();
        assert_eq!(record.artifacts, vec![Artifact::Other(odd)]);
    }

    #[test]
    fn a_missing_field_is_refused() {
        assert_malformed(
            parse_edited(|v| {
                v.as_object_mut().unwrap().remove("accounts");
            }),
            "missing field `accounts`",
        );
        assert_malformed(
            parse_edited(|v| {
                v.as_object_mut().unwrap().remove("schema");
            }),
            "missing field `schema`",
        );
    }

    /// A record from another schema is refused by its version, even though
    /// everything in it would parse: unknown fields are ignored, so the
    /// version is all that stops an older reader misreading a breaking change.
    #[test]
    fn another_schema_is_refused_by_version() {
        for schema in [0, 2] {
            assert_eq!(
                parse_edited(|v| {
                    v["schema"] = serde_json::json!(schema);
                    v["tokens"] = serde_json::json!("/etc/faber/credentials");
                }),
                Err(TrustError::UnsupportedSchema { found: schema })
            );
        }
    }

    #[test]
    fn junk_is_malformed() {
        assert_malformed(SetupRecord::from_json(b"not json"), "expected ident");
        assert_malformed(SetupRecord::from_json(b""), "EOF");
    }

    // --- reading a real file --------------------------------------------------

    /// These use a scratch directory, never `/etc/faber`. A test can't create a
    /// root-owned file without running as root, so each asserts the outcome for
    /// whichever account runs it. Both CI legs run unprivileged; the Linux
    /// container recipe in AGENTS.md runs as root.
    #[cfg(unix)]
    mod on_disk {
        use super::*;
        use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

        /// A scratch directory at mode 0755, removed on drop.
        struct Scratch(PathBuf);

        impl Scratch {
            fn new(name: &str) -> Self {
                let dir = std::env::temp_dir().join(format!("fbr-{name}-{}", std::process::id()));
                let _ = std::fs::remove_dir_all(&dir);
                std::fs::create_dir(&dir).unwrap();
                chmod(&dir, 0o755);
                Scratch(dir)
            }

            fn path(&self, rel: &str) -> PathBuf {
                self.0.join(rel)
            }
        }

        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        fn chmod(path: &Path, mode: u32) {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
        }

        /// Write `record` the way setup does: 0644 in the scratch directory.
        fn write_record(scratch: &Scratch, record: &SetupRecord) -> PathBuf {
            let path = scratch.path("setup.json");
            std::fs::write(&path, record.to_json().unwrap()).unwrap();
            chmod(&path, 0o644);
            path
        }

        /// A file this process creates is owned by its effective uid, so this
        /// checks `current_euid` without spawning `id -u`. A spawn would fork,
        /// and a fork holds `crate::lock`'s test locks open (issue #47).
        #[test]
        fn current_euid_owns_what_this_process_creates() {
            let scratch = Scratch::new("euid");
            let path = scratch.path("owned");
            std::fs::write(&path, b"").unwrap();
            assert_eq!(std::fs::metadata(&path).unwrap().uid(), current_euid());
        }

        #[test]
        fn no_record_is_none() {
            let scratch = Scratch::new("absent");
            assert_eq!(load_trusted(&scratch.path("setup.json")), Ok(None));
        }

        /// The facts come from the file and from its directory, whoever runs
        /// the test.
        #[test]
        fn facts_are_read_from_the_file_and_its_directory() {
            let scratch = Scratch::new("facts");
            let path = write_record(&scratch, &full_record());
            let facts = gather_facts(&path).unwrap().expect("the file exists");
            let euid = current_euid();
            assert_eq!(
                facts,
                MetaFacts {
                    is_symlink: false,
                    is_regular: true,
                    uid: euid,
                    mode: facts.mode,
                    parent_is_dir: true,
                    parent_uid: euid,
                    parent_mode: facts.parent_mode,
                }
            );
            assert_eq!(facts.mode & 0o7777, 0o644);
            assert_eq!(facts.parent_mode & 0o7777, 0o755);
        }

        /// A symlink is reported as one, not followed to its target.
        #[test]
        fn a_symlinked_record_is_refused_not_followed() {
            let scratch = Scratch::new("link");
            let target = write_record(&scratch, &full_record());
            let link = scratch.path("linked.json");
            symlink(&target, &link).unwrap();
            assert_eq!(load_trusted(&link), Err(TrustError::Symlink));
        }

        /// Reaching the record through a symlinked directory shows up as a
        /// parent that is not a real directory.
        #[test]
        fn a_symlinked_directory_is_not_a_real_one() {
            let scratch = Scratch::new("dirlink");
            write_record(&scratch, &full_record());
            let via = std::env::temp_dir().join(format!("fbr-via-{}", std::process::id()));
            let _ = std::fs::remove_file(&via);
            symlink(&scratch.0, &via).unwrap();
            let facts = gather_facts(&via.join("setup.json"));
            let _ = std::fs::remove_file(&via);
            let facts = facts.unwrap().expect("the file exists");
            assert!(facts.is_regular);
            assert!(!facts.parent_is_dir);
        }

        #[test]
        fn a_directory_at_the_path_is_refused() {
            let scratch = Scratch::new("dir");
            let path = scratch.path("setup.json");
            std::fs::create_dir(&path).unwrap();
            assert_eq!(load_trusted(&path), Err(TrustError::NotRegular));
        }

        /// A well-formed record owned by whoever runs the test. Unprivileged,
        /// that is the refusal every non-root writer gets; as root, the file
        /// passes the rule and is read.
        #[test]
        fn a_record_is_trusted_only_when_root_wrote_it() {
            let scratch = Scratch::new("owned");
            let path = write_record(&scratch, &full_record());
            match current_euid() {
                0 => assert_eq!(load_trusted(&path), Ok(Some(full_record()))),
                uid => assert_eq!(load_trusted(&path), Err(TrustError::NotRootOwned { uid })),
            }
        }

        /// A trusted file that doesn't parse is still refused. Only reachable
        /// as root, so unprivileged runs check the refusal they get instead.
        #[test]
        fn a_trusted_file_that_does_not_parse_is_refused() {
            let scratch = Scratch::new("junk");
            let path = scratch.path("setup.json");
            std::fs::write(&path, b"{}").unwrap();
            chmod(&path, 0o644);
            match current_euid() {
                0 => assert!(matches!(load_trusted(&path), Err(TrustError::Malformed(_)))),
                uid => assert_eq!(load_trusted(&path), Err(TrustError::NotRootOwned { uid })),
            }
        }
    }
}
