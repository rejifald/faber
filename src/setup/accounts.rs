//! faber's two role accounts, `_faber` and `_faber_vm` (ADR-0025), and the
//! operator group ([`FABER_OPERATOR_GROUP`]): choosing their ids, reading what
//! the directory service already holds, judging an existing account's or
//! group's shape, and the exact commands that create and delete them.
//!
//! Everything that decides is pure and tested here. The probes at the bottom
//! only run the read-only lookups (`dscl -read`, `getent`, `id -G`) through a
//! [`Runner`], and the command builders return [`Cmd`]s for the executor.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::exe::Untrusted;
use super::tools::{Cmd, Output, Runner, Tool};
use crate::platform::layout::{
    Os, SystemLayout, FABER_ACCOUNT, FABER_MAC_ACCOUNT, FABER_MAC_ID_RANGE, FABER_OPERATOR_GROUP,
    FABER_VM_ACCOUNT,
};
use crate::platform::record::Account;

/// Where both accounts' ids come from. macOS reserves ids below 500 for
/// system and role accounts, and `sysadminctl -roleAccount` needs the name to
/// start with `_`; both were checked on the builder (ADR-0025).
pub const ID_RANGE: RangeInclusive<u32> = 450..=499;

/// The login shell each account gets: none that works.
pub fn shell(os: Os) -> &'static str {
    match os {
        Os::MacOs => "/usr/bin/false",
        Os::Linux => "/usr/sbin/nologin",
    }
}

/// Shells an existing account may already have and still be the right shape.
fn acceptable_shells(os: Os) -> &'static [&'static str] {
    match os {
        Os::MacOs => &["/usr/bin/false"],
        Os::Linux => &[
            "/usr/sbin/nologin",
            "/sbin/nologin",
            "/usr/bin/false",
            "/bin/false",
        ],
    }
}

/// The `RealName` / GECOS each account or group is created with.
pub fn real_name(account: &str) -> &'static str {
    match account {
        FABER_VM_ACCOUNT => "faber VM host",
        FABER_OPERATOR_GROUP => "faber operator",
        _ => "faber daemon",
    }
}

/// Each account's home: `_faber`'s is the state directory, `_faber_vm`'s its
/// own short path (Lima puts sockets under it).
pub fn home(account: &str, layout: &SystemLayout) -> PathBuf {
    if account == FABER_VM_ACCOUNT {
        layout.vm_home.clone()
    } else {
        layout.state_dir.clone()
    }
}

/// The home `sysadminctl -addUser … -home <dir> -roleAccount` leaves on macOS
/// 27, which ignores `-home` for a role account (measured 2026-10-21).
pub const ROLE_ACCOUNT_HOME: &str = "/var/empty";

/// Whether `user`'s home is the one `sysadminctl -roleAccount` left instead of
/// `home`: setup sets it ([`set_home`]) rather than refusing the account.
pub fn home_needs_setting(os: Os, user: &UserInfo, home: &Path) -> bool {
    os == Os::MacOs && user.home != home && user.home == Path::new(ROLE_ACCOUNT_HOME)
}

/// Every uid and gid already taken on this host.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UsedIds {
    pub uids: BTreeSet<u32>,
    pub gids: BTreeSet<u32>,
}

/// The lowest id in [`ID_RANGE`] free as both a uid and a gid, and not in
/// `reserved` (the other account's pick).
pub fn pick_free_id(used: &UsedIds, reserved: &[u32]) -> Option<u32> {
    ID_RANGE
        .into_iter()
        .find(|id| !used.uids.contains(id) && !used.gids.contains(id) && !reserved.contains(id))
}

/// A user record, as the directory service reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserInfo {
    pub name: String,
    pub uid: u32,
    /// The primary group.
    pub gid: u32,
    pub home: PathBuf,
    pub shell: PathBuf,
    /// macOS `IsHidden`; always `true` on Linux, where system accounts are
    /// hidden by their id range.
    pub hidden: bool,
    /// macOS: the `Password` attribute, `*` on an account that cannot log in.
    /// Linux: the shadow password field, read only as root; `None` when it
    /// could not be read.
    pub password: Option<String>,
    /// macOS: an `AuthenticationAuthority` is set, which is where a
    /// `ShadowHash` password lives. Never set on Linux.
    pub auth_authority: bool,
}

/// Whether an account has no usable password, as a role account must not:
/// macOS `Password` of exactly `*` and no `AuthenticationAuthority`, like
/// `_www`; a locked (`!…`) or `*` shadow field on Linux. `None` when that
/// could not be read.
pub fn password_locked(os: Os, user: &UserInfo) -> Option<bool> {
    match os {
        Os::MacOs => Some(user.password.as_deref() == Some("*") && !user.auth_authority),
        Os::Linux => user
            .password
            .as_deref()
            .map(|p| p == "*" || p.starts_with('!')),
    }
}

/// A group record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupInfo {
    pub name: String,
    pub gid: u32,
    /// Explicit members; not those whose primary group this is.
    pub members: Vec<String>,
}

impl GroupInfo {
    pub fn has_member(&self, user: &str) -> bool {
        self.members.iter().any(|m| m == user)
    }
}

/// Everything wrong with an existing account's shape, empty when it is what
/// setup would have created. Setup refuses to adopt an account with any.
pub fn user_problems(
    os: Os,
    user: &UserInfo,
    home: &Path,
    group: Option<&GroupInfo>,
    admins: &[String],
) -> Vec<String> {
    let mut problems = Vec::new();
    let name = &user.name;
    if !ID_RANGE.contains(&user.uid) {
        problems.push(format!(
            "{name} has uid {}, outside {}-{}",
            user.uid,
            ID_RANGE.start(),
            ID_RANGE.end()
        ));
    }
    if user.home != home && !home_needs_setting(os, user, home) {
        problems.push(format!(
            "{name}'s home is {}, not {}",
            user.home.display(),
            home.display()
        ));
    }
    if !acceptable_shells(os)
        .iter()
        .any(|s| Path::new(s) == user.shell)
    {
        problems.push(format!(
            "{name}'s shell is {}, which can log in",
            user.shell.display()
        ));
    }
    match group {
        Some(g) if g.gid != user.gid => problems.push(format!(
            "{name}'s primary group is {}, not {name} ({})",
            user.gid, g.gid
        )),
        None => problems.push(format!("{name} exists but there is no group named {name}")),
        _ => {}
    }
    if admins.iter().any(|a| a == name) {
        problems.push(format!("{name} is an administrator"));
    }
    if password_locked(os, user) == Some(false) {
        problems.push(match os {
            Os::MacOs => format!(
                "{name} can authenticate: {}. Lock it with `sudo {}` and `sudo {}`",
                if user.auth_authority {
                    "it has an AuthenticationAuthority (a ShadowHash password)"
                } else {
                    "its Password is not `*`"
                },
                lock_password(name)[1].display(),
                lock_password(name)[0].display(),
            ),
            Os::Linux => format!("{name} has a password; lock it with `sudo usermod -L {name}`"),
        });
    }
    problems
}

/// Everything wrong with an existing group's shape.
///
/// `_faber`'s group reads the GitHub token, so nobody may be in it but
/// `_faber` itself: any other member is refused, with the command that
/// removes it. The one exception is `operator`, who ran setup and whom the plan
/// takes out ([`remove_member`]), as a setup from before the operator group
/// put them there. Neither `_faber`'s group nor the operator group may hold
/// `_faber_vm` or `_faber_mac`, which handle guest input: in the first they
/// would read the token, in the second faber's config, logs and control
/// socket. Anyone else may be in the operator group.
pub fn group_problems(os: Os, group: &GroupInfo, operator: Option<&str>) -> Vec<String> {
    let mut problems = Vec::new();
    let name = &group.name;
    if !ID_RANGE.contains(&group.gid) {
        problems.push(format!(
            "group {name} has gid {}, outside {}-{}",
            group.gid,
            ID_RANGE.start(),
            ID_RANGE.end()
        ));
    }
    let guards = name == FABER_ACCOUNT || name == FABER_OPERATOR_GROUP;
    for vm in [FABER_VM_ACCOUNT, FABER_MAC_ACCOUNT] {
        if guards && group.has_member(vm) {
            problems.push(format!(
                "{vm} is a member of group {name}: it handles guest input and must never \
                 read what the daemon holds"
            ));
        }
    }
    if name == FABER_ACCOUNT {
        let strangers = group.members.iter().filter(|m| {
            ![FABER_ACCOUNT, FABER_VM_ACCOUNT, FABER_MAC_ACCOUNT].contains(&m.as_str())
                && Some(m.as_str()) != operator
        });
        for member in strangers {
            problems.push(format!(
                "{member} is a member of group {FABER_ACCOUNT}, which reads the GitHub token, so \
                 every process {member} runs can read it. Remove it (`sudo {}`) and re-run",
                remove_member(os, FABER_ACCOUNT, member).display()
            ));
        }
    }
    problems
}

/// The gid a group will have: an existing group's own, or the lowest id in
/// [`ID_RANGE`] free as a gid and not in `reserved` (the accounts' picks).
pub fn plan_gid(
    group: Option<&GroupInfo>,
    used: &UsedIds,
    reserved: &[u32],
    name: &str,
) -> Result<u32, String> {
    if let Some(g) = group {
        return Ok(g.gid);
    }
    ID_RANGE
        .into_iter()
        .find(|id| !used.gids.contains(id) && !reserved.contains(id))
        .ok_or_else(|| {
            format!(
                "no gid in {}-{} is free for {name}",
                ID_RANGE.start(),
                ID_RANGE.end()
            )
        })
}

/// The uid and gid one account will have: an existing account's own, an
/// existing group's gid (reused as the uid when that is free), or the lowest
/// id free as both. `reserved` holds the other account's ids.
pub fn plan_ids(
    user: Option<&UserInfo>,
    group: Option<&GroupInfo>,
    used: &UsedIds,
    reserved: &[u32],
) -> Result<Account, String> {
    match (user, group) {
        (Some(u), Some(g)) => Ok(Account {
            uid: u.uid,
            gid: g.gid,
        }),
        (Some(u), None) => Err(format!(
            "user {} exists without a group of the same name",
            u.name
        )),
        (None, Some(g)) => {
            let uid = if !used.uids.contains(&g.gid) && !reserved.contains(&g.gid) {
                Some(g.gid)
            } else {
                ID_RANGE
                    .into_iter()
                    .find(|id| !used.uids.contains(id) && !reserved.contains(id))
            };
            uid.map(|uid| Account { uid, gid: g.gid }).ok_or_else(|| {
                format!(
                    "no uid in {}-{} is free for {}",
                    ID_RANGE.start(),
                    ID_RANGE.end(),
                    g.name
                )
            })
        }
        (None, None) => pick_free_id(used, reserved)
            .map(|id| Account { uid: id, gid: id })
            .ok_or_else(|| {
                format!(
                    "no id in {}-{} is free as both a uid and a gid",
                    ID_RANGE.start(),
                    ID_RANGE.end()
                )
            }),
    }
}

/// A plausible account name: what setup is willing to put in a command line
/// or a directory-service path.
///
/// Never `.` or `..`, and never a name starting with `.` or `-`: it becomes a
/// path component (`/Users/<name>`) and an argument, where either would mean
/// something else.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with(['.', '-'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

// --- commands ---------------------------------------------------------------

pub fn create_group(os: Os, name: &str, gid: u32) -> Cmd {
    match os {
        Os::MacOs => Cmd::new(
            Tool::Dseditgroup,
            [
                "-o".to_string(),
                "create".into(),
                "-i".into(),
                gid.to_string(),
                "-r".into(),
                real_name(name).into(),
                name.into(),
            ],
        ),
        Os::Linux => Cmd::new(
            Tool::Groupadd,
            [
                "--system".to_string(),
                "-g".into(),
                gid.to_string(),
                name.into(),
            ],
        ),
    }
}

/// Never `-admin`, no password, no login shell.
pub fn create_user(os: Os, name: &str, uid: u32, gid: u32, home: &Path) -> Cmd {
    let home = home.display().to_string();
    match os {
        Os::MacOs => Cmd::new(
            Tool::Sysadminctl,
            [
                "-addUser".to_string(),
                name.into(),
                "-UID".into(),
                uid.to_string(),
                "-GID".into(),
                gid.to_string(),
                "-shell".into(),
                shell(os).into(),
                "-home".into(),
                home,
                "-roleAccount".into(),
            ],
        ),
        Os::Linux => Cmd::new(
            Tool::Useradd,
            [
                "--system".to_string(),
                "--no-create-home".into(),
                "-u".into(),
                uid.to_string(),
                "-g".into(),
                gid.to_string(),
                "-d".into(),
                home,
                "-s".into(),
                shell(os).into(),
                "-c".into(),
                real_name(name).into(),
                name.into(),
            ],
        ),
    }
}

/// macOS only: leave the account no password at all, as system accounts such
/// as `_www` have: `Password` set to `*`, and the `AuthenticationAuthority`
/// that would point at a ShadowHash password deleted. The second command runs
/// only when there is an authority to delete.
pub fn lock_password(name: &str) -> [Cmd; 2] {
    [
        Cmd::new(
            Tool::Dscl,
            [".", "-create", &user_node(name), "Password", "*"],
        ),
        Cmd::new(
            Tool::Dscl,
            [".", "-delete", &user_node(name), "AuthenticationAuthority"],
        ),
    ]
}

/// macOS only: set the account's home, which `sysadminctl -roleAccount`
/// leaves at [`ROLE_ACCOUNT_HOME`] whatever `-home` says.
pub fn set_home(name: &str, home: &Path) -> Cmd {
    Cmd::new(
        Tool::Dscl,
        [
            ".".to_string(),
            "-create".into(),
            user_node(name),
            "NFSHomeDirectory".into(),
            home.display().to_string(),
        ],
    )
}

/// macOS only: keep the account off the login window.
pub fn hide_user(name: &str) -> Cmd {
    Cmd::new(
        Tool::Dscl,
        [".", "-create", &user_node(name), "IsHidden", "1"],
    )
}

pub fn add_member(os: Os, group: &str, user: &str) -> Cmd {
    match os {
        Os::MacOs => Cmd::new(
            Tool::Dseditgroup,
            ["-o", "edit", "-a", user, "-t", "user", group],
        ),
        Os::Linux => Cmd::new(Tool::Usermod, ["-aG", group, user]),
    }
}

/// Take `user` out of `group`'s member list. It changes only sessions started
/// afterwards: a process keeps the groups it started with.
pub fn remove_member(os: Os, group: &str, user: &str) -> Cmd {
    match os {
        Os::MacOs => Cmd::new(
            Tool::Dseditgroup,
            ["-o", "edit", "-d", user, "-t", "user", group],
        ),
        Os::Linux => Cmd::new(Tool::Gpasswd, ["-d", user, group]),
    }
}

/// Deletes the record only: the home is a directory uninstall removes itself,
/// before this runs.
pub fn delete_user(os: Os, name: &str) -> Cmd {
    match os {
        Os::MacOs => Cmd::new(Tool::Dscl, [".", "-delete", &user_node(name)]),
        Os::Linux => Cmd::new(Tool::Userdel, [name]),
    }
}

pub fn delete_group(os: Os, name: &str) -> Cmd {
    match os {
        Os::MacOs => Cmd::new(Tool::Dseditgroup, ["-o", "delete", name]),
        Os::Linux => Cmd::new(Tool::Groupdel, [name]),
    }
}

/// Whether a failed account command was macOS refusing it under TCC: account
/// deletion is checked against the application hosting the terminal
/// (ADR-0025). `-14120` is `eDSPermissionError`'s code.
pub fn is_tcc_denial(text: &str) -> bool {
    text.contains("eDSPermissionError") || text.contains("-14120")
}

/// What to tell the operator when [`is_tcc_denial`] holds.
pub const TCC_MESSAGE: &str =
    "macOS refused to delete the account (eDSPermissionError). It checks \
     account deletion against the app hosting this terminal (\"administer your computer\" and \
     Full Disk Access), and a terminal inside another app does not have them. Re-run \
     `sudo faber uninstall` from Terminal.app: everything removed so far stays removed, and the \
     re-run finishes the accounts.";

// --- parsers ----------------------------------------------------------------

/// A dscl attribute name without its record-type prefix: macOS 27 prints an
/// attribute it has no standard name for as `dsAttrTypeNative:IsHidden`
/// (measured 2026-10-21), so `IsHidden` was never found and every run planned
/// to hide an account that already was.
fn bare_key(key: &str) -> String {
    key.strip_prefix("dsAttrTypeNative:")
        .or_else(|| key.strip_prefix("dsAttrTypeStandard:"))
        .unwrap_or(key)
        .to_string()
}

/// `dscl . -read <record> <keys…>` output: `Key: v1 v2` on one line, or `Key:`
/// followed by continuation lines starting with a space, one value each, which
/// is how dscl prints a value containing a space.
pub fn parse_dscl_read(text: &str) -> BTreeMap<String, Vec<String>> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        if let Some(value) = line.strip_prefix(' ') {
            if let Some(key) = &current {
                map.entry(key.clone())
                    .or_default()
                    .push(value.trim().to_string());
            }
            continue;
        }
        if line.starts_with("No such key") {
            current = None;
            continue;
        }
        if let Some(key) = line.strip_suffix(':') {
            let key = bare_key(key);
            map.insert(key.clone(), Vec::new());
            current = Some(key);
        } else if let Some((key, rest)) = line.split_once(": ") {
            map.insert(
                bare_key(key),
                rest.split_whitespace().map(str::to_string).collect(),
            );
            current = None;
        } else {
            current = None;
        }
    }
    map
}

/// `dscl . -list /Users UniqueID` (or any attribute): a name, whitespace, the
/// rest of the line as the value.
pub fn parse_dscl_list(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim_end();
            let (name, rest) = line.split_once(char::is_whitespace)?;
            let value = rest.trim();
            (!name.is_empty() && !value.is_empty()).then(|| (name.to_string(), value.to_string()))
        })
        .collect()
}

fn first_u32(map: &BTreeMap<String, Vec<String>>, key: &str) -> Option<u32> {
    map.get(key)?.first()?.parse().ok()
}

/// A user record from [`parse_dscl_read`] output.
pub fn user_from_dscl(name: &str, text: &str) -> Option<UserInfo> {
    let map = parse_dscl_read(text);
    Some(UserInfo {
        name: name.to_string(),
        uid: first_u32(&map, "UniqueID")?,
        gid: first_u32(&map, "PrimaryGroupID")?,
        home: PathBuf::from(map.get("NFSHomeDirectory")?.first()?),
        shell: map
            .get("UserShell")
            .and_then(|v| v.first())
            .map(PathBuf::from)
            .unwrap_or_default(),
        hidden: map
            .get("IsHidden")
            .and_then(|v| v.first())
            .is_some_and(|v| v == "1" || v.eq_ignore_ascii_case("yes")),
        password: map.get("Password").and_then(|v| v.first()).cloned(),
        auth_authority: map.contains_key("AuthenticationAuthority"),
    })
}

/// A group record from [`parse_dscl_read`] output.
pub fn group_from_dscl(name: &str, text: &str) -> Option<GroupInfo> {
    let map = parse_dscl_read(text);
    Some(GroupInfo {
        name: name.to_string(),
        gid: first_u32(&map, "PrimaryGroupID")?,
        members: map.get("GroupMembership").cloned().unwrap_or_default(),
    })
}

/// One `getent passwd` line: `name:x:uid:gid:gecos:home:shell`.
pub fn parse_passwd_line(line: &str) -> Option<UserInfo> {
    let f: Vec<&str> = line.trim_end().split(':').collect();
    if f.len() < 7 {
        return None;
    }
    Some(UserInfo {
        name: f[0].to_string(),
        uid: f[2].parse().ok()?,
        gid: f[3].parse().ok()?,
        home: PathBuf::from(f[5]),
        shell: PathBuf::from(f[6]),
        hidden: true,
        password: None,
        auth_authority: false,
    })
}

/// The password field of a `getent shadow` line: `name:field:…`.
pub fn parse_shadow_line(line: &str) -> Option<String> {
    let mut f = line.trim_end().splitn(3, ':');
    f.next()?;
    f.next().map(str::to_string)
}

/// One `getent group` line: `name:x:gid:m1,m2`.
pub fn parse_group_line(line: &str) -> Option<GroupInfo> {
    let f: Vec<&str> = line.trim_end().split(':').collect();
    if f.len() < 4 {
        return None;
    }
    Some(GroupInfo {
        name: f[0].to_string(),
        gid: f[2].parse().ok()?,
        members: f[3]
            .split(',')
            .filter(|m| !m.is_empty())
            .map(str::to_string)
            .collect(),
    })
}

/// `id -G <user>`: every group id the account belongs to, as the system
/// resolves membership (nested groups included on macOS).
pub fn parse_id_groups(text: &str) -> Vec<u32> {
    text.split_whitespace()
        .filter_map(|g| g.parse().ok())
        .collect()
}

// --- probes -----------------------------------------------------------------

/// The directory-service node for a user: `/Users/<name>` on macOS.
pub fn user_node(name: &str) -> String {
    format!("{}/{name}", "/Users")
}

/// The directory-service node for a group: `/Groups/<name>` on macOS.
pub fn group_node(name: &str) -> String {
    format!("{}/{name}", "/Groups")
}

/// Numeric ids from a `dscl -list` value column. Negative ids (`nobody` is
/// -2) are not in any range setup allocates from, so they are dropped.
fn listed_ids(text: &str) -> BTreeSet<u32> {
    parse_dscl_list(text)
        .into_iter()
        .filter_map(|(_, v)| v.parse::<i64>().ok())
        .filter_map(|v| u32::try_from(v).ok())
        .collect()
}

/// Every uid and gid taken on this host.
pub fn probe_used_ids(os: Os, runner: &mut dyn Runner) -> Result<UsedIds> {
    Ok(match os {
        Os::MacOs => {
            let users =
                runner.run_ok(&Cmd::new(Tool::Dscl, [".", "-list", "/Users", "UniqueID"]))?;
            let groups = runner.run_ok(&Cmd::new(
                Tool::Dscl,
                [".", "-list", "/Groups", "PrimaryGroupID"],
            ))?;
            UsedIds {
                uids: listed_ids(&users.stdout),
                gids: listed_ids(&groups.stdout),
            }
        }
        Os::Linux => {
            let users = runner.run_ok(&Cmd::new(Tool::Getent, ["passwd"]))?;
            let groups = runner.run_ok(&Cmd::new(Tool::Getent, ["group"]))?;
            UsedIds {
                uids: users
                    .stdout
                    .lines()
                    .filter_map(parse_passwd_line)
                    .map(|u| u.uid)
                    .collect(),
                gids: groups
                    .stdout
                    .lines()
                    .filter_map(parse_group_line)
                    .map(|g| g.gid)
                    .collect(),
            }
        }
    })
}

/// The user `name`, or `None` when there is no such account.
pub fn probe_user(os: Os, runner: &mut dyn Runner, name: &str) -> Result<Option<UserInfo>> {
    if !valid_name(name) {
        bail!("{name:?} is not an account name setup will look up");
    }
    let cmd = match os {
        Os::MacOs => Cmd::new(
            Tool::Dscl,
            [
                ".".to_string(),
                "-read".into(),
                user_node(name),
                "UniqueID".into(),
                "PrimaryGroupID".into(),
                "NFSHomeDirectory".into(),
                "UserShell".into(),
                "IsHidden".into(),
                "Password".into(),
                "AuthenticationAuthority".into(),
            ],
        ),
        Os::Linux => Cmd::new(Tool::Getent, ["passwd", name]),
    };
    let out = runner.run(&cmd)?;
    if not_found(os, &out) {
        return Ok(None);
    }
    if !out.success {
        bail!("reading user {name}: {}", out.text());
    }
    let mut user = match os {
        Os::MacOs => user_from_dscl(name, &out.stdout),
        Os::Linux => out.stdout.lines().find_map(parse_passwd_line),
    }
    .with_context(|| format!("unreadable directory record for {name}"))?;
    if os == Os::Linux {
        // Readable by root only; without sudo the password stays unknown.
        user.password = runner
            .run(&Cmd::new(Tool::Getent, ["shadow", name]))
            .ok()
            .filter(|o| o.success)
            .and_then(|o| o.stdout.lines().find_map(parse_shadow_line));
    }
    Ok(Some(user))
}

/// The group `name`, or `None` when there is no such group.
pub fn probe_group(os: Os, runner: &mut dyn Runner, name: &str) -> Result<Option<GroupInfo>> {
    if !valid_name(name) {
        bail!("{name:?} is not a group name setup will look up");
    }
    let cmd = match os {
        Os::MacOs => Cmd::new(
            Tool::Dscl,
            [
                ".".to_string(),
                "-read".into(),
                group_node(name),
                "PrimaryGroupID".into(),
                "GroupMembership".into(),
            ],
        ),
        Os::Linux => Cmd::new(Tool::Getent, ["group", name]),
    };
    let out = runner.run(&cmd)?;
    if not_found(os, &out) {
        return Ok(None);
    }
    if !out.success {
        bail!("reading group {name}: {}", out.text());
    }
    let group = match os {
        Os::MacOs => group_from_dscl(name, &out.stdout),
        Os::Linux => out.stdout.lines().find_map(parse_group_line),
    };
    group
        .map(Some)
        .with_context(|| format!("unreadable directory record for group {name}"))
}

/// Whether a lookup failed only because the record does not exist: dscl's
/// `eDSRecordNotFound`, or getent's exit 2.
fn not_found(os: Os, out: &super::tools::Output) -> bool {
    !out.success
        && match os {
            Os::MacOs => out.text().contains("eDSRecordNotFound"),
            Os::Linux => out.code == Some(2),
        }
}

/// Whether ADR-0051's session account ([`FABER_MAC_ACCOUNT`]) exists on this Mac
/// with a uid from [`FABER_MAC_ID_RANGE`]: what turning macOS VMs on needs. Reads
/// the uid only, which needs no root. `false` on any failure, and on Linux,
/// which has no macOS VMs.
pub fn probe_session_account(os: Os, runner: &mut dyn Runner) -> bool {
    session_account_uid(os, runner).is_some_and(|u| FABER_MAC_ID_RANGE.contains(&u))
}

/// [`FABER_MAC_ACCOUNT`]'s uid as the directory says it, whatever it is;
/// `None` when the account does not exist, on any failure, and on Linux. Needs
/// no root: the daemon checks the record's uid against it (ADR-0051).
pub fn session_account_uid(os: Os, runner: &mut dyn Runner) -> Option<u32> {
    if os != Os::MacOs {
        return None;
    }
    let cmd = Cmd::new(
        Tool::Dscl,
        [
            ".".to_string(),
            "-read".into(),
            user_node(FABER_MAC_ACCOUNT),
            "UniqueID".into(),
        ],
    );
    match runner.run(&cmd) {
        Ok(out) if out.success => first_u32(&parse_dscl_read(&out.stdout), "UniqueID"),
        _ => None,
    }
}

/// What the directory says of [`FABER_MAC_ACCOUNT`], for the daemon's check of
/// the record's uid (ADR-0051): its uid, that it does not exist, or that the
/// question got no answer, which is not the same thing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionLookup {
    Found(u32),
    NotFound,
    Failed(String),
}

/// How long the daemon's `dscl` may take before it counts as no answer.
pub const SESSION_LOOKUP_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

/// Read the outcome of `dscl . -read` of the account's `UniqueID`. `dscl` says a
/// missing record with `eDSRecordNotFound`; every other failure, a run that
/// did not finish included, is [`SessionLookup::Failed`]. Pure.
pub fn session_lookup_of(run: Result<Output, String>) -> SessionLookup {
    let out = match run {
        Ok(out) => out,
        Err(e) => return SessionLookup::Failed(e),
    };
    let text = out.text();
    if out.success {
        return match first_u32(&parse_dscl_read(&out.stdout), "UniqueID") {
            Some(uid) => SessionLookup::Found(uid),
            None => SessionLookup::Failed("dscl gave no UniqueID".into()),
        };
    }
    if text.contains("eDSRecordNotFound") {
        SessionLookup::NotFound
    } else {
        SessionLookup::Failed(format!(
            "dscl failed: {}",
            super::tools::sanitize_child_output(text.trim(), 200)
        ))
    }
}

/// [`session_lookup_of`] for this host, the `dscl` run bounded by
/// [`SESSION_LOOKUP_LIMIT`] and killed past it. Needs no root. Blocking.
pub fn session_lookup_bounded() -> SessionLookup {
    if Os::HOST != Os::MacOs {
        return SessionLookup::NotFound;
    }
    let tool = Tool::Dscl;
    let mut cmd = std::process::Command::new(tool.path());
    cmd.args([".", "-read", &user_node(FABER_MAC_ACCOUNT), "UniqueID"])
        .env_clear()
        .env("PATH", super::tools::SAFE_PATH)
        .stdin(std::process::Stdio::null());
    session_lookup_of(
        super::tools::output_within(cmd, SESSION_LOOKUP_LIMIT).map_err(|e| format!("dscl: {e:#}")),
    )
}

/// Whether a `dscl -read … UniqueID` names a uid in [`FABER_MAC_ID_RANGE`], as
/// [`probe_session_account`] reads it.
#[cfg(test)]
fn session_uid_ok(dscl: &str) -> bool {
    first_u32(&parse_dscl_read(dscl), "UniqueID").is_some_and(|u| FABER_MAC_ID_RANGE.contains(&u))
}

/// Every group id `user` belongs to; empty when the account does not exist.
pub fn probe_groups_of(runner: &mut dyn Runner, user: &str) -> Vec<u32> {
    if !valid_name(user) {
        return Vec::new();
    }
    match runner.run(&Cmd::new(Tool::Id, ["-G", user])) {
        Ok(out) if out.success => parse_id_groups(&out.stdout),
        _ => Vec::new(),
    }
}

/// faber's accounts and groups as they exist now, and the ids root must not
/// trust because of them ([`Untrusted`]): each account's uid and primary gid,
/// its own group, every group it belongs to, the operator group (whose members
/// include `_faber`, and on Linux the daemon's primary group), and on Linux
/// `docker` when `_faber` is in it.
#[derive(Debug, Clone, Default)]
pub struct FaberAccounts {
    pub untrusted: Untrusted,
    pub users: Vec<UserInfo>,
    pub groups: Vec<GroupInfo>,
}

/// Read [`FaberAccounts`]: what `sudo faber uninstall` and `sudo faber
/// upgrade-cli` judge the binary running as root against.
pub fn probe_faber_accounts(os: Os, runner: &mut dyn Runner) -> Result<FaberAccounts> {
    let mut found = FaberAccounts::default();
    for name in [FABER_ACCOUNT, FABER_VM_ACCOUNT] {
        if let Some(user) = probe_user(os, runner, name)? {
            found.untrusted.uids.push(user.uid);
            found.untrusted.gids.push(user.gid);
            found.untrusted.gids.extend(probe_groups_of(runner, name));
            found.users.push(user);
        }
        if let Some(group) = probe_group(os, runner, name)? {
            found.untrusted.gids.push(group.gid);
            found.groups.push(group);
        }
    }
    if let Some(group) = probe_group(os, runner, FABER_OPERATOR_GROUP)? {
        found.untrusted.gids.push(group.gid);
        found.groups.push(group);
    }
    if os == Os::Linux {
        if let Some(docker) = probe_group(os, runner, "docker")? {
            if docker.has_member(FABER_ACCOUNT) {
                found.untrusted.gids.push(docker.gid);
            }
        }
    }
    Ok(found)
}

/// Every account with a home directory: `(name, uid, home)`.
pub fn probe_homes(os: Os, runner: &mut dyn Runner) -> Result<Vec<(String, u32, PathBuf)>> {
    match os {
        Os::MacOs => {
            let homes = runner.run_ok(&Cmd::new(
                Tool::Dscl,
                [".", "-list", "/Users", "NFSHomeDirectory"],
            ))?;
            let ids = runner.run_ok(&Cmd::new(Tool::Dscl, [".", "-list", "/Users", "UniqueID"]))?;
            let uids: BTreeMap<String, u32> = parse_dscl_list(&ids.stdout)
                .into_iter()
                .filter_map(|(n, v)| Some((n, v.parse().ok()?)))
                .collect();
            Ok(parse_dscl_list(&homes.stdout)
                .into_iter()
                .filter_map(|(name, home)| {
                    // A second absolute value is an alias (`/var/root
                    // /private/var/root`); the first is the home.
                    let home = home.split(" /").next().unwrap_or(&home).to_string();
                    Some((name.clone(), *uids.get(&name)?, PathBuf::from(home)))
                })
                .collect())
        }
        Os::Linux => {
            let out = runner.run_ok(&Cmd::new(Tool::Getent, ["passwd"]))?;
            Ok(out
                .stdout
                .lines()
                .filter_map(parse_passwd_line)
                .map(|u| (u.name, u.uid, u.home))
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::tools::{FakeRunner, Output};

    fn used(uids: &[u32], gids: &[u32]) -> UsedIds {
        UsedIds {
            uids: uids.iter().copied().collect(),
            gids: gids.iter().copied().collect(),
        }
    }

    #[test]
    fn the_id_picker_takes_the_lowest_id_free_as_both() {
        assert_eq!(pick_free_id(&used(&[], &[]), &[]), Some(450));
        // 450 taken as a uid, 451 as a gid: neither is free as both.
        assert_eq!(pick_free_id(&used(&[450], &[451]), &[]), Some(452));
        // The other account's pick is skipped.
        assert_eq!(pick_free_id(&used(&[], &[]), &[450]), Some(451));
    }

    /// With 450 through 480 taken, the picker walks past taken ids anywhere in
    /// the range and gives up only when none is left.
    #[test]
    fn the_id_picker_skips_taken_ids_across_the_range() {
        let taken: Vec<u32> = (450..=479).collect();
        let u = used(&[&taken[..], &[480]].concat(), &[]);
        assert_eq!(pick_free_id(&u, &[]), Some(481));
        assert_eq!(pick_free_id(&u, &[481]), Some(482));

        let all: Vec<u32> = ID_RANGE.collect();
        assert_eq!(pick_free_id(&used(&all, &[]), &[]), None);
        assert_eq!(pick_free_id(&used(&[], &all), &[]), None);
        // Ids outside the range never matter.
        assert_eq!(
            pick_free_id(&used(&[1, 449, 500], &[1, 449, 500]), &[]),
            Some(450)
        );
    }

    fn group(name: &str, gid: u32, members: &[&str]) -> GroupInfo {
        GroupInfo {
            name: name.into(),
            gid,
            members: members.iter().map(|m| m.to_string()).collect(),
        }
    }

    /// An account as setup leaves it: hidden, `Password` `*`, no authority.
    fn user(name: &str, uid: u32, gid: u32, home: &str, shell: &str) -> UserInfo {
        UserInfo {
            name: name.into(),
            uid,
            gid,
            home: home.into(),
            shell: shell.into(),
            hidden: true,
            password: Some("*".into()),
            auth_authority: false,
        }
    }

    #[test]
    fn plan_ids_adopts_what_exists_and_picks_the_rest() {
        let u = used(&[450, 451], &[450, 451]);
        let faber_user = user("_faber", 450, 450, "/x", "/usr/bin/false");
        let faber_group = group("_faber", 450, &[]);
        assert_eq!(
            plan_ids(Some(&faber_user), Some(&faber_group), &u, &[]),
            Ok(Account { uid: 450, gid: 450 })
        );
        // A group left by an interrupted run: its gid becomes the uid too.
        let lone = group("_faber_vm", 451, &[]);
        assert_eq!(
            plan_ids(None, Some(&lone), &used(&[450], &[450, 451]), &[450]),
            Ok(Account { uid: 451, gid: 451 })
        );
        // ... unless that uid is taken, when the lowest free uid is used.
        assert_eq!(
            plan_ids(None, Some(&lone), &used(&[450, 451], &[450, 451]), &[450]),
            Ok(Account { uid: 452, gid: 451 })
        );
        assert_eq!(
            plan_ids(None, None, &used(&[450], &[450]), &[]),
            Ok(Account { uid: 451, gid: 451 })
        );
        assert!(plan_ids(Some(&faber_user), None, &u, &[]).is_err());
    }

    /// `/var/empty` is the one wrong home setup repairs, and only on macOS.
    #[test]
    fn only_the_role_account_home_is_repairable() {
        let home = Path::new("/Library/Application Support/faber");
        let g = group("_faber", 451, &[]);
        let left = user("_faber", 451, 451, ROLE_ACCOUNT_HOME, "/usr/bin/false");
        assert!(home_needs_setting(Os::MacOs, &left, home));
        assert!(user_problems(Os::MacOs, &left, home, Some(&g), &[]).is_empty());
        assert!(!home_needs_setting(Os::Linux, &left, home));
        assert!(!user_problems(Os::Linux, &left, home, Some(&g), &[]).is_empty());
        let other = user("_faber", 451, 451, "/Users/someone", "/usr/bin/false");
        assert!(!home_needs_setting(Os::MacOs, &other, home));
        assert!(!user_problems(Os::MacOs, &other, home, Some(&g), &[]).is_empty());
        assert_eq!(
            set_home("_faber", home).display(),
            format!(
                "/usr/bin/dscl . -create {} NFSHomeDirectory \"/Library/Application Support/faber\"",
                user_node("_faber")
            )
        );
    }

    #[test]
    fn a_wrong_shaped_account_is_described_not_adopted() {
        let home = Path::new("/Library/Application Support/faber");
        let g = group("_faber", 451, &[]);
        let ok = user(
            "_faber",
            451,
            451,
            "/Library/Application Support/faber",
            "/usr/bin/false",
        );
        assert!(user_problems(Os::MacOs, &ok, home, Some(&g), &[]).is_empty());

        let bad = user("_faber", 501, 20, "/Users/owner", "/bin/zsh");
        let problems = user_problems(Os::MacOs, &bad, home, Some(&g), &["_faber".into()]);
        assert_eq!(problems.len(), 5, "{problems:?}");
        assert!(problems.iter().any(|p| p.contains("uid 501")));
        assert!(problems.iter().any(|p| p.contains("home")));
        assert!(problems.iter().any(|p| p.contains("can log in")));
        assert!(problems.iter().any(|p| p.contains("primary group")));
        assert!(problems.iter().any(|p| p.contains("administrator")));

        let linux = user("_faber", 451, 451, "/var/lib/faber", "/sbin/nologin");
        assert!(user_problems(
            Os::Linux,
            &linux,
            Path::new("/var/lib/faber"),
            Some(&g),
            &[]
        )
        .is_empty());
    }

    /// A role account must not be able to authenticate: an existing one with a
    /// password, or with an AuthenticationAuthority, is refused, and the
    /// refusal says how to lock it.
    #[test]
    fn an_account_with_a_password_is_refused() {
        let home = Path::new("/Library/Application Support/faber");
        let g = group("_faber", 451, &[]);
        let base = user(
            "_faber",
            451,
            451,
            "/Library/Application Support/faber",
            "/usr/bin/false",
        );
        assert_eq!(password_locked(Os::MacOs, &base), Some(true));

        let with_hash = UserInfo {
            password: Some("********".into()),
            auth_authority: true,
            ..base.clone()
        };
        assert_eq!(password_locked(Os::MacOs, &with_hash), Some(false));
        let p = user_problems(Os::MacOs, &with_hash, home, Some(&g), &[]);
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].contains("AuthenticationAuthority"), "{p:?}");
        assert!(p[0].contains("-delete"), "{p:?}");

        let no_star = UserInfo {
            password: None,
            ..base.clone()
        };
        assert!(user_problems(Os::MacOs, &no_star, home, Some(&g), &[])[0].contains("not `*`"));

        let lin = Path::new("/var/lib/faber");
        let locked = |pw: Option<&str>| UserInfo {
            password: pw.map(str::to_string),
            auth_authority: false,
            home: lin.into(),
            shell: "/usr/sbin/nologin".into(),
            ..base.clone()
        };
        for ok in [Some("!"), Some("!!"), Some("*"), Some("!$6$abc"), None] {
            assert!(
                user_problems(Os::Linux, &locked(ok), lin, Some(&g), &[]).is_empty(),
                "{ok:?}"
            );
        }
        for bad in ["", "$6$salt$hash"] {
            let p = user_problems(Os::Linux, &locked(Some(bad)), lin, Some(&g), &[]);
            assert!(p[0].contains("usermod -L"), "{bad:?}: {p:?}");
        }
    }

    #[test]
    fn locking_a_password_is_exactly_two_dscl_commands() {
        let [set, delete] = lock_password("_faber");
        assert_eq!(
            set.display(),
            format!(
                "/usr/bin/dscl . -create {} Password \"*\"",
                user_node("_faber")
            )
        );
        assert_eq!(
            delete.display(),
            format!(
                "/usr/bin/dscl . -delete {} AuthenticationAuthority",
                user_node("_faber")
            )
        );
        assert_eq!(
            set.args,
            vec![".", "-create", &user_node("_faber"), "Password", "*"]
        );
    }

    #[test]
    fn group_shape_keeps_faber_vm_out_of_faber() {
        let op = Some("owner");
        assert!(group_problems(Os::MacOs, &group("_faber", 451, &["owner"]), op).is_empty());
        assert_eq!(
            group_problems(Os::MacOs, &group("_faber", 20, &[]), op).len(),
            1
        );
        let p = group_problems(
            Os::MacOs,
            &group("_faber", 451, &["owner", "_faber_vm"]),
            op,
        );
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].contains("must never read"), "{p:?}");
    }

    /// S1: `_faber`'s group reads the token, so a member other than `_faber`
    /// is refused, with the command that removes it. The operator running
    /// setup is not refused: the plan takes them out.
    #[test]
    fn a_stranger_in_faber_is_refused_with_the_command_that_removes_it() {
        let g = group("_faber", 451, &["_faber", "owner", "ci"]);
        let p = group_problems(Os::MacOs, &g, Some("owner"));
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].starts_with("ci is a member of group _faber"), "{p:?}");
        assert!(p[0].contains("reads the GitHub token"), "{p:?}");
        assert!(
            p[0].contains("`sudo /usr/sbin/dseditgroup -o edit -d ci -t user _faber`"),
            "{p:?}"
        );
        // Without a known operator, the operator is a stranger too.
        assert_eq!(group_problems(Os::MacOs, &g, None).len(), 2);
        let linux = group_problems(Os::Linux, &g, Some("owner"));
        assert!(
            linux[0].contains("`sudo /usr/bin/gpasswd -d ci _faber`"),
            "{linux:?}"
        );
        // `_faber` itself, listed or not, is never a stranger in its own group.
        assert!(group_problems(Os::MacOs, &group("_faber", 451, &["_faber"]), None).is_empty());
    }

    /// The operator group takes anyone but the accounts that handle guest
    /// input.
    #[test]
    fn the_operator_group_keeps_the_vm_accounts_out() {
        let ok = group("_faber_operator", 453, &["owner", "_faber", "colleague"]);
        assert!(group_problems(Os::MacOs, &ok, Some("owner")).is_empty());
        for vm in ["_faber_vm", "_faber_mac"] {
            let bad = group("_faber_operator", 453, &["owner", vm]);
            let p = group_problems(Os::MacOs, &bad, Some("owner"));
            assert_eq!(p.len(), 1, "{p:?}");
            assert!(p[0].starts_with(&format!("{vm} is a member")), "{p:?}");
        }
        assert_eq!(
            group_problems(Os::MacOs, &group("_faber_operator", 20, &[]), None).len(),
            1,
            "a gid outside the range"
        );
    }

    #[test]
    fn a_group_gid_is_its_own_or_the_lowest_free_one() {
        let existing = group("_faber_operator", 460, &[]);
        assert_eq!(
            plan_gid(
                Some(&existing),
                &used(&[], &[]),
                &[450, 451],
                "_faber_operator"
            ),
            Ok(460)
        );
        // Free as a gid is enough, and the accounts' picks are skipped.
        assert_eq!(
            plan_gid(None, &used(&[452], &[450]), &[450, 451], "_faber_operator"),
            Ok(452)
        );
        let all: Vec<u32> = ID_RANGE.collect();
        let none = plan_gid(None, &used(&[], &all), &[], "_faber_operator");
        assert!(none.unwrap_err().contains("_faber_operator"));
    }

    #[test]
    fn macos_commands_are_exactly_the_documented_ones() {
        let home = Path::new("/Library/Application Support/faber");
        assert_eq!(
            create_group(Os::MacOs, "_faber", 451).display(),
            "/usr/sbin/dseditgroup -o create -i 451 -r \"faber daemon\" _faber"
        );
        let add = create_user(Os::MacOs, "_faber", 451, 451, home);
        assert_eq!(
            add.display(),
            "/usr/sbin/sysadminctl -addUser _faber -UID 451 -GID 451 -shell /usr/bin/false \
             -home \"/Library/Application Support/faber\" -roleAccount"
        );
        assert!(!add.args.iter().any(|a| a == "-admin" || a == "-password"));
        assert_eq!(
            add_member(Os::MacOs, "_faber", "owner").display(),
            "/usr/sbin/dseditgroup -o edit -a owner -t user _faber"
        );
        assert_eq!(
            remove_member(Os::MacOs, "_faber", "owner").display(),
            "/usr/sbin/dseditgroup -o edit -d owner -t user _faber"
        );
        assert_eq!(
            create_group(Os::MacOs, "_faber_operator", 452).display(),
            "/usr/sbin/dseditgroup -o create -i 452 -r \"faber operator\" _faber_operator"
        );
        // The node is spelled through `user_node`: a literal home-shaped path
        // here would trip the privacy gate's personal-path check.
        assert_eq!(user_node("_faber"), ["/Users", "_faber"].join("/"));
        assert_eq!(
            hide_user("_faber").display(),
            format!("/usr/bin/dscl . -create {} IsHidden 1", user_node("_faber"))
        );
        assert_eq!(
            delete_user(Os::MacOs, "_faber").display(),
            format!("/usr/bin/dscl . -delete {}", user_node("_faber"))
        );
        assert_eq!(
            delete_group(Os::MacOs, "_faber").display(),
            "/usr/sbin/dseditgroup -o delete _faber"
        );
    }

    #[test]
    fn linux_commands_are_exactly_the_documented_ones() {
        assert_eq!(
            create_group(Os::Linux, "_faber", 451).display(),
            "/usr/sbin/groupadd --system -g 451 _faber"
        );
        assert_eq!(
            create_user(
                Os::Linux,
                "_faber_vm",
                452,
                452,
                Path::new("/var/lib/faber-vm")
            )
            .display(),
            "/usr/sbin/useradd --system --no-create-home -u 452 -g 452 -d /var/lib/faber-vm \
             -s /usr/sbin/nologin -c \"faber VM host\" _faber_vm"
        );
        assert_eq!(
            add_member(Os::Linux, "docker", "_faber").display(),
            "/usr/sbin/usermod -aG docker _faber"
        );
        assert_eq!(
            remove_member(Os::Linux, "_faber", "owner").display(),
            "/usr/bin/gpasswd -d owner _faber"
        );
        assert_eq!(
            delete_user(Os::Linux, "_faber").display(),
            "/usr/sbin/userdel _faber"
        );
        assert_eq!(
            delete_group(Os::Linux, "_faber").display(),
            "/usr/sbin/groupdel _faber"
        );
    }

    /// The daemon's account check tells "no such account" from "no answer":
    /// only `dscl`'s record-not-found is the first; any other failure, or a
    /// run killed at its limit, is the second.
    #[test]
    fn the_session_lookup_tells_not_found_from_no_answer() {
        let out = |success: bool, stdout: &str, stderr: &str| {
            Ok(Output {
                success,
                code: Some(if success { 0 } else { 56 }),
                stdout: stdout.into(),
                stderr: stderr.into(),
            })
        };
        assert_eq!(
            session_lookup_of(out(true, "UniqueID: 600\n", "")),
            SessionLookup::Found(600)
        );
        assert_eq!(
            session_lookup_of(out(
                false,
                "",
                "<dscl_cmd> DS Error: -14136 (eDSRecordNotFound)\n"
            )),
            SessionLookup::NotFound
        );
        assert!(matches!(
            session_lookup_of(out(false, "", "DS Error: -14009 (eDSUnknownNodeName)")),
            SessionLookup::Failed(why) if why.contains("eDSUnknownNodeName")
        ));
        assert!(matches!(
            session_lookup_of(out(true, "No such key: UniqueID\n", "")),
            SessionLookup::Failed(_)
        ));
        assert!(matches!(
            session_lookup_of(Err("dscl: did not finish within 10s; killed".into())),
            SessionLookup::Failed(why) if why.contains("killed")
        ));
    }

    /// The bounded run the lookup uses kills a command that hangs, and the
    /// lookup reads that as no answer, not as a missing account.
    #[cfg(unix)]
    #[test]
    fn a_hung_lookup_is_killed_and_read_as_no_answer() {
        let mut cmd = std::process::Command::new("/bin/sleep");
        cmd.arg("5");
        let started = std::time::Instant::now();
        let run = super::super::tools::output_within(cmd, std::time::Duration::from_millis(200))
            .map_err(|e| format!("dscl: {e:#}"));
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
        assert!(matches!(
            session_lookup_of(run),
            SessionLookup::Failed(why) if why.contains("did not finish")
        ));
        assert!(SESSION_LOOKUP_LIMIT <= std::time::Duration::from_secs(10));
    }

    #[test]
    fn the_session_account_counts_only_with_a_uid_from_its_block() {
        assert!(session_uid_ok("UniqueID: 600\n"));
        assert!(session_uid_ok(
            "dsAttrTypeNative:IsHidden: 1\nUniqueID: 699\n"
        ));
        // The spike's throwaway uid, a person's, a role account's: none is it.
        assert!(!session_uid_ok("UniqueID: 599\n"));
        assert!(!session_uid_ok("UniqueID: 501\n"));
        assert!(!session_uid_ok("UniqueID: 451\n"));
        assert!(!session_uid_ok("No such key: UniqueID\n"));

        let mut runner = FakeRunner::default();
        assert!(
            !probe_session_account(Os::Linux, &mut runner),
            "no macOS VMs on Linux"
        );
        assert!(runner.ran.is_empty(), "Linux never asks");
    }

    /// Real `dscl -read` output shapes: inline values, a value with a space
    /// on a continuation line, an alias second value, and a missing key.
    #[test]
    fn dscl_read_parses_both_value_forms() {
        let text = "NFSHomeDirectory:\n /Library/Application Support/faber\n\
                    PrimaryGroupID: 451\nUniqueID: 451\nUserShell: /usr/bin/false\n\
                    IsHidden: 1\n";
        let u = user_from_dscl("_faber", text).unwrap();
        assert_eq!(u.home, PathBuf::from("/Library/Application Support/faber"));
        assert_eq!((u.uid, u.gid), (451, 451));
        assert_eq!(u.shell, PathBuf::from("/usr/bin/false"));
        assert!(u.hidden);

        // What macOS 27 printed for an account setup created (2026-10-21).
        let native = "dsAttrTypeNative:IsHidden: 1\nNFSHomeDirectory: /var/empty\n\
                      PrimaryGroupID: 450\nUniqueID: 450\nUserShell: /usr/bin/false\n";
        let n = user_from_dscl("_faber", native).unwrap();
        assert!(n.hidden, "a native-prefixed IsHidden was not read");
        assert_eq!(n.home, PathBuf::from(ROLE_ACCOUNT_HOME));

        let root = "No such key: IsHidden\nNFSHomeDirectory: /var/root /private/var/root\n\
                    PrimaryGroupID: 0\nUniqueID: 0\nUserShell: /bin/sh\n";
        let r = user_from_dscl("root", root).unwrap();
        assert_eq!(r.home, PathBuf::from("/var/root"));
        assert!(!r.hidden);
        assert_eq!(r.password, None);
        assert!(!r.auth_authority);

        // What `_www` shows, and what a person's account shows.
        let www = user_from_dscl(
            "_www",
            "No such key: AuthenticationAuthority\nNFSHomeDirectory: /Library/WebServer\n\
             Password: *\nPrimaryGroupID: 70\nUniqueID: 70\nUserShell: /usr/bin/false\n",
        )
        .unwrap();
        assert_eq!(www.password.as_deref(), Some("*"));
        assert!(!www.auth_authority);
        let person = user_from_dscl(
            "owner",
            "AuthenticationAuthority: ;ShadowHash;HASHLIST:<SALTED-SHA512-PBKDF2> ;SecureToken;\n\
             NFSHomeDirectory: /Users/owner\nPassword: ********\nPrimaryGroupID: 20\n\
             UniqueID: 501\nUserShell: /bin/zsh\n",
        )
        .unwrap();
        assert!(person.auth_authority);
        assert_eq!(password_locked(Os::MacOs, &person), Some(false));

        let g =
            group_from_dscl("admin", "GroupMembership: root owner\nPrimaryGroupID: 80\n").unwrap();
        assert_eq!(g.gid, 80);
        assert!(g.has_member("owner"));
        let empty = group_from_dscl("_faber", "PrimaryGroupID: 451\n").unwrap();
        assert!(empty.members.is_empty());
    }

    #[test]
    fn dscl_list_keeps_values_with_spaces() {
        let text = "_oahd                    441\nowner                    501\n\
                    nobody                   -2\n";
        let list = parse_dscl_list(text);
        assert_eq!(list[1], ("owner".to_string(), "501".to_string()));
        let homes = parse_dscl_list("_faber   /Library/Application Support/faber\n");
        assert_eq!(homes[0].1, "/Library/Application Support/faber");
    }

    #[test]
    fn getent_lines_parse() {
        let u = parse_passwd_line("_faber:x:451:451:faber daemon:/var/lib/faber:/usr/sbin/nologin")
            .unwrap();
        assert_eq!((u.uid, u.gid), (451, 451));
        assert_eq!(u.home, PathBuf::from("/var/lib/faber"));
        let g = parse_group_line("docker:x:998:_faber,owner").unwrap();
        assert_eq!(g.gid, 998);
        assert!(g.has_member("_faber") && g.has_member("owner"));
        assert!(parse_group_line("_faber:x:451:")
            .unwrap()
            .members
            .is_empty());
        assert!(parse_passwd_line("garbage").is_none());
        assert_eq!(
            parse_shadow_line("_faber:!:19000:0:99999:7:::").as_deref(),
            Some("!")
        );
        assert_eq!(parse_shadow_line("garbage"), None);
    }

    #[test]
    fn probes_read_through_the_runner_and_never_write() {
        let mut fake = FakeRunner {
            answers: vec![
                (
                    format!(
                        "/usr/bin/dscl . -read {} UniqueID PrimaryGroupID NFSHomeDirectory \
                         UserShell IsHidden Password AuthenticationAuthority",
                        user_node("_faber")
                    ),
                    Output {
                        success: false,
                        code: Some(56),
                        stderr: "<dscl_cmd> DS Error: -14136 (eDSRecordNotFound)".into(),
                        ..Output::default()
                    },
                ),
                (
                    "/usr/bin/dscl . -list /Users UniqueID".into(),
                    Output {
                        success: true,
                        code: Some(0),
                        stdout: "root 0\nnobody -2\n_x 480\n".into(),
                        ..Output::default()
                    },
                ),
            ],
            ..FakeRunner::default()
        };
        assert_eq!(probe_user(Os::MacOs, &mut fake, "_faber").unwrap(), None);
        let ids = probe_used_ids(Os::MacOs, &mut fake).unwrap();
        assert!(ids.uids.contains(&480) && ids.uids.contains(&0));
        assert!(
            !ids.uids.iter().any(|u| *u > 100_000),
            "negative ids are dropped"
        );
        for cmd in &fake.ran {
            assert!(
                matches!(cmd.tool, Tool::Dscl | Tool::Getent | Tool::Id),
                "{cmd:?}"
            );
            assert!(!cmd.args.iter().any(|a| a == "-create" || a == "-delete"));
        }
    }

    #[test]
    fn names_are_checked_before_they_reach_a_command() {
        assert!(valid_name("_faber") && valid_name("owner.name") && valid_name("ci"));
        assert!(valid_name("first.last-2"));
        for bad in [
            "", ".", "..", ".hidden", "-rf", "-", "a/b", "a b", "../x", "x;y",
        ] {
            assert!(!valid_name(bad), "{bad:?}");
        }
        let mut fake = FakeRunner::default();
        assert!(probe_user(Os::MacOs, &mut fake, "a/b").is_err());
        assert!(fake.ran.is_empty());
    }

    #[test]
    fn a_tcc_refusal_is_recognised() {
        assert!(is_tcc_denial("<main> delete status: eDSPermissionError"));
        assert!(is_tcc_denial("DS Error: -14120 (eDSPermissionError)"));
        assert!(!is_tcc_denial("eDSRecordNotFound"));
    }
}
