//! The daemon's GitHub credential (ADR-0027): a fine-grained personal access
//! token the operator hands over with `sudo faber auth set`, kept where only
//! root can write it and only `_faber` (through its group) can read it, and
//! checked for what it can do.
//!
//! # The store
//!
//! One file per owner, `github-<owner>.json`, in [`CREDENTIALS_DIR`]: the
//! directory owned by root at mode 0755, the file by `root:_faber` at mode 0640,
//! the same rule as `/etc/faber/grants.toml`: root writes, faber reads. Root
//! writes it ([`Store::save`], atomically, through [`crate::setup::fsx`], never
//! following a link); the daemon, as `_faber`, reads it ([`Store::load`]).
//! `_faber` cannot rewrite its own token. Whatever about the file or its
//! directory is not exactly that is refused with a reason and never used
//! ([`ownership`]). faber serves one owner, so more than one file is refused too.
//!
//! One exception, read but never refused: the store's earlier form, the file
//! owned by `_faber` at mode 0600 ([`Ownership::Legacy`]). Refusing it would
//! stop a host that updated unattended, so it is used and made loud instead: a
//! warning at boot and on every re-read that finds it, and
//! [`CredentialView::file_warning`] in `/api/state` and the dashboard.
//! `sudo faber setup` re-owns it ([`Store::reown_legacy`]), and `sudo faber
//! auth set` rewrites it in the current form.
//!
//! # Who else is in the group
//!
//! The file's group reads the token, so it must have no member but `_faber`
//! ([`GroupRule`]). On a host whose setup record names the operator group the
//! operator has left `_faber`'s group, and a token anyone else could read is
//! refused ([`LoadError::Exposed`]). A record from before that names none: its
//! operator is still in the group, and the token is used with a warning, the
//! shape of the earlier form above, until `sudo faber setup` moves them. Both
//! are judged at boot and at every re-read of the store, from the group
//! database's member list ([`crate::platform::group_db`]).
//!
//! A LEGACY host (no trusted setup record) has no store at all: the token used
//! to come from `gh` and the environment, and both are gone. That host claims
//! nothing until it is set up. Nothing here changes that.
//!
//! # What the token never touches
//!
//! The token is held as a [`Secret`], whose `Debug` prints a redaction and which
//! has no `Display` or `Serialize`: it cannot reach a log line, an error, the
//! audit log or `/api/state` by accident. The store writer serializes it
//! explicitly, and only there. It is never an argument, never an environment
//! variable, and never in `config.toml`.
//!
//! # What the token may do
//!
//! A fine-grained token cannot be asked for its permissions, only probed. So
//! [`required`] says which ones the enabled features need, `faber auth link`
//! asks GitHub for exactly those, and the daemon learns per repository which
//! are missing ([`Health`]): from probes of the reads, and from a permission
//! refusal ([`is_permission_refusal`]) on any call. A missing one turns off the
//! feature that needs it, for that repository, visibly ([`CredentialView`]).

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::{self, Read};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::config::{DriftMode, FileConfig, ImageBuildMode, WasteMode};
use crate::platform::layout::{CREDENTIALS_DIR, FABER_ACCOUNT, FABER_OPERATOR_GROUP};
use crate::setup::fsx::{self, Account};

// --- the secret -------------------------------------------------------------

/// A token. Its `Debug` is a redaction, it has no `Display` or `Serialize`, and
/// its bytes are overwritten when it is dropped (best effort: a copy in a
/// request header outlives it).
#[derive(Clone)]
pub struct Secret(String);

impl Secret {
    pub fn new(token: String) -> Self {
        Self(token)
    }

    /// The token itself, for the one place that needs it: an `Authorization`
    /// header, or the store's own writer.
    pub fn expose(&self) -> &str {
        &self.0
    }

    /// Whether this is the same token as `other`, without exposing either.
    pub fn same_as(&self, other: &Secret) -> bool {
        self.0 == other.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // SAFETY: every byte is replaced by NUL, which is valid UTF-8, so the
        // string stays a valid `String` for the rest of its drop.
        let bytes = unsafe { self.0.as_mut_vec() };
        for b in bytes.iter_mut() {
            // Volatile, so the write is not elided as dead.
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

// --- what a token must look like --------------------------------------------

/// The prefix of a fine-grained personal access token.
pub const FINE_GRAINED_PREFIX: &str = "github_pat_";

/// The shortest and longest tail after [`FINE_GRAINED_PREFIX`] accepted. Real
/// ones are 82 characters; the bounds only keep out what cannot be one.
const MIN_TAIL: usize = 20;
const MAX_TAIL: usize = 255;

/// Token prefixes that are not fine-grained tokens, and what each is.
const OTHER_KINDS: [(&str, &str); 5] = [
    ("ghp_", "a classic personal access token"),
    ("gho_", "an OAuth token, the kind `gh auth token` prints"),
    ("ghu_", "a GitHub App user-to-server token"),
    ("ghs_", "a GitHub App installation token"),
    ("ghr_", "a refresh token"),
];

/// Whether `token` is shaped like a fine-grained personal access token. The
/// refusal names what was wrong, never the token.
pub fn check_shape(token: &str) -> Result<(), String> {
    if token.is_empty() {
        return Err("no token was given".to_string());
    }
    for (prefix, kind) in OTHER_KINDS {
        if token.starts_with(prefix) {
            return Err(format!(
                "this is {kind}, not a fine-grained personal access token \
                 ({FINE_GRAINED_PREFIX}…). Classic and OAuth tokens reach every repository \
                 the account can, with no per-repository grant, and faber accepts only \
                 fine-grained ones (ADR-0027). Run `faber auth link` to create one"
            ));
        }
    }
    let Some(tail) = token.strip_prefix(FINE_GRAINED_PREFIX) else {
        return Err(format!(
            "this does not look like a fine-grained personal access token: it should start \
             with {FINE_GRAINED_PREFIX}. Run `faber auth link` to create one"
        ));
    };
    if !tail.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return Err(
            "the token holds characters a GitHub token never does (a stray quote, space or \
             line break?)"
                .to_string(),
        );
    }
    if !(MIN_TAIL..=MAX_TAIL).contains(&tail.len()) {
        return Err(format!(
            "the token is {} characters long, which no fine-grained token is (was it cut \
             short when it was pasted?)",
            token.len()
        ));
    }
    Ok(())
}

/// Whether `s` is a GitHub login: 1 to 39 ASCII letters, digits and hyphens,
/// not starting with a hyphen. It becomes part of a file name and a URL, so
/// nothing else is let through.
pub fn valid_login(s: &str) -> bool {
    (1..=39).contains(&s.len())
        && !s.starts_with('-')
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

// --- the store --------------------------------------------------------------

/// The schema [`Store::save`] writes and [`Store::load`] accepts.
pub const SCHEMA: u32 = 1;

const FILE_PREFIX: &str = "github-";
const FILE_SUFFIX: &str = ".json";

/// The most of a token file that is read. A real one is a few hundred bytes.
const READ_CAP: u64 = 16 * 1024;

/// What a stored-token file is called for `owner`.
pub fn file_name(owner: &str) -> String {
    format!("{FILE_PREFIX}{owner}{FILE_SUFFIX}")
}

/// The owner a store entry is for, if `name` is a stored-token file at all.
/// `Some(Err)` for one shaped like it whose owner is not a login.
fn owner_of(name: &str) -> Option<Result<String, ()>> {
    let owner = name.strip_prefix(FILE_PREFIX)?.strip_suffix(FILE_SUFFIX)?;
    Some(if valid_login(owner) {
        Ok(owner.to_string())
    } else {
        Err(())
    })
}

/// A token read from the store, with what the store says about it.
#[derive(Debug, Clone)]
pub struct StoredToken {
    /// The account the token acts as, and the only owner faber serves.
    pub owner: String,
    pub token: Secret,
    /// When GitHub expires it, epoch ms.
    pub expires_at_ms: u64,
    /// When `sudo faber auth set` stored it, epoch ms. `/api/state` reports it,
    /// so `auth set` can tell when the daemon has picked this token up.
    pub stored_at_ms: u64,
    /// The file was in the store's earlier, `_faber`-owned form
    /// ([`Ownership::Legacy`]): what to say about it. Used all the same.
    pub file_warning: Option<String>,
}

impl StoredToken {
    pub fn meta(&self) -> Meta {
        Meta {
            owner: self.owner.clone(),
            expires_at_ms: self.expires_at_ms,
            stored_at_ms: self.stored_at_ms,
        }
    }
}

/// Why the store gave no token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// A LEGACY host: there is no store to read.
    NoStore,
    /// The store holds no token.
    Missing,
    /// The store holds something faber will not use. The reason names the
    /// file and the defect, never the token.
    Refused(String),
    /// The token file's group has a member besides `_faber`, so every process
    /// that member runs can read it ([`GroupRule::Refuse`]). The reason names
    /// the file and the members.
    Exposed(String),
}

impl LoadError {
    /// What is wrong, without what to do about it.
    pub fn defect(&self) -> String {
        match self {
            LoadError::NoStore => {
                "this host keeps no GitHub token: faber reads one only from the credential \
                 store `sudo faber setup` creates (ADR-0027)"
                    .to_string()
            }
            LoadError::Missing => "no GitHub token is stored".to_string(),
            LoadError::Refused(why) | LoadError::Exposed(why) => {
                format!("{why}, so faber will not use it")
            }
        }
    }

    /// What to run about it.
    pub fn fix(&self) -> &'static str {
        match self {
            LoadError::NoStore => {
                "run `sudo faber setup`, then `faber auth link` and `sudo faber auth set`"
            }
            LoadError::Missing => {
                "run `faber auth link`, create the token it links to on GitHub, then `sudo \
                 faber auth set`"
            }
            LoadError::Refused(_) => "re-run `sudo faber auth set`",
            LoadError::Exposed(_) => {
                "run `sudo faber setup`, which says how to take them out, log out of their \
                 sessions, then replace the token: `faber auth link`, `sudo faber auth set`, and \
                 revoke the old one on GitHub"
            }
        }
    }

    /// The sentence `/api/state` and the dashboard show: the defect and the fix.
    pub fn reason(&self) -> String {
        format!("{}: {}", self.defect(), self.fix())
    }

    /// `missing` for no token at all, `invalid` for one faber refuses.
    pub fn status(&self) -> Status {
        match self {
            LoadError::NoStore | LoadError::Missing => Status::Missing,
            LoadError::Refused(_) | LoadError::Exposed(_) => Status::Invalid,
        }
    }
}

/// What a token GitHub refuses is reported as.
pub const REJECTED: &str = "GitHub rejected the stored token: expired or revoked. Run `faber auth \
     link` (or regenerate the token on GitHub), then `sudo faber auth set`";

/// What a token taken from the store, and not yet confirmed by GitHub to act
/// as the served owner, is reported as while claims wait for that.
pub const UNCONFIRMED: &str = "a new GitHub token is being checked";

/// The file as `faber auth set` writes it. Deliberately no `Debug`.
#[derive(Deserialize)]
struct OnDisk {
    schema: u32,
    owner: String,
    token: String,
    expires_at_ms: u64,
    stored_at_ms: u64,
}

/// [`OnDisk`] for writing, borrowing rather than copying the token.
#[derive(Serialize)]
struct OnDiskRef<'a> {
    schema: u32,
    owner: &'a str,
    token: &'a str,
    expires_at_ms: u64,
    stored_at_ms: u64,
}

/// What changes when the store does: each entry's inode, mtime, mode, owner,
/// group and size, the directory's included. A token is always written by rename,
/// so a new one changes the directory and the file both.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stamp(Vec<StampEntry>);

/// One entry of a [`Stamp`]: the directory itself has an empty name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct StampEntry {
    name: String,
    ino: u64,
    mtime: i64,
    mtime_nsec: i64,
    mode: u32,
    uid: u32,
    gid: u32,
    size: u64,
}

/// What a member of the token file's group besides `_faber` means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupRule {
    /// The setup record names the operator group, so the operator reads
    /// faber's files through that and nobody belongs in `_faber`'s: the
    /// token is refused.
    Refuse,
    /// A record from before the operator group: its operator is still in
    /// `_faber`'s, by setup's own doing. The token is used, and the exposure
    /// said at every read, until `sudo faber setup` moves them. A release
    /// does not withdraw a token a host relies on (AGENTS.md).
    Warn,
}

/// Who the group database lists as a group's members, `None` for no such
/// group: [`crate::platform::group_db::members`], or a test's own.
type MembersOf = fn(u32) -> io::Result<Option<Vec<String>>>;

/// The credential store at one directory.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
    /// `_faber`: its group reads a token file. Its uid owned one in the
    /// store's earlier form ([`Ownership::Legacy`]).
    faber: Account,
    /// Who must own the directory, its parent and a token file: root.
    dir_owner: Account,
    /// What a stranger in `faber`'s group means.
    group_rule: GroupRule,
    members_of: MembersOf,
}

impl Store {
    /// The store a set-up host keeps, its files readable by `faber`'s group,
    /// which nobody else may be in.
    pub fn new(dir: PathBuf, faber: Account) -> Self {
        Self {
            dir,
            faber,
            dir_owner: fsx::ROOT,
            group_rule: GroupRule::Refuse,
            members_of: crate::platform::group_db::members,
        }
    }

    /// This host's store, or `None` on a LEGACY host, which has none.
    pub fn system() -> Option<Self> {
        let record = crate::platform::setup_record()?;
        let store = Self::new(
            PathBuf::from(CREDENTIALS_DIR),
            Account {
                uid: record.faber_uid(),
                gid: record.faber_gid(),
            },
        );
        Some(match record.operator_gid() {
            Some(_) => store,
            None => store.with_group_rule(GroupRule::Warn),
        })
    }

    /// This store, judging a stranger in `faber`'s group by `rule`.
    pub fn with_group_rule(self, rule: GroupRule) -> Self {
        Self {
            group_rule: rule,
            ..self
        }
    }

    /// A store whose directories and files `dir_owner` owns rather than root,
    /// for tests that run unprivileged. Its group lists nobody: the test's own
    /// group is not faber's.
    #[cfg(test)]
    pub(crate) fn owned_by(dir: PathBuf, faber: Account, dir_owner: Account) -> Self {
        Self {
            dir,
            faber,
            dir_owner,
            group_rule: GroupRule::Refuse,
            members_of: |_| Ok(Some(Vec::new())),
        }
    }

    /// This store, reading group members through `members_of`.
    #[cfg(test)]
    pub(crate) fn with_members(self, members_of: MembersOf) -> Self {
        Self { members_of, ..self }
    }

    /// What the members of `faber`'s group say about the token at `path`, a
    /// file of the current form, which that group reads: nothing, a warning
    /// ([`GroupRule::Warn`]), or the refusal ([`GroupRule::Refuse`]). A lookup
    /// that fails is a warning, never a refusal: the file's mode is what
    /// protects it, and this only notices a group that grew.
    fn exposure(&self, path: &Path) -> Result<Option<String>, LoadError> {
        let shown = path.display();
        let members = match (self.members_of)(self.faber.gid) {
            Ok(members) => members.unwrap_or_default(),
            Err(e) => {
                return Ok(Some(format!(
                    "could not read who is in group {FABER_ACCOUNT}, which reads {shown} ({e}); \
                     `sudo faber setup` checks it"
                )))
            }
        };
        let strangers: Vec<String> = members.into_iter().filter(|m| m != FABER_ACCOUNT).collect();
        if strangers.is_empty() {
            return Ok(None);
        }
        let (names, are) = match strangers.as_slice() {
            [one] => (one.clone(), "is"),
            many => (many.join(", "), "are"),
        };
        match self.group_rule {
            GroupRule::Refuse => Err(LoadError::Exposed(format!(
                "{names} {are} in group {FABER_ACCOUNT}, which reads {shown}: every process \
                 they run can read the token"
            ))),
            GroupRule::Warn => Ok(Some(format!(
                "{names} {are} in group {FABER_ACCOUNT}, which reads {shown}: every process \
                 they run can read the token. Run `sudo faber setup`, which moves the operator \
                 to {FABER_OPERATOR_GROUP}, log out of every session, then replace the token \
                 (ADR-0027)"
            ))),
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Why a directory on the way to the token is unfit, if it is: it must be
    /// a real directory, owned by the store's owner, and writable by nobody
    /// else, or someone else could swap the token for theirs.
    fn dir_problem(&self, path: &Path, meta: &std::fs::Metadata) -> Option<String> {
        let shown = path.display();
        if !meta.is_dir() {
            Some(format!("{shown} is not a directory"))
        } else if meta.uid() != self.dir_owner.uid {
            Some(format!(
                "{shown} is owned by uid {}, not uid {}",
                meta.uid(),
                self.dir_owner.uid
            ))
        } else if meta.mode() & 0o022 != 0 {
            Some(format!(
                "{shown} is group- or world-writable (mode {:o})",
                meta.mode() & 0o7777
            ))
        } else {
            None
        }
    }

    /// The owners the store holds a file for, sorted. A file shaped like a
    /// token file whose owner is not a login is refused.
    fn owners(&self) -> Result<Vec<String>, LoadError> {
        let entries = std::fs::read_dir(&self.dir).map_err(|e| {
            LoadError::Refused(format!("{} could not be listed ({e})", self.dir.display()))
        })?;
        let mut owners = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| {
                LoadError::Refused(format!("{} could not be listed ({e})", self.dir.display()))
            })?;
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            match owner_of(&name) {
                None => continue,
                Some(Ok(owner)) => owners.push(owner),
                Some(Err(())) => {
                    return Err(LoadError::Refused(format!(
                        "{} holds a file named like a token whose owner is not a GitHub login",
                        self.dir.display()
                    )))
                }
            }
        }
        owners.sort();
        Ok(owners)
    }

    /// Read the one stored token, checking everything about it first.
    pub fn load(&self) -> Result<StoredToken, LoadError> {
        let dir = match fsx::open_leaf_dir(&self.dir) {
            Ok(dir) => dir,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(LoadError::Missing),
            Err(e) => {
                return Err(LoadError::Refused(format!(
                    "{} could not be opened as a real directory ({e})",
                    self.dir.display()
                )))
            }
        };
        let meta = dir.metadata().map_err(|e| {
            LoadError::Refused(format!("{} could not be read ({e})", self.dir.display()))
        })?;
        if let Some(problem) = self.dir_problem(&self.dir, &meta) {
            return Err(LoadError::Refused(problem));
        }
        let owners = self.owners()?;
        let owner = match owners.as_slice() {
            [] => return Err(LoadError::Missing),
            [one] => one.clone(),
            many => {
                return Err(LoadError::Refused(format!(
                    "{} holds tokens for {} owners ({}), and faber serves one",
                    self.dir.display(),
                    many.len(),
                    many.join(", ")
                )))
            }
        };
        let name = file_name(&owner);
        let path = self.dir.join(&name);
        let file = dir.open_file(&name).map_err(|e| {
            if std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink()) {
                LoadError::Refused(format!("{} is a symlink", path.display()))
            } else {
                LoadError::Refused(format!("{} could not be opened ({e})", path.display()))
            }
        })?;
        let fmeta = file.metadata().map_err(|e| {
            LoadError::Refused(format!("{} could not be read ({e})", path.display()))
        })?;
        // The earlier form is 0600: its group reads nothing.
        let file_warning =
            match ownership(&path, FileFacts::of(&fmeta), self.dir_owner.uid, self.faber) {
                Ownership::Current => self.exposure(&path)?,
                Ownership::Legacy => Some(legacy_warning(&path)),
                Ownership::Refused(why) => return Err(LoadError::Refused(why)),
            };
        let mut bytes = Vec::new();
        file.take(READ_CAP + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| {
                LoadError::Refused(format!("{} could not be read ({e})", path.display()))
            })?;
        let parsed = parse(&bytes, &path, &owner);
        // Best effort, like `Secret`'s drop: the file's bytes held the token.
        bytes.fill(0);
        parsed.map(|token| StoredToken {
            file_warning,
            ..token
        })
    }

    /// Root's side of the store's earlier form: if the token file is still
    /// owned by `_faber` at mode 0600 ([`Ownership::Legacy`]), copy its bytes
    /// (read through the descriptor it was checked on, uninterpreted) into a
    /// NEW inode owned by `root:_faber` at mode 0640 and rename it over the
    /// old one. It is a new inode, not a chown in place, so no descriptor
    /// `_faber` opened on the old file can write the token afterwards
    /// (Unix checks permissions only at open). `Ok(Some(path))` when it
    /// rewrote that file, `Ok(None)` when there was
    /// nothing to do (no store, no token, or already in the current form). A
    /// file in any other state is left exactly as it is, and the error says
    /// why: setup does not guess at a file it did not expect.
    pub fn reown_legacy(&self) -> io::Result<Option<PathBuf>> {
        let refused = |why: String| io::Error::new(io::ErrorKind::PermissionDenied, why);
        let dir = match fsx::open_leaf_dir(&self.dir) {
            Ok(dir) => dir,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        if let Some(problem) = self.dir_problem(&self.dir, &dir.metadata()?) {
            return Err(refused(problem));
        }
        let owners = self.owners().map_err(|e| refused(e.defect()))?;
        let owner = match owners.as_slice() {
            [] => return Ok(None),
            [one] => one.clone(),
            many => {
                return Err(refused(format!(
                    "{} holds tokens for {} owners, and faber serves one",
                    self.dir.display(),
                    many.len()
                )))
            }
        };
        let name = file_name(&owner);
        let path = self.dir.join(&name);
        let file = dir.open_file(&name).map_err(|e| {
            refused(format!(
                "{} could not be opened without following a link ({e})",
                path.display()
            ))
        })?;
        let meta = file.metadata()?;
        match ownership(&path, FileFacts::of(&meta), self.dir_owner.uid, self.faber) {
            Ownership::Current => Ok(None),
            Ownership::Refused(why) => Err(refused(why)),
            Ownership::Legacy => {
                let mut bytes = Vec::new();
                let read = (&file).take(READ_CAP + 1).read_to_end(&mut bytes);
                let written = match read {
                    Err(e) => Err(e),
                    Ok(_) if bytes.len() as u64 > READ_CAP => Err(refused(format!(
                        "{} is larger than {READ_CAP} bytes",
                        path.display()
                    ))),
                    Ok(_) => dir.write_atomic(
                        &name,
                        &mut bytes.as_slice(),
                        Account {
                            uid: self.dir_owner.uid,
                            gid: self.faber.gid,
                        },
                        TOKEN_MODE,
                    ),
                };
                bytes.fill(0);
                written?;
                Ok(Some(path))
            }
        }
    }

    /// What the store looks like now, cheaply, to notice a change without
    /// reading a token. `None` when the directory is not there.
    pub fn stamp(&self) -> Option<Stamp> {
        let entry = |name: String, m: &std::fs::Metadata| StampEntry {
            name,
            ino: m.ino(),
            mtime: m.mtime(),
            mtime_nsec: m.mtime_nsec(),
            mode: m.mode(),
            uid: m.uid(),
            gid: m.gid(),
            size: m.size(),
        };
        let dm = std::fs::symlink_metadata(&self.dir).ok()?;
        let mut out = vec![entry(String::new(), &dm)];
        if let Ok(entries) = std::fs::read_dir(&self.dir) {
            for e in entries.flatten() {
                let Some(name) = e.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                if owner_of(&name).is_none() {
                    continue;
                }
                if let Ok(m) = std::fs::symlink_metadata(e.path()) {
                    out.push(entry(name, &m));
                }
            }
        }
        out.sort();
        Some(Stamp(out))
    }

    /// Store `token` for `owner`, owned by root with `_faber`'s group at mode
    /// 0640, replacing any token stored before: root writes it, `_faber` only
    /// reads it, through its group. Written atomically (a temp file given its
    /// owner and mode through its own descriptor, then renamed into place), so
    /// it also replaces a file in the store's earlier, `_faber`-owned form.
    /// Root's half of the store: the directory is created owned by root at
    /// 0755 if missing, and refused if anyone else planted it. Exactly one
    /// owner is served, so a token for any other owner is removed; their names
    /// are returned for the caller to say so.
    pub fn save(
        &self,
        owner: &str,
        token: &Secret,
        expires_at_ms: u64,
        stored_at_ms: u64,
    ) -> io::Result<Vec<String>> {
        if !valid_login(owner) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the owner is not a GitHub login",
            ));
        }
        check_shape(token.expose())
            .map_err(|why| io::Error::new(io::ErrorKind::InvalidInput, why))?;
        let (parent, name) = fsx::split(&self.dir)?;
        let parent_dir = fsx::open_leaf_dir(parent)?;
        let pmeta = parent_dir.metadata()?;
        if let Some(problem) = self.dir_problem(parent, &pmeta) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, problem));
        }
        let (dir, _) = fsx::ensure_dir(
            &parent_dir,
            name,
            self.dir_owner,
            0o755,
            &[self.dir_owner.uid],
        )?;
        let mut body = serde_json::to_vec(&OnDiskRef {
            schema: SCHEMA,
            owner,
            token: token.expose(),
            expires_at_ms,
            stored_at_ms,
        })
        .map_err(io::Error::other)?;
        body.push(b'\n');
        let file_owner = Account {
            uid: self.dir_owner.uid,
            gid: self.faber.gid,
        };
        let written = dir.write_atomic(
            &file_name(owner),
            &mut body.as_slice(),
            file_owner,
            TOKEN_MODE,
        );
        body.fill(0);
        written?;
        let mut removed = Vec::new();
        let others = self.owners().map_err(|e| io::Error::other(e.reason()))?;
        for other in others.into_iter().filter(|o| o != owner) {
            dir.unlink(&file_name(&other))?;
            removed.push(other);
        }
        Ok(removed)
    }
}

/// Parse a token file's bytes, refusing anything but what [`Store::save`]
/// writes. A parse error's own text is never shown: it can quote the file.
fn parse(bytes: &[u8], path: &Path, owner: &str) -> Result<StoredToken, LoadError> {
    if bytes.len() as u64 > READ_CAP {
        return Err(LoadError::Refused(format!(
            "{} is larger than any token file",
            path.display()
        )));
    }
    let shown = path.display();
    let on_disk: OnDisk = serde_json::from_slice(bytes).map_err(|_| {
        LoadError::Refused(format!("{shown} is not the JSON `faber auth set` writes"))
    })?;
    let token = Secret::new(on_disk.token);
    if on_disk.schema != SCHEMA {
        return Err(LoadError::Refused(format!(
            "{shown} has schema {}, and this faber reads {SCHEMA}",
            on_disk.schema
        )));
    }
    if on_disk.owner != owner {
        return Err(LoadError::Refused(format!(
            "{shown} names a different owner inside than its file name does"
        )));
    }
    if check_shape(token.expose()).is_err() {
        return Err(LoadError::Refused(format!(
            "{shown} holds something that is not a fine-grained token"
        )));
    }
    if on_disk.expires_at_ms == 0 {
        return Err(LoadError::Refused(format!("{shown} records no expiry")));
    }
    Ok(StoredToken {
        owner: on_disk.owner,
        token,
        expires_at_ms: on_disk.expires_at_ms,
        stored_at_ms: on_disk.stored_at_ms,
        file_warning: None,
    })
}

// --- who may own a token file -------------------------------------------------

/// The mode [`Store::save`] gives a token file: root reads and writes it,
/// `_faber`'s group reads it, nobody else anything.
pub const TOKEN_MODE: u32 = 0o640;

/// What a directory entry is, as [`ownership`] judges it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Regular,
    Symlink,
    /// A directory, FIFO, socket or device.
    Other,
}

/// The facts about a token file that decide whether faber uses it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFacts {
    pub kind: EntryKind,
    pub uid: u32,
    pub gid: u32,
    /// The full `st_mode`.
    pub mode: u32,
}

impl FileFacts {
    /// The facts of `meta`, which `symlink_metadata` or a descriptor's
    /// `metadata` gave: a symlink is seen as one, never followed.
    pub fn of(meta: &std::fs::Metadata) -> Self {
        let t = meta.file_type();
        Self {
            kind: if t.is_symlink() {
                EntryKind::Symlink
            } else if t.is_file() {
                EntryKind::Regular
            } else {
                EntryKind::Other
            },
            uid: meta.uid(),
            gid: meta.gid(),
            mode: meta.mode(),
        }
    }
}

/// Whether faber uses a token file, and in which form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ownership {
    /// Owned by root with `_faber`'s group, writable by root only, and
    /// readable by nobody outside that group: what [`Store::save`] writes.
    Current,
    /// The store's earlier form: owned by `_faber`, mode 0600 or tighter.
    /// Used, but `_faber` could rewrite its own token, so it is reported
    /// ([`legacy_warning`]) until `sudo faber setup` or `sudo faber auth set`
    /// makes it root's.
    Legacy,
    /// Anything else: never used. The reason names the path and the defect,
    /// never the token.
    Refused(String),
}

/// Judge a token file at `path` from its `facts`, given who root is
/// (`root_uid`) and `_faber`'s account. Pure.
pub fn ownership(path: &Path, facts: FileFacts, root_uid: u32, faber: Account) -> Ownership {
    let shown = path.display();
    let mode = facts.mode & 0o7777;
    match facts.kind {
        EntryKind::Symlink => return Ownership::Refused(format!("{shown} is a symlink")),
        EntryKind::Other => return Ownership::Refused(format!("{shown} is not a regular file")),
        EntryKind::Regular => {}
    }
    if facts.uid == root_uid {
        if facts.gid != faber.gid {
            Ownership::Refused(format!(
                "{shown} has group gid {}, not faber's (gid {})",
                facts.gid, faber.gid
            ))
        } else if mode & 0o027 != 0 {
            Ownership::Refused(format!(
                "{shown} is group-writable or open to others (mode {mode:o})"
            ))
        } else {
            Ownership::Current
        }
    } else if facts.uid == faber.uid {
        if mode & 0o077 != 0 {
            Ownership::Refused(format!(
                "{shown} is readable or writable by others (mode {mode:o})"
            ))
        } else {
            Ownership::Legacy
        }
    } else {
        Ownership::Refused(format!(
            "{shown} is owned by uid {}, neither root (uid {root_uid}) nor faber's account \
             (uid {})",
            facts.uid, faber.uid
        ))
    }
}

/// What is said about a token file in the store's earlier form.
pub fn legacy_warning(path: &Path) -> String {
    format!(
        "{} is owned by {}, so faber's own account could rewrite its token; run `sudo faber \
         setup` or `sudo faber auth set` to make it root-owned (ADR-0027)",
        path.display(),
        crate::platform::layout::FABER_ACCOUNT
    )
}

/// Every token-named entry in `dir` with its facts, a symlink seen as one:
/// read-only, for setup's plan. Empty when `dir` cannot be listed.
pub fn token_files(dir: &Path) -> Vec<(PathBuf, FileFacts)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(PathBuf, FileFacts)> = entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| owner_of(n).is_some())
        })
        .filter_map(|e| {
            let path = e.path();
            let meta = std::fs::symlink_metadata(&path).ok()?;
            Some((path, FileFacts::of(&meta)))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

// --- expiry -----------------------------------------------------------------

const DAY_MS: u64 = 24 * 60 * 60 * 1000;

/// How many days ahead an expiry is warned about (`expiring`).
pub const WARN_DAYS: u64 = 14;

/// How many days ahead it is also logged as a warning once a day.
pub const DAILY_DAYS: u64 = 3;

/// How close a token is to expiring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expiry {
    /// More than [`WARN_DAYS`] away.
    Fine,
    /// Within [`WARN_DAYS`]: `expiring`.
    Soon { days_left: u64 },
    /// Within [`DAILY_DAYS`]: `expiring`, and logged daily.
    Imminent { days_left: u64 },
    /// Already past.
    Expired,
}

/// How close `expires_at_ms` is, at `now_ms`. Days left round up, so a token
/// with an hour to go has 1 day left, not 0.
pub fn expiry(expires_at_ms: u64, now_ms: u64) -> Expiry {
    if expires_at_ms <= now_ms {
        return Expiry::Expired;
    }
    let days_left = (expires_at_ms - now_ms).div_ceil(DAY_MS);
    if days_left <= DAILY_DAYS {
        Expiry::Imminent { days_left }
    } else if days_left <= WARN_DAYS {
        Expiry::Soon { days_left }
    } else {
        Expiry::Fine
    }
}

/// The UTC date of `ms`, as `YYYY-MM-DD`.
pub fn date_of(ms: u64) -> String {
    let s = crate::setup::stamp(ms / 1000);
    format!("{}-{}-{}", &s[0..4], &s[4..6], &s[6..8])
}

/// Parse GitHub's `github-authentication-token-expiration` header, which reads
/// like `2026-12-31 23:59:59 UTC` (or with a `+HHMM` offset), to epoch ms.
pub fn parse_expiry_header(value: &str) -> Option<u64> {
    let mut parts = value.split_whitespace();
    let (date, time, zone) = (parts.next()?, parts.next()?, parts.next().unwrap_or("UTC"));
    if parts.next().is_some() || date.len() != 10 || time.len() != 8 {
        return None;
    }
    let local = crate::retry::parse_rfc3339_to_ms(&format!("{date}T{time}Z"))?;
    let offset_ms: i64 = match zone {
        "UTC" | "GMT" | "Z" => 0,
        z if z.len() == 5 && (z.starts_with('+') || z.starts_with('-')) => {
            let hh: i64 = z[1..3].parse().ok()?;
            let mm: i64 = z[3..5].parse().ok()?;
            if hh > 23 || mm > 59 {
                return None;
            }
            let sign = if z.starts_with('-') { -1 } else { 1 };
            sign * (hh * 3600 + mm * 60) * 1000
        }
        _ => return None,
    };
    u64::try_from(local as i64 - offset_ms).ok()
}

/// Parse a `YYYY-MM-DD` date, as `--expires` and the prompt take it, to the
/// start of that day in UTC: the earlier end, so the warnings come no later.
pub fn parse_date(value: &str) -> Option<u64> {
    let v = value.trim();
    let b = v.as_bytes();
    let shaped = b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && b.iter()
            .enumerate()
            .all(|(i, c)| i == 4 || i == 7 || c.is_ascii_digit());
    if !shaped {
        return None;
    }
    let ms = crate::retry::parse_rfc3339_to_ms(&format!("{v}T00:00:00Z"))?;
    // A date that rolls over (`2026-02-31`) is not the date it says.
    (date_of(ms) == v).then_some(ms)
}

// --- permissions ------------------------------------------------------------

/// A permission a fine-grained token can carry, at the level faber uses it.
/// Ordered, so lists of them come out in a stable order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Permission {
    ActionsRead,
    ActionsWrite,
    /// Read and write: every runner endpoint needs it, and write cannot be
    /// probed without side effects, so the read probe stands in for it.
    Administration,
    ChecksRead,
    ContentsRead,
    /// What the gate's read of a merge queue is taken to need (ADR-0059 §3),
    /// over GraphQL. ADR-0059's *Validation* is to confirm it is this box.
    MergeQueuesRead,
    PullRequestsRead,
    StatusesWrite,
}

impl Permission {
    /// The permission's name in GitHub's token template link.
    pub fn github_name(self) -> &'static str {
        match self {
            Permission::ActionsRead | Permission::ActionsWrite => "actions",
            Permission::Administration => "administration",
            Permission::ChecksRead => "checks",
            Permission::ContentsRead => "contents",
            Permission::MergeQueuesRead => "merge_queues",
            Permission::PullRequestsRead => "pull_requests",
            Permission::StatusesWrite => "statuses",
        }
    }

    /// `read` or `write`, as the template link spells it.
    pub fn level(self) -> &'static str {
        match self {
            Permission::ActionsWrite | Permission::Administration | Permission::StatusesWrite => {
                "write"
            }
            _ => "read",
        }
    }

    /// How a person reads it: `Actions: read`.
    pub fn label(self) -> &'static str {
        match self {
            Permission::ActionsRead => "Actions: read",
            Permission::ActionsWrite => "Actions: write",
            Permission::Administration => "Administration: write",
            Permission::ChecksRead => "Checks: read",
            Permission::ContentsRead => "Contents: read",
            Permission::MergeQueuesRead => "Merge queues: read",
            Permission::PullRequestsRead => "Pull requests: read",
            Permission::StatusesWrite => "Commit statuses: write",
        }
    }

    /// Whether a fine-grained token can be granted it. Checks: read reportedly
    /// cannot, so the link leaves it out and the features that need it stay off
    /// where GitHub does not let it be read anyway.
    pub fn grantable(self) -> bool {
        self != Permission::ChecksRead
    }

    /// Whether faber can only learn it is held by using it.
    pub fn is_write(self) -> bool {
        self.level() == "write"
    }

    /// What to do about it being missing.
    pub fn fix(self, feature: Feature) -> String {
        if self.grantable() {
            format!(
                "tick {} on the token's page on GitHub — fine-grained permissions can be \
                 edited without a new token",
                self.label()
            )
        } else {
            format!(
                "fine-grained tokens cannot currently be granted {}, so {} stays off for this \
                 repository",
                self.label(),
                feature.label()
            )
        }
    }
}

/// A faber feature that needs a permission.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Feature {
    /// Claiming jobs and minting runners: every host.
    Core,
    /// The gate's check of whose code a job runs (ADR-0059 §3): every host.
    Gate,
    AutoRetry,
    DriftDetection,
    GitMirror,
    RunnerImageVariants,
    RunnerRetirement,
    /// The likely-wasted claim check (ADR-0063 §7).
    WasteCheck,
}

impl Feature {
    pub fn label(self) -> &'static str {
        match self {
            Feature::Core => "running jobs",
            Feature::Gate => "the gate's check of whose code a job runs (ADR-0059)",
            Feature::AutoRetry => "auto-retry (ADR-0003)",
            Feature::DriftDetection => "drift detection (ADR-0001)",
            Feature::GitMirror => "the git mirror (ADR-0035)",
            Feature::RunnerImageVariants => "runner-image variants (ADR-0008)",
            Feature::RunnerRetirement => "runner-retirement warnings",
            Feature::WasteCheck => "the waste check (ADR-0063)",
        }
    }
}

/// One permission one feature needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Need {
    pub permission: Permission,
    pub feature: Feature,
}

impl Need {
    fn new(permission: Permission, feature: Feature) -> Self {
        Self {
            permission,
            feature,
        }
    }
}

/// What the features `cfg` turns on need of the token (ADR-0027's table, plus
/// what the code shows the table left out: Pull requests: read for the git
/// mirror's refspec, Contents: read for runner-image variants, and Checks: read
/// for the runner-retirement warning every host samples). Every host needs Pull
/// requests: read for the gate's read of a pull request's author (ADR-0059,
/// amending ADR-0027's *Always* row).
pub fn required(cfg: &FileConfig) -> Vec<Need> {
    use Feature::*;
    use Permission::*;
    let mut needs = vec![
        Need::new(ActionsRead, Core),
        Need::new(Administration, Core),
        Need::new(PullRequestsRead, Gate),
        Need::new(ChecksRead, RunnerRetirement),
    ];
    if cfg.retry.enabled {
        needs.push(Need::new(ActionsWrite, AutoRetry));
    }
    if cfg.playwright_drift.mode != DriftMode::Off {
        needs.push(Need::new(ContentsRead, DriftDetection));
        needs.push(Need::new(StatusesWrite, DriftDetection));
    }
    if cfg.runner_image.build == ImageBuildMode::OnDemand {
        needs.push(Need::new(ContentsRead, RunnerImageVariants));
    }
    if cfg.profiles.iter().any(|p| p.git_mirror) {
        needs.push(Need::new(ContentsRead, GitMirror));
        needs.push(Need::new(PullRequestsRead, GitMirror));
    }
    // ADR-0063 §7. A missing one only turns its signals into "not fired".
    let waste = &cfg.waste;
    if waste.reads_pull() {
        needs.push(Need::new(PullRequestsRead, WasteCheck));
    }
    if waste.run_failed != WasteMode::Off {
        needs.push(Need::new(ActionsRead, WasteCheck));
    }
    if waste.skips() {
        needs.push(Need::new(ActionsWrite, WasteCheck));
    }
    if waste.conflicted == WasteMode::Skip {
        needs.push(Need::new(StatusesWrite, WasteCheck));
    }
    needs
}

/// Reads the gate makes only for some jobs, without the token being asked for
/// them (ADR-0059 §3): a commit's ancestry, for a merge group that left its
/// queue, and a merge queue's entries, whose box ADR-0059's *Validation* is to
/// confirm. A refusal of one names the gate, which then keeps the verdict it
/// gave before on a private repository; `faber auth link` does not ask for them.
pub const OPTIONAL_READS: &[Need] = &[
    Need {
        permission: Permission::ContentsRead,
        feature: Feature::Gate,
    },
    Need {
        permission: Permission::MergeQueuesRead,
        feature: Feature::Gate,
    },
];

/// The permissions in `needs` the daemon probes with a read on each served
/// repository ([`crate::github::Github::probe`]), in order.
pub fn probed(needs: &[Need]) -> Vec<Permission> {
    let mut out: Vec<Permission> = needs
        .iter()
        .map(|n| n.permission)
        .filter(|p| {
            matches!(
                p,
                Permission::ActionsRead
                    | Permission::Administration
                    | Permission::ContentsRead
                    | Permission::PullRequestsRead
            )
        })
        .collect();
    out.sort();
    out.dedup();
    out
}

/// What the token's repositories and the opted-in ones disagree about, for
/// `owner`'s repositories: granted but not opted in, and opted in but not
/// granted, or granted without the Administration access minting needs.
pub fn mismatches(
    owner: &str,
    granted: &[String],
    opted_in: &[String],
    cannot_administer: impl Fn(&str) -> bool,
) -> Mismatches {
    let owned = |r: &&str| {
        r.split_once('/')
            .is_some_and(|(o, _)| o.eq_ignore_ascii_case(owner))
    };
    let granted: BTreeSet<&str> = granted.iter().map(String::as_str).filter(owned).collect();
    let opted: BTreeSet<&str> = opted_in.iter().map(String::as_str).collect();
    Mismatches {
        granted_not_opted_in: granted
            .iter()
            .filter(|r| !opted.contains(*r))
            .map(|r| r.to_string())
            .collect(),
        opted_in_not_granted: opted
            .iter()
            .filter(|r| !granted.contains(*r) || cannot_administer(r))
            .map(|r| r.to_string())
            .collect(),
    }
}

/// The features `needs` lists for `permission`, in order.
pub fn features_needing(needs: &[Need], permission: Permission) -> Vec<Feature> {
    let mut out: Vec<Feature> = needs
        .iter()
        .filter(|n| n.permission == permission)
        .map(|n| n.feature)
        .collect();
    out.sort();
    out.dedup();
    out
}

/// The lifetime `faber auth link` asks for unless told otherwise.
pub const DEFAULT_DAYS: u32 = 90;

/// GitHub's range for a fine-grained token's lifetime. "Never" is refused.
pub const DAYS: std::ops::RangeInclusive<u32> = 1..=366;

/// Percent-encode `s` for a query value: everything but RFC 3986's unreserved
/// characters.
fn encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The GitHub link that pre-fills a new fine-grained token for this host: its
/// name and description, the owner when known, the lifetime, and one parameter
/// per grantable permission `needs` lists, at the highest level any need asks
/// for. Repositories cannot be pre-selected: picking them on GitHub's own page
/// is the grant.
pub fn link_url(host: &str, owner: Option<&str>, days: u32, needs: &[Need]) -> String {
    let name = format!("faber-{host}");
    let description =
        format!("faber on {host}: runs the jobs of the repositories picked here (ADR-0027)");
    let mut url = format!(
        "https://github.com/settings/personal-access-tokens/new?name={}&description={}",
        encode(&name),
        encode(&description)
    );
    if let Some(owner) = owner.filter(|o| valid_login(o)) {
        url.push_str(&format!("&target_name={}", encode(owner)));
    }
    url.push_str(&format!("&expires_in={days}"));
    let mut levels: BTreeMap<&'static str, &'static str> = BTreeMap::new();
    for need in needs.iter().filter(|n| n.permission.grantable()) {
        let level = levels
            .entry(need.permission.github_name())
            .or_insert("read");
        if need.permission.level() == "write" {
            *level = "write";
        }
    }
    for (name, level) in levels {
        url.push_str(&format!("&{name}={level}"));
    }
    url
}

/// Whether an answer is GitHub refusing the token a permission, rather than a
/// rate limit or anything else: a `403` saying the resource is not accessible,
/// with requests still left in the rate-limit window.
pub fn is_permission_refusal(status: u16, rate_limit_exhausted: bool, body: &str) -> bool {
    status == 403
        && !rate_limit_exhausted
        && !body.to_ascii_lowercase().contains("rate limit")
        && body.contains("Resource not accessible")
}

// --- the daemon's view of its credential ------------------------------------

/// The credential's state, as `/api/state` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    /// No token: nothing is claimed.
    Missing,
    /// A token, in use.
    Stored,
    /// A token faber will not or cannot use: nothing new is claimed.
    Invalid,
    /// A token in use that expires within [`WARN_DAYS`].
    Expiring,
}

/// A permission a served repository lacks, and what that costs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MissingPermission {
    pub repo: String,
    pub permission: String,
    pub feature: String,
    pub fix: String,
}

/// What the token's repository list and the `faber` topic disagree about.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Mismatches {
    /// Granted to the token but not opted in: remove it from the token. The
    /// only way an over-broad token shows, since faber cannot read its grant.
    pub granted_not_opted_in: Vec<String>,
    /// Opted in, but the token cannot administer it, so nothing runs here.
    pub opted_in_not_granted: Vec<String>,
}

/// `credential` in `/api/state`, in the stub and the daemon alike. Never the
/// token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CredentialView {
    pub status: Status,
    pub owner: Option<String>,
    pub expires_at_ms: Option<u64>,
    pub stored_at_ms: Option<u64>,
    /// Why, for anything but a plain `stored`.
    pub reason: Option<String>,
    pub missing_permissions: Vec<MissingPermission>,
    /// Write permissions the enabled features need that no write has proved
    /// yet: a write cannot be probed without side effects.
    pub unverified_permissions: Vec<String>,
    pub mismatches: Mismatches,
    /// The token file is in the store's earlier, `_faber`-owned form
    /// ([`Ownership::Legacy`]), or its group has members besides `_faber` on a
    /// host set up before the operator group ([`GroupRule::Warn`]): what to
    /// run about it. It does not block claims; the status stays `stored` or
    /// `expiring`.
    pub file_warning: Option<String>,
}

impl CredentialView {
    /// No usable token, for `why`.
    pub fn unusable(err: &LoadError) -> Self {
        Self {
            status: err.status(),
            owner: None,
            expires_at_ms: None,
            stored_at_ms: None,
            reason: Some(err.reason()),
            missing_permissions: Vec::new(),
            unverified_permissions: Vec::new(),
            mismatches: Mismatches::default(),
            file_warning: None,
        }
    }

    /// A stored token described by `meta`, made `invalid` by `problem`.
    pub fn of(meta: &Meta, problem: Option<String>, now_ms: u64) -> Self {
        let (status, reason) = match problem {
            Some(why) => (Status::Invalid, Some(why)),
            None => match expiry(meta.expires_at_ms, now_ms) {
                Expiry::Fine => (Status::Stored, None),
                Expiry::Soon { days_left } | Expiry::Imminent { days_left } => {
                    (Status::Expiring, Some(expiring_text(meta, days_left)))
                }
                Expiry::Expired => (
                    Status::Invalid,
                    Some(format!(
                        "the stored token expired on {}: {}",
                        date_of(meta.expires_at_ms),
                        ROTATE
                    )),
                ),
            },
        };
        Self {
            status,
            owner: Some(meta.owner.clone()),
            expires_at_ms: Some(meta.expires_at_ms),
            stored_at_ms: Some(meta.stored_at_ms),
            reason,
            missing_permissions: Vec::new(),
            unverified_permissions: Vec::new(),
            mismatches: Mismatches::default(),
            file_warning: None,
        }
    }

    /// Whether faber must claim nothing new on this credential.
    pub fn blocks_claims(&self) -> bool {
        matches!(self.status, Status::Missing | Status::Invalid)
    }
}

/// How to rotate a token.
const ROTATE: &str = "use \"Regenerate token\" on its page on GitHub, which keeps its \
     repositories and permissions, then run `sudo faber auth set`";

fn expiring_text(meta: &Meta, days_left: u64) -> String {
    format!(
        "the GitHub token expires in {days_left} day{} ({}): {ROTATE}",
        if days_left == 1 { "" } else { "s" },
        date_of(meta.expires_at_ms)
    )
}

/// What `/api/state` says about a stored token beyond the token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub owner: String,
    pub expires_at_ms: u64,
    pub stored_at_ms: u64,
}

/// Why a stored token is `invalid` while the daemon runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Problem {
    /// GitHub answered `401`, and re-reading the store did not change that.
    Rejected,
    /// The store now holds a token for another owner. Discovery is scoped to
    /// the owner fixed at boot, so only a restart can serve it.
    OwnerChanged { stored: String, serving: String },
    /// GitHub says the stored token acts as another account than the owner
    /// this daemon serves, though the store files it under that owner. A
    /// restart would not help: only a token for the served owner does.
    ActsAsOther { acts_as: String, serving: String },
    /// The store changed into something faber will not use.
    Unusable(String),
}

impl Problem {
    pub fn reason(&self) -> String {
        match self {
            Problem::Rejected => REJECTED.to_string(),
            Problem::OwnerChanged { stored, serving } => format!(
                "the stored token belongs to a different owner ({stored}, not {serving}); \
                 restart faber to serve it"
            ),
            Problem::ActsAsOther { acts_as, serving } => format!(
                "GitHub says the stored token acts as {acts_as}, but this daemon serves \
                 {serving}; re-run `sudo faber auth set` with a token for {serving}"
            ),
            Problem::Unusable(why) => why.clone(),
        }
    }
}

/// How long a permission found missing is treated as missing before the next
/// call that needs it is let through to test it again. Fine-grained
/// permissions are edited on GitHub without a new token, so nothing on this
/// host changes when the operator fixes one.
pub const RECHECK_MS: u64 = 60 * 60 * 1000;

/// A permission found missing for a repository.
#[derive(Debug, Clone)]
struct Missing {
    since_ms: u64,
}

/// What the daemon knows about its credential: the token's metadata, whether
/// it is usable, and which permissions which repositories lack. Held by the
/// [`crate::github::Github`] client, which is where every answer arrives.
#[derive(Debug, Default)]
pub struct Health {
    meta: Option<Meta>,
    stamp: Option<Stamp>,
    /// The token in use was read from a file in the store's earlier form.
    file_warning: Option<String>,
    problem: Option<Problem>,
    /// GitHub's word that the token in use acts as another account than the
    /// served owner ([`Problem::ActsAsOther`]). Kept apart from `problem`,
    /// which a store that briefly holds something else overwrites, so that
    /// nothing short of GitHub confirming a token for the served owner lifts
    /// it: not re-reading the same token, not a different one either.
    acts_as_other: Option<Problem>,
    /// A token was taken from the store that GitHub has not yet been asked
    /// about (`GET /user`).
    unconfirmed: bool,
    /// GitHub's last answer to the token in use was a `401`. Kept apart from
    /// `problem`, which a standing [`Problem::ActsAsOther`] or
    /// [`Problem::OwnerChanged`] holds instead, so that a rejected token is
    /// throttled whatever else is wrong with it. Ended by GitHub accepting a
    /// request, or by a different token.
    token_rejected: bool,
    needs: Vec<Need>,
    missing: BTreeMap<(String, Permission), Missing>,
    /// Write permissions some write has proved.
    proved: BTreeSet<Permission>,
    /// `(repo, permission)` pairs already warned about.
    warned: BTreeSet<(String, Permission)>,
    mismatches: Mismatches,
    /// The day (epoch days) the imminent-expiry warning was last logged.
    expiry_warned_day: Option<u64>,
}

impl Health {
    /// The health of a token just read from the store.
    pub fn of(token: &StoredToken, stamp: Option<Stamp>) -> Self {
        Self {
            meta: Some(token.meta()),
            stamp,
            file_warning: token.file_warning.clone(),
            ..Self::default()
        }
    }

    /// What is said about the token file's form, if it is the earlier one.
    pub fn file_warning(&self) -> Option<&str> {
        self.file_warning.as_deref()
    }

    pub fn meta(&self) -> Option<&Meta> {
        self.meta.as_ref()
    }

    pub fn stamp(&self) -> Option<&Stamp> {
        self.stamp.as_ref()
    }

    pub fn problem(&self) -> Option<&Problem> {
        self.problem.as_ref()
    }

    /// GitHub's standing word that the token acts as another account, which
    /// outlives a re-read of the store ([`Self::confirmed`] ends it).
    pub fn acts_as_other(&self) -> Option<&Problem> {
        self.acts_as_other.as_ref()
    }

    /// Whether a token taken from the store still waits for GitHub to say
    /// whom it acts as.
    pub fn unconfirmed(&self) -> bool {
        self.unconfirmed
    }

    /// A token GitHub has not been asked about yet was taken.
    pub fn set_unconfirmed(&mut self) {
        self.unconfirmed = true;
    }

    /// Whether GitHub's last answer to the token in use was a `401`, whatever
    /// problem stands besides.
    pub fn token_rejected(&self) -> bool {
        self.token_rejected
    }

    /// GitHub says the token in use acts as the served owner. `true` when that
    /// ends an [`Problem::ActsAsOther`] block.
    pub fn confirmed(&mut self) -> bool {
        self.unconfirmed = false;
        if self.acts_as_other.take().is_none() {
            return false;
        }
        if matches!(self.problem, Some(Problem::ActsAsOther { .. })) {
            self.problem = None;
        }
        true
    }

    pub fn needs(&self) -> &[Need] {
        &self.needs
    }

    pub fn set_needs(&mut self, needs: Vec<Need>) {
        self.needs = needs;
    }

    /// A new token from the store: its metadata, and a clean slate for what it
    /// may do. `true` when it differs from the one before.
    pub fn replace(&mut self, token: &StoredToken, stamp: Option<Stamp>) -> bool {
        let meta = token.meta();
        let changed = self.meta.as_ref() != Some(&meta);
        self.meta = Some(meta);
        self.stamp = stamp;
        self.file_warning = token.file_warning.clone();
        self.problem = None;
        self.acts_as_other = None;
        // The caller re-applies a rejection when the token is the same one.
        self.token_rejected = false;
        self.missing.clear();
        self.proved.clear();
        self.warned.clear();
        self.mismatches = Mismatches::default();
        changed
    }

    /// The store changed into something not used; the token in memory stays,
    /// so that running jobs can be wound down, but nothing new is claimed.
    pub fn set_problem(&mut self, problem: Problem, stamp: Option<Stamp>) -> bool {
        self.stamp = stamp;
        let changed = self.problem.as_ref() != Some(&problem);
        if matches!(problem, Problem::ActsAsOther { .. }) {
            // GitHub just answered for the token in use.
            self.acts_as_other = Some(problem.clone());
            self.unconfirmed = false;
        }
        self.problem = Some(problem);
        changed
    }

    /// GitHub answered `401` to the token. `true` when that is news. A standing
    /// owner change or [`Problem::ActsAsOther`] keeps its reason, but the
    /// rejection is recorded all the same ([`Self::token_rejected`]).
    pub fn rejected(&mut self) -> bool {
        self.token_rejected = true;
        match self.problem {
            Some(Problem::OwnerChanged { .. })
            | Some(Problem::ActsAsOther { .. })
            | Some(Problem::Rejected) => false,
            _ => {
                self.problem = Some(Problem::Rejected);
                true
            }
        }
    }

    /// GitHub accepted the token. `true` when that ends a rejection.
    pub fn accepted(&mut self) -> bool {
        self.token_rejected = false;
        if self.problem == Some(Problem::Rejected) {
            self.problem = None;
            true
        } else {
            false
        }
    }

    /// GitHub refused `permission` on `repo`. The warning to log, the first
    /// time this pair is refused.
    pub fn refused(&mut self, repo: &str, permission: Permission, now_ms: u64) -> Option<String> {
        let key = (repo.to_string(), permission);
        self.missing
            .insert(key.clone(), Missing { since_ms: now_ms });
        if !self.warned.insert(key) {
            return None;
        }
        let features = self.features_of(permission);
        let feature = features.first().copied().unwrap_or(Feature::Core);
        let off: Vec<&str> = features.iter().map(|f| f.label()).collect();
        let what = match off.len() {
            0 => "the call that needs it is".to_string(),
            1 => format!("{} is", off[0]),
            _ => format!("{} are", off.join(" and ")),
        };
        Some(format!(
            "GitHub token lacks {} on {repo}: {what} off for this repository; {}",
            permission.label(),
            permission.fix(feature)
        ))
    }

    /// The features a refusal of `permission` turns off: those the enabled
    /// features need, and the gate's for an [`OPTIONAL_READS`] read.
    fn features_of(&self, permission: Permission) -> Vec<Feature> {
        let needs: Vec<Need> = self.needs.iter().chain(OPTIONAL_READS).copied().collect();
        features_needing(&needs, permission)
    }

    /// A call needing `permission` on `repo` succeeded.
    pub fn permitted(&mut self, repo: &str, permission: Permission) {
        self.missing.remove(&(repo.to_string(), permission));
        self.warned.remove(&(repo.to_string(), permission));
        if permission.is_write() {
            self.proved.insert(permission);
        }
    }

    /// Whether to skip work on `repo` that needs `permission`: it was refused
    /// less than [`RECHECK_MS`] ago. After that one call is let through to
    /// test it again, and a refusal re-arms it.
    pub fn blocked(&self, repo: &str, permission: Permission, now_ms: u64) -> bool {
        self.missing
            .get(&(repo.to_string(), permission))
            .is_some_and(|m| now_ms.saturating_sub(m.since_ms) < RECHECK_MS)
    }

    pub fn set_mismatches(&mut self, mismatches: Mismatches) {
        self.mismatches = mismatches;
    }

    /// The warning to log about an expiry within [`DAILY_DAYS`], at most once
    /// a day.
    pub fn daily_expiry_warning(&mut self, now_ms: u64) -> Option<String> {
        let meta = self.meta.as_ref()?;
        let text = match expiry(meta.expires_at_ms, now_ms) {
            Expiry::Imminent { days_left } => expiring_text(meta, days_left),
            Expiry::Expired => format!(
                "the stored GitHub token expired on {}, so nothing new is claimed: {ROTATE}",
                date_of(meta.expires_at_ms)
            ),
            Expiry::Fine | Expiry::Soon { .. } => return None,
        };
        let day = now_ms / DAY_MS;
        if self.expiry_warned_day == Some(day) {
            return None;
        }
        self.expiry_warned_day = Some(day);
        Some(text)
    }

    /// `credential` for `/api/state`.
    pub fn view(&self, now_ms: u64) -> CredentialView {
        let Some(meta) = &self.meta else {
            return CredentialView::unusable(&LoadError::Missing);
        };
        let mut view = CredentialView::of(meta, self.problem.as_ref().map(Problem::reason), now_ms);
        view.missing_permissions = self
            .missing
            .keys()
            .map(|(repo, permission)| {
                let feature = self
                    .features_of(*permission)
                    .first()
                    .copied()
                    .unwrap_or(Feature::Core);
                MissingPermission {
                    repo: repo.clone(),
                    permission: permission.label().to_string(),
                    feature: feature.label().to_string(),
                    fix: permission.fix(feature),
                }
            })
            .collect();
        let mut unverified: Vec<Permission> = self
            .needs
            .iter()
            .map(|n| n.permission)
            .filter(|p| p.is_write() && !self.proved.contains(p))
            .collect();
        unverified.sort();
        unverified.dedup();
        view.unverified_permissions = unverified
            .into_iter()
            .map(|p| p.label().to_string())
            .collect();
        view.mismatches = self.mismatches.clone();
        view.file_warning = self.file_warning.clone();
        view
    }
}

/// The `stored_at_ms` of the stored token GitHub last rejected, process-wide,
/// so standby's stub can say `invalid` rather than `stored` for it.
static REJECTED_STORED_AT: Mutex<Option<u64>> = Mutex::new(None);

/// Note that GitHub rejected the token stored at `stored_at_ms`.
pub fn note_rejected(stored_at_ms: u64) {
    *REJECTED_STORED_AT.lock().unwrap() = Some(stored_at_ms);
}

/// `credential` in standby's `/api/state`: read from the store each time it is
/// asked, so `sudo faber auth set` shows at once.
pub fn standby_view(now_ms: u64) -> CredentialView {
    let Some(store) = Store::system() else {
        return CredentialView::unusable(&LoadError::NoStore);
    };
    match store.load() {
        Err(e) => CredentialView::unusable(&e),
        Ok(token) => {
            let rejected = *REJECTED_STORED_AT.lock().unwrap() == Some(token.stored_at_ms);
            let mut view = CredentialView::of(
                &token.meta(),
                rejected.then(|| Problem::Rejected.reason()),
                now_ms,
            );
            view.file_warning = token.file_warning.clone();
            view
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::fsx::tests::{me, Scratch};

    /// A token of the right shape, assembled at run time so no line of this
    /// file looks like one (`tests/privacy.rs`).
    fn fake_token(fill: char) -> String {
        format!("{FINE_GRAINED_PREFIX}{}", fill.to_string().repeat(40))
    }

    #[test]
    fn a_secret_never_prints_itself() {
        let secret = Secret::new(fake_token('A'));
        let shown = format!("{secret:?}");
        assert_eq!(shown, "Secret(<redacted>)");
        assert!(!shown.contains(FINE_GRAINED_PREFIX));
        let stored = StoredToken {
            owner: "owner".into(),
            token: secret.clone(),
            expires_at_ms: 1,
            stored_at_ms: 2,
            file_warning: None,
        };
        assert!(!format!("{stored:?}").contains(FINE_GRAINED_PREFIX));
        assert!(secret.same_as(&Secret::new(fake_token('A'))));
        assert!(!secret.same_as(&Secret::new(fake_token('B'))));
    }

    #[test]
    fn only_fine_grained_tokens_are_accepted() {
        assert_eq!(check_shape(&fake_token('a')), Ok(()));
        let with_tail = |tail: String| format!("{FINE_GRAINED_PREFIX}{tail}");
        assert!(check_shape(&with_tail("a_b9".repeat(10))).is_ok());
        for (prefix, kind) in OTHER_KINDS {
            let token = format!("{prefix}{}", "x".repeat(36));
            let why = check_shape(&token).unwrap_err();
            assert!(why.contains(kind), "{why}");
            assert!(why.contains("fine-grained"), "{why}");
            assert!(!why.contains(&token), "the refusal never quotes the token");
        }
        let refused = [
            String::new(),
            "x".repeat(50),
            with_tail("short".into()),
            with_tail("a".repeat(MAX_TAIL + 1)),
            with_tail(format!("{} ", "a".repeat(30))),
            with_tail(format!("{}\"", "a".repeat(30))),
            with_tail(format!("{}-x", "a".repeat(30))),
        ];
        for token in refused {
            let why = check_shape(&token).unwrap_err();
            assert!(token.is_empty() || !why.contains(&token), "{why}");
        }
    }

    #[test]
    fn a_login_is_letters_digits_and_inner_hyphens() {
        let longest = "a".repeat(39);
        let too_long = "a".repeat(40);
        for ok in ["owner", "a", "Owner-2", longest.as_str()] {
            assert!(valid_login(ok), "{ok}");
        }
        for bad in [
            "",
            "-owner",
            "own/er",
            "own.er",
            "own er",
            "../x",
            too_long.as_str(),
        ] {
            assert!(!valid_login(bad), "{bad}");
        }
        assert_eq!(owner_of("github-owner.json"), Some(Ok("owner".into())));
        assert_eq!(owner_of("github-.json"), Some(Err(())));
        assert_eq!(owner_of("setup.json"), None);
    }

    /// A store in a scratch directory, owned by this process, as root and
    /// `_faber` would own the real one.
    fn scratch_store(s: &Scratch) -> (Store, Account) {
        use std::os::unix::fs::PermissionsExt;
        let me = me(s);
        std::fs::create_dir(s.path("etc")).unwrap();
        // Whatever the umask, the parent must be writable by its owner only.
        std::fs::set_permissions(s.path("etc"), std::fs::Permissions::from_mode(0o755)).unwrap();
        (Store::owned_by(s.path("etc/credentials"), me, me), me)
    }

    #[test]
    fn a_stored_token_reads_back() {
        let s = Scratch::new("cred-roundtrip");
        let (store, me) = scratch_store(&s);
        assert_eq!(store.load().unwrap_err(), LoadError::Missing);
        assert_eq!(store.stamp(), None);
        let token = Secret::new(fake_token('Q'));
        let removed = store.save("owner", &token, 2_000, 1_000).unwrap();
        assert!(removed.is_empty());
        let back = store.load().expect("reads back");
        assert_eq!(back.owner, "owner");
        assert!(back.token.same_as(&token));
        assert_eq!((back.expires_at_ms, back.stored_at_ms), (2_000, 1_000));
        assert_eq!(
            back.file_warning, None,
            "what save writes is the current form"
        );
        let meta = std::fs::metadata(s.path("etc/credentials/github-owner.json")).unwrap();
        assert_eq!(meta.mode() & 0o777, 0o640);
        assert_eq!((meta.uid(), meta.gid()), (me.uid, me.gid));
        let dmeta = std::fs::metadata(s.path("etc/credentials")).unwrap();
        assert_eq!(dmeta.mode() & 0o777, 0o755);
        assert!(store.stamp().is_some());
    }

    /// One owner is served: storing another's token removes the first.
    #[test]
    fn storing_another_owners_token_replaces_the_first() {
        let s = Scratch::new("cred-owner");
        let (store, _) = scratch_store(&s);
        let first = store.stamp();
        store
            .save("first", &Secret::new(fake_token('A')), 9, 1)
            .unwrap();
        let after_first = store.stamp();
        assert_ne!(first, after_first);
        let removed = store
            .save("second", &Secret::new(fake_token('B')), 9, 2)
            .unwrap();
        assert_eq!(removed, vec!["first".to_string()]);
        assert_eq!(store.load().unwrap().owner, "second");
        assert_ne!(store.stamp(), after_first, "a new token changes the stamp");
    }

    fn refused(store: &Store) -> String {
        match store.load() {
            Err(LoadError::Refused(why)) => why,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_token_file_anyone_else_could_read_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let s = Scratch::new("cred-mode");
        let (store, _) = scratch_store(&s);
        store
            .save("owner", &Secret::new(fake_token('A')), 9, 1)
            .unwrap();
        let file = s.path("etc/credentials/github-owner.json");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(refused(&store).contains("mode 644"), "{}", refused(&store));
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o660)).unwrap();
        assert!(refused(&store).contains("mode 660"), "{}", refused(&store));
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(store.load().is_ok());

        let dir = s.path("etc/credentials");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o775)).unwrap();
        assert!(refused(&store).contains("writable"), "{}", refused(&store));
    }

    #[test]
    fn a_token_file_owned_by_another_account_is_refused() {
        let s = Scratch::new("cred-uid");
        let (store, me) = scratch_store(&s);
        store
            .save("owner", &Secret::new(fake_token('A')), 9, 1)
            .unwrap();
        // Root's file, but not with faber's group.
        let other_faber = Account {
            uid: me.uid + 1,
            gid: me.gid + 1,
        };
        let other = Store::owned_by(store.dir.clone(), other_faber, me);
        assert!(
            refused(&other).contains("not faber's"),
            "{}",
            refused(&other)
        );
        let other_dir = Store::owned_by(
            store.dir.clone(),
            me,
            Account {
                uid: me.uid + 1,
                gid: me.gid,
            },
        );
        assert!(refused(&other_dir).contains("owned by uid"));
    }

    /// `_faber`'s group as a setup before the operator group left it: the
    /// operator in it, beside `_faber` itself.
    fn operator_in_the_group(_: u32) -> io::Result<Option<Vec<String>>> {
        Ok(Some(vec![FABER_ACCOUNT.into(), "owner".into()]))
    }

    /// S1: on a host whose record names the operator group, a token whose
    /// group has a member besides `_faber` is refused, naming the member, and
    /// the fix is setup and a new token, not `auth set` alone. `_faber` itself
    /// listed in its own group is no stranger.
    #[test]
    fn a_token_whose_group_has_another_member_is_refused() {
        let s = Scratch::new("cred-member");
        let (store, _) = scratch_store(&s);
        store
            .save("owner", &Secret::new(fake_token('A')), 9, 1)
            .unwrap();
        let exposed = store.clone().with_members(operator_in_the_group);
        let err = exposed.load().unwrap_err();
        let LoadError::Exposed(why) = &err else {
            panic!("expected an exposure, got {err:?}");
        };
        assert!(why.starts_with("owner is in group _faber"), "{why}");
        assert!(why.contains("github-owner.json"), "{why}");
        assert!(!why.contains(FINE_GRAINED_PREFIX), "{why}");
        assert_eq!(err.status(), Status::Invalid);
        assert!(err.fix().contains("sudo faber setup"), "{}", err.fix());
        assert!(
            err.reason().contains("replace the token"),
            "{}",
            err.reason()
        );

        let alone = store.with_members(|_| Ok(Some(vec![FABER_ACCOUNT.into()])));
        assert_eq!(alone.load().unwrap().file_warning, None);
    }

    /// A record from before the operator group: its operator is in the group by
    /// setup's own doing, so the token is used, and the warning says what to
    /// run. A lookup that fails warns too, and never refuses.
    #[test]
    fn a_host_set_up_before_the_operator_group_is_warned_not_refused() {
        let s = Scratch::new("cred-member-warn");
        let (store, _) = scratch_store(&s);
        store
            .save("owner", &Secret::new(fake_token('A')), 9, 1)
            .unwrap();
        let before = store
            .clone()
            .with_members(operator_in_the_group)
            .with_group_rule(GroupRule::Warn);
        let token = before.load().expect("used");
        let warning = token.file_warning.expect("said");
        assert!(warning.starts_with("owner is in group _faber"), "{warning}");
        assert!(warning.contains("sudo faber setup"), "{warning}");
        assert!(warning.contains(FABER_OPERATOR_GROUP), "{warning}");

        let unknown = store.with_members(|_| Err(io::Error::other("no directory service")));
        let warning = unknown.load().expect("used").file_warning.expect("said");
        assert!(warning.contains("no directory service"), "{warning}");
    }

    #[test]
    fn a_symlinked_token_or_directory_is_refused() {
        let s = Scratch::new("cred-link");
        let (store, _) = scratch_store(&s);
        store
            .save("owner", &Secret::new(fake_token('A')), 9, 1)
            .unwrap();
        let real = s.path("elsewhere.json");
        std::fs::rename(s.path("etc/credentials/github-owner.json"), &real).unwrap();
        std::os::unix::fs::symlink(&real, s.path("etc/credentials/github-owner.json")).unwrap();
        assert!(refused(&store).contains("symlink"), "{}", refused(&store));

        let s = Scratch::new("cred-dirlink");
        let (store, _) = scratch_store(&s);
        std::fs::create_dir(s.path("real")).unwrap();
        std::os::unix::fs::symlink(s.path("real"), s.path("etc/credentials")).unwrap();
        let _ = refused(&store);
    }

    #[test]
    fn two_owners_in_the_store_are_refused() {
        let s = Scratch::new("cred-two");
        let (store, _) = scratch_store(&s);
        store
            .save("first", &Secret::new(fake_token('A')), 9, 1)
            .unwrap();
        std::fs::copy(
            s.path("etc/credentials/github-first.json"),
            s.path("etc/credentials/github-second.json"),
        )
        .unwrap();
        let why = refused(&store);
        assert!(
            why.contains("2 owners") && why.contains("serves one"),
            "{why}"
        );
    }

    #[test]
    fn a_file_that_is_not_what_auth_set_writes_is_refused_without_quoting_it() {
        let s = Scratch::new("cred-bad");
        let (store, _) = scratch_store(&s);
        store
            .save("owner", &Secret::new(fake_token('A')), 9, 1)
            .unwrap();
        let file = s.path("etc/credentials/github-owner.json");
        let token = fake_token('Z');
        let cases = [
            format!("{{\"schema\":1,\"owner\":\"owner\",\"token\":\"{token}\""),
            format!(
                "{{\"schema\":2,\"owner\":\"owner\",\"token\":\"{token}\",\"expires_at_ms\":9,\"stored_at_ms\":1}}"
            ),
            format!(
                "{{\"schema\":1,\"owner\":\"other\",\"token\":\"{token}\",\"expires_at_ms\":9,\"stored_at_ms\":1}}"
            ),
            "{\"schema\":1,\"owner\":\"owner\",\"token\":\"nope\",\"expires_at_ms\":9,\"stored_at_ms\":1}"
                .to_string(),
            format!(
                "{{\"schema\":1,\"owner\":\"owner\",\"token\":\"{token}\",\"expires_at_ms\":0,\"stored_at_ms\":1}}"
            ),
        ];
        for case in cases {
            std::fs::write(&file, &case).unwrap();
            let why = refused(&store);
            assert!(!why.contains(FINE_GRAINED_PREFIX), "{why}");
        }
    }

    #[test]
    fn expiry_thresholds() {
        let now = 1_000 * DAY_MS;
        assert_eq!(expiry(now, now), Expiry::Expired);
        assert_eq!(expiry(now - 1, now), Expiry::Expired);
        assert_eq!(expiry(now + 1, now), Expiry::Imminent { days_left: 1 });
        assert_eq!(
            expiry(now + 3 * DAY_MS, now),
            Expiry::Imminent { days_left: 3 }
        );
        assert_eq!(
            expiry(now + 3 * DAY_MS + 1, now),
            Expiry::Soon { days_left: 4 }
        );
        assert_eq!(
            expiry(now + 14 * DAY_MS, now),
            Expiry::Soon { days_left: 14 }
        );
        assert_eq!(expiry(now + 14 * DAY_MS + 1, now), Expiry::Fine);
    }

    #[test]
    fn githubs_expiry_header_parses() {
        // 2026-12-31T23:59:59Z
        let want = crate::retry::parse_rfc3339_to_ms("2026-12-31T23:59:59Z").unwrap();
        assert_eq!(parse_expiry_header("2026-12-31 23:59:59 UTC"), Some(want));
        assert_eq!(
            parse_expiry_header("2027-01-01 01:59:59 +0200"),
            Some(want),
            "an offset is applied"
        );
        assert_eq!(parse_expiry_header("2026-12-31 18:59:59 -0500"), Some(want));
        for bad in [
            "",
            "2026-12-31",
            "2026-12-31 23:59 UTC",
            "soon",
            "2026-12-31 23:59:59 PST",
        ] {
            assert_eq!(parse_expiry_header(bad), None, "{bad}");
        }
        assert_eq!(date_of(want), "2026-12-31");
        assert_eq!(
            parse_date("2026-12-31"),
            crate::retry::parse_rfc3339_to_ms("2026-12-31T00:00:00Z")
        );
        for bad in ["2026-02-31", "2026-1-01", "31-12-2026", "2026/12/31", ""] {
            assert_eq!(parse_date(bad), None, "{bad}");
        }
    }

    fn cfg() -> FileConfig {
        let mut cfg = FileConfig::default();
        cfg.playwright_drift.mode = DriftMode::Off;
        cfg
    }

    fn perms(needs: &[Need]) -> Vec<Permission> {
        let mut out: Vec<Permission> = needs.iter().map(|n| n.permission).collect();
        out.sort();
        out.dedup();
        out
    }

    #[test]
    fn the_waste_check_needs_what_its_modes_do() {
        use Permission::*;
        let mut waste = cfg();
        waste.waste.run_failed = WasteMode::Record;
        assert_eq!(
            perms(&required(&waste)),
            vec![ActionsRead, Administration, ChecksRead, PullRequestsRead]
        );
        waste.waste.superseded = WasteMode::Demote;
        assert_eq!(
            features_needing(&required(&waste), PullRequestsRead),
            vec![Feature::Gate, Feature::WasteCheck]
        );
        assert!(!perms(&required(&waste)).contains(&ActionsWrite));
        waste.waste.closed = WasteMode::Skip;
        assert_eq!(
            features_needing(&required(&waste), ActionsWrite),
            vec![Feature::WasteCheck]
        );
        assert!(!perms(&required(&waste)).contains(&StatusesWrite));
        waste.waste.conflicted = WasteMode::Skip;
        assert_eq!(
            features_needing(&required(&waste), StatusesWrite),
            vec![Feature::WasteCheck]
        );
    }

    #[test]
    fn what_each_feature_needs() {
        use Permission::*;
        let base = cfg();
        assert_eq!(
            perms(&required(&base)),
            vec![ActionsRead, Administration, ChecksRead, PullRequestsRead]
        );
        // Every host: the gate reads a pull request's author (ADR-0059).
        assert_eq!(
            features_needing(&required(&base), PullRequestsRead),
            vec![Feature::Gate]
        );

        let mut retry = cfg();
        retry.retry.enabled = true;
        assert_eq!(
            perms(&required(&retry)),
            vec![
                ActionsRead,
                ActionsWrite,
                Administration,
                ChecksRead,
                PullRequestsRead
            ]
        );
        // Auto-retry judges by faber's own record (ADR-0003, amendment
        // 2026-10-18): it needs Actions: write and nothing from Checks.
        assert_eq!(
            features_needing(&required(&retry), ChecksRead),
            vec![Feature::RunnerRetirement]
        );
        assert_eq!(
            features_needing(&required(&retry), ActionsWrite),
            vec![Feature::AutoRetry]
        );

        let drift = FileConfig::default();
        assert_eq!(
            perms(&required(&drift)),
            vec![
                ActionsRead,
                Administration,
                ChecksRead,
                ContentsRead,
                PullRequestsRead,
                StatusesWrite
            ],
            "drift detection surfaces by default"
        );

        let mut variants = cfg();
        variants.runner_image.build = ImageBuildMode::OnDemand;
        assert_eq!(
            perms(&required(&variants)),
            vec![
                ActionsRead,
                Administration,
                ChecksRead,
                ContentsRead,
                PullRequestsRead
            ]
        );

        let mut mirror = cfg();
        mirror.profiles.push(crate::config::FileProfile {
            name: "docker".into(),
            labels: vec!["faber".into()],
            backend: crate::config::BackendKind::Docker,
            image: Some("ghcr.io/owner/my-runner:1".into()),
            dind: Some(false),
            dind_image: None,
            cache_volume: false,
            git_mirror: true,
            mirror: false,
            drop_grants: Vec::new(),
            env: Default::default(),
            cpus: None,
            memory_mib: None,
        });
        assert_eq!(
            perms(&required(&mirror)),
            vec![
                ActionsRead,
                Administration,
                ChecksRead,
                ContentsRead,
                PullRequestsRead
            ]
        );
    }

    #[test]
    fn the_link_asks_for_exactly_the_grantable_permissions() {
        let mut all = FileConfig::default();
        all.retry.enabled = true;
        let url = link_url("demo-mac", Some("owner"), 90, &required(&all));
        assert_eq!(
            url,
            "https://github.com/settings/personal-access-tokens/new?name=faber-demo-mac\
             &description=faber%20on%20demo-mac%3A%20runs%20the%20jobs%20of%20the%20repositories\
             %20picked%20here%20%28ADR-0027%29\
             &target_name=owner&expires_in=90\
             &actions=write&administration=write&contents=read&pull_requests=read&statuses=write"
        );
        assert!(!url.contains("checks="), "Checks: read cannot be granted");

        let bare = link_url("a b&c", None, 7, &required(&cfg()));
        assert!(bare.contains("name=faber-a%20b%26c&"), "{bare}");
        assert!(!bare.contains("target_name"), "{bare}");
        assert!(
            bare.ends_with("&expires_in=7&actions=read&administration=write&pull_requests=read"),
            "{bare}"
        );
        let bad_owner = link_url("h", Some("x&y=z"), 7, &[]);
        assert!(!bad_owner.contains("target_name"), "{bad_owner}");
    }

    #[test]
    fn a_permission_refusal_is_told_from_a_rate_limit() {
        let denied = r#"{"message":"Resource not accessible by personal access token"}"#;
        assert!(is_permission_refusal(403, false, denied));
        assert!(!is_permission_refusal(403, true, denied));
        assert!(!is_permission_refusal(404, false, denied));
        assert!(!is_permission_refusal(
            403,
            false,
            r#"{"message":"API rate limit exceeded for user"}"#
        ));
        assert!(!is_permission_refusal(
            403,
            false,
            r#"{"message":"You have exceeded a secondary rate limit"}"#
        ));
        assert!(!is_permission_refusal(403, false, "already running"));
    }

    #[test]
    fn the_token_and_the_topic_disagreements_are_both_flagged() {
        let granted = vec![
            "Owner/both".to_string(),
            "owner/granted-only".to_string(),
            "owner/no-admin".to_string(),
            "someone/else".to_string(),
        ];
        let opted = vec![
            "Owner/both".to_string(),
            "owner/no-admin".to_string(),
            "owner/topic-only".to_string(),
        ];
        let m = mismatches("owner", &granted, &opted, |r| r == "owner/no-admin");
        assert_eq!(m.granted_not_opted_in, vec!["owner/granted-only"]);
        assert_eq!(
            m.opted_in_not_granted,
            vec!["owner/no-admin", "owner/topic-only"]
        );
        assert_eq!(
            mismatches("owner", &[], &[], |_| false),
            Mismatches::default()
        );
    }

    #[test]
    fn only_reads_are_probed() {
        let mut all = FileConfig::default();
        all.retry.enabled = true;
        assert_eq!(
            probed(&required(&all)),
            vec![
                Permission::ActionsRead,
                Permission::Administration,
                Permission::ContentsRead,
                Permission::PullRequestsRead
            ]
        );
    }

    #[test]
    fn a_refused_gate_read_names_the_gate_though_the_token_was_not_asked_for_it() {
        // ADR-0059: the ancestry and merge-queue reads are not required, so
        // nothing else names them; a refusal must not read as "running jobs".
        let now = 1_000 * DAY_MS;
        let mut health = Health::of(&token(1, now + 30 * DAY_MS), None);
        health.set_needs(required(&cfg()));
        for permission in [Permission::ContentsRead, Permission::MergeQueuesRead] {
            let warning = health
                .refused("owner/demo", permission, now)
                .expect("warned the first time");
            assert!(warning.contains(Feature::Gate.label()), "{warning}");
        }
        let view = health.view(now);
        let missing: Vec<&str> = view
            .missing_permissions
            .iter()
            .map(|m| m.feature.as_str())
            .collect();
        assert_eq!(missing, [Feature::Gate.label(); 2]);
        // And the token link asks for neither.
        let url = link_url("demo-mac", None, 90, &required(&cfg()));
        assert!(
            !url.contains("merge_queues") && !url.contains("contents"),
            "{url}"
        );
    }

    fn token(stored_at_ms: u64, expires_at_ms: u64) -> StoredToken {
        StoredToken {
            owner: "owner".into(),
            token: Secret::new(fake_token('A')),
            expires_at_ms,
            stored_at_ms,
            file_warning: None,
        }
    }

    /// `credential` in `/api/state` has no token field, whatever state it is
    /// in, and never the token's text.
    #[test]
    fn the_state_view_never_carries_the_token() {
        let now = 1_000 * DAY_MS;
        let t = token(now, now + 100 * DAY_MS);
        let mut health = Health::of(&t, None);
        health.set_needs(required(&FileConfig::default()));
        let _ = health.refused("owner/demo", Permission::StatusesWrite, now);
        for view in [
            health.view(now),
            CredentialView::unusable(&LoadError::Missing),
            CredentialView::unusable(&LoadError::NoStore),
        ] {
            let json = serde_json::to_value(&view).unwrap();
            let obj = json.as_object().unwrap();
            assert!(!obj.contains_key("token"), "{json}");
            assert!(!json.to_string().contains(FINE_GRAINED_PREFIX), "{json}");
            for key in [
                "status",
                "owner",
                "expires_at_ms",
                "stored_at_ms",
                "reason",
                "missing_permissions",
                "mismatches",
            ] {
                assert!(obj.contains_key(key), "{key} in {json}");
            }
        }
    }

    #[test]
    fn the_credential_status_follows_expiry_and_rejection() {
        let now = 1_000 * DAY_MS;
        let fine = Health::of(&token(1, now + 30 * DAY_MS), None);
        assert_eq!(fine.view(now).status, Status::Stored);
        assert_eq!(fine.view(now).reason, None);

        let mut soon = Health::of(&token(1, now + 10 * DAY_MS), None);
        let v = soon.view(now);
        assert_eq!(v.status, Status::Expiring);
        assert!(v.reason.unwrap().contains("10 days"));
        assert_eq!(soon.daily_expiry_warning(now), None, "only within 3 days");

        let mut close = Health::of(&token(1, now + 2 * DAY_MS), None);
        assert!(close.daily_expiry_warning(now).is_some());
        assert_eq!(close.daily_expiry_warning(now + 1000), None, "once a day");
        assert!(close.daily_expiry_warning(now + DAY_MS).is_some());

        assert!(soon.rejected());
        assert!(!soon.rejected(), "news only once");
        let v = soon.view(now);
        assert_eq!(v.status, Status::Invalid);
        assert!(v.blocks_claims());
        assert_eq!(v.reason.as_deref(), Some(REJECTED));
        assert!(soon.accepted());
        assert_eq!(soon.view(now).status, Status::Expiring);

        let mut moved = Health::of(&token(1, now + 30 * DAY_MS), None);
        moved.set_problem(
            Problem::OwnerChanged {
                stored: "other".into(),
                serving: "owner".into(),
            },
            None,
        );
        assert!(!moved.rejected());
        assert!(!moved.accepted(), "only a new token ends an owner change");
        assert!(moved.view(now).reason.unwrap().contains("restart faber"));

        let other = Problem::ActsAsOther {
            acts_as: "other".into(),
            serving: "owner".into(),
        };
        let mut lying = Health::of(&token(1, now + 30 * DAY_MS), None);
        assert!(lying.set_problem(other.clone(), None));
        assert!(!lying.set_problem(other, None), "news only once");
        assert!(!lying.rejected());
        assert!(lying.token_rejected(), "recorded under the standing block");
        assert!(!lying.accepted(), "only a new token ends it");
        assert!(
            !lying.token_rejected(),
            "an accepted request ends the rejection"
        );
        let v = lying.view(now);
        assert!(v.blocks_claims());
        let why = v.reason.unwrap();
        assert!(
            why.contains("acts as other") && why.contains("serves owner"),
            "{why}"
        );
        assert!(why.contains("sudo faber auth set"), "{why}");

        let expired = Health::of(&token(1, now - 1), None);
        assert_eq!(expired.view(now).status, Status::Invalid);
    }

    #[test]
    fn a_missing_permission_blocks_its_feature_until_rechecked() {
        let now = 1_000 * DAY_MS;
        let mut health = Health::of(&token(1, now + 30 * DAY_MS), None);
        let mut all = FileConfig::default();
        all.retry.enabled = true;
        health.set_needs(required(&all));
        let warn = health
            .refused("owner/demo", Permission::ChecksRead, now)
            .expect("warned the first time");
        assert!(warn.contains("Checks: read"), "{warn}");
        assert!(
            !warn.contains("auto-retry"),
            "auto-retry no longer reads annotations: {warn}"
        );
        assert!(warn.contains("runner-retirement warnings"), "{warn}");
        assert!(warn.contains("cannot currently be granted"), "{warn}");
        assert_eq!(
            health.refused("owner/demo", Permission::ChecksRead, now),
            None
        );
        assert!(health.blocked("owner/demo", Permission::ChecksRead, now));
        assert!(!health.blocked("owner/other", Permission::ChecksRead, now));
        assert!(!health.blocked("owner/demo", Permission::ChecksRead, now + RECHECK_MS));
        let view = health.view(now);
        assert_eq!(view.missing_permissions.len(), 1);
        assert_eq!(view.missing_permissions[0].permission, "Checks: read");
        assert!(view
            .unverified_permissions
            .contains(&"Actions: write".to_string()));

        health.permitted("owner/demo", Permission::ChecksRead);
        health.permitted("owner/demo", Permission::ActionsWrite);
        let view = health.view(now);
        assert!(view.missing_permissions.is_empty());
        assert!(!view
            .unverified_permissions
            .contains(&"Actions: write".to_string()));
        let warn = health
            .refused("owner/demo", Permission::Administration, now)
            .unwrap();
        assert!(warn.contains("edited without a new token"), "{warn}");
    }

    // --- who may own a token file ---------------------------------------------

    const ROOT_UID: u32 = 0;
    const FABER: Account = Account { uid: 250, gid: 250 };

    fn facts(kind: EntryKind, uid: u32, gid: u32, mode: u32) -> FileFacts {
        FileFacts {
            kind,
            uid,
            gid,
            mode: 0o100_000 | mode,
        }
    }

    fn verdict(f: FileFacts) -> Ownership {
        ownership(
            Path::new("/etc/faber/credentials/github-owner.json"),
            f,
            ROOT_UID,
            FABER,
        )
    }

    fn refusal(f: FileFacts) -> String {
        match verdict(f) {
            Ownership::Refused(why) => {
                assert!(why.contains("github-owner.json"), "names the path: {why}");
                why
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn root_owned_with_fabers_group_is_the_current_form() {
        use EntryKind::Regular;
        assert_eq!(verdict(facts(Regular, 0, 250, 0o640)), Ownership::Current);
        assert_eq!(verdict(facts(Regular, 0, 250, 0o600)), Ownership::Current);
        assert_eq!(verdict(facts(Regular, 0, 250, 0o440)), Ownership::Current);
    }

    #[test]
    fn fabers_own_0600_file_is_the_legacy_form() {
        use EntryKind::Regular;
        assert_eq!(verdict(facts(Regular, 250, 250, 0o600)), Ownership::Legacy);
        assert_eq!(verdict(facts(Regular, 250, 20, 0o400)), Ownership::Legacy);
    }

    #[test]
    fn every_other_token_file_is_refused() {
        use EntryKind::{Other, Regular, Symlink};
        // `_faber`'s own file, readable by its group: not the legacy form.
        assert!(refusal(facts(Regular, 250, 250, 0o640)).contains("mode 640"));
        // Root's, but another group could read it.
        assert!(refusal(facts(Regular, 0, 20, 0o640)).contains("gid 20"));
        // World-readable, group-writable.
        assert!(refusal(facts(Regular, 0, 250, 0o644)).contains("mode 644"));
        assert!(refusal(facts(Regular, 0, 250, 0o660)).contains("mode 660"));
        // A third account's.
        assert!(refusal(facts(Regular, 501, 250, 0o600)).contains("uid 501"));
        // Not a regular file at all, however it is owned.
        assert!(refusal(facts(Symlink, 0, 250, 0o640)).contains("symlink"));
        assert!(refusal(facts(Other, 0, 250, 0o640)).contains("not a regular file"));
        assert!(refusal(facts(Other, 250, 250, 0o600)).contains("not a regular file"));
    }

    #[test]
    fn save_writes_root_with_the_stores_group_at_0640() {
        let s = Scratch::new("cred-save-mode");
        let me = me(&s);
        std::fs::create_dir(s.path("etc")).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(s.path("etc"), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        let store = Store::owned_by(s.path("etc/credentials"), me, me);
        store
            .save("owner", &Secret::new(fake_token('S')), 9, 1)
            .unwrap();
        let meta = std::fs::symlink_metadata(s.path("etc/credentials/github-owner.json")).unwrap();
        assert!(meta.file_type().is_file());
        assert_eq!(meta.mode() & 0o7777, TOKEN_MODE);
        assert_eq!(meta.uid(), store.dir_owner.uid);
        assert_eq!(meta.gid(), store.faber.gid);
        // A file already in the current form is not touched by setup.
        assert_eq!(store.reown_legacy().unwrap(), None);
        assert_eq!(
            std::fs::symlink_metadata(s.path("etc/credentials/github-owner.json"))
                .unwrap()
                .ino(),
            meta.ino()
        );
    }

    #[test]
    fn setup_leaves_a_refused_token_file_as_it_is() {
        use std::os::unix::fs::PermissionsExt;
        let s = Scratch::new("cred-reown-refused");
        let (store, _) = scratch_store(&s);
        assert_eq!(store.reown_legacy().unwrap(), None, "no store yet");
        store
            .save("owner", &Secret::new(fake_token('R')), 9, 1)
            .unwrap();
        let file = s.path("etc/credentials/github-owner.json");
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = store.reown_legacy().unwrap_err();
        assert!(err.to_string().contains("mode 644"), "{err}");
        assert_eq!(
            std::fs::metadata(&file).unwrap().mode() & 0o7777,
            0o644,
            "left exactly as it was"
        );
    }

    #[test]
    fn a_legacy_token_file_is_used_and_reported() {
        let now = 1_000 * DAY_MS;
        let path = Path::new("/etc/faber/credentials/github-owner.json");
        assert_eq!(
            verdict(facts(EntryKind::Regular, 250, 250, 0o600)),
            Ownership::Legacy
        );
        let mut legacy = token(now, now + 100 * DAY_MS);
        legacy.file_warning = Some(legacy_warning(path));
        let mut health = Health::of(&legacy, None);
        let view = health.view(now);
        assert_eq!(
            view.status,
            Status::Stored,
            "the earlier form blocks nothing"
        );
        assert!(!view.blocks_claims());
        let warning = view
            .file_warning
            .clone()
            .expect("the earlier form is reported");
        assert!(warning.contains("github-owner.json"), "{warning}");
        assert!(warning.contains("sudo faber setup"), "{warning}");
        assert!(!warning.contains(FINE_GRAINED_PREFIX), "{warning}");
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["file_warning"], serde_json::json!(warning));

        // Re-owned by root: the next read is in the current form, and the
        // warning goes with it.
        health.replace(&token(now, now + 100 * DAY_MS), None);
        assert_eq!(health.view(now).file_warning, None);
        assert_eq!(health.file_warning(), None);
    }

    /// A chown alone changes the stamp, so a re-owned file is re-read (and its
    /// warning cleared) without a restart.
    #[test]
    fn a_group_change_changes_the_stamp() {
        let entry = |gid| StampEntry {
            name: "github-owner.json".into(),
            ino: 7,
            mtime: 1,
            mtime_nsec: 0,
            mode: 0o100_600,
            uid: 0,
            gid,
            size: 200,
        };
        assert_ne!(Stamp(vec![entry(0)]), Stamp(vec![entry(250)]));
    }
}
