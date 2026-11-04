//! ADR-0051's session account: the account whose own GUI login session runs
//! faber's macOS VMs, made and kept by `sudo faber setup` (#354), macOS only.
//!
//! macOS job VMs do not boot under a system daemon (`Failed to get current
//! host key`), so they start inside a login session. Never the operator's:
//! [`FABER_MAC_ACCOUNT`], a third account beside `_faber` and `_faber_vm`, has
//! a session of its own. What the spikes proved shapes it:
//!
//! - a uid from [`FABER_MAC_ID_RANGE`]: Background Task Management keeps no
//!   records for uids below 500, so a LaunchAgent of one never runs, which is
//!   why `sysadminctl -roleAccount` (uids below 500 only) cannot make it and
//!   setup uses `dscl`;
//! - shell `/sbin/nologin`, not `/usr/bin/false`: PAM refuses the latter at
//!   the login window after the password verifies ("deactivated");
//! - not admin, never holds the GitHub token, a real password, primary group
//!   `staff`, home [`FABER_MAC_HOME`] (`_faber_mac:_faber` `0710`, like
//!   `_faber_vm`'s: `_faber` passes through to the supervisor's socket and
//!   lists nothing);
//! - created locked (`Password *`), so a run stopped before the password is
//!   set leaves an account nobody can log in as, never a passwordless one;
//! - kept off SSH by [`SSH_DENY`] (`DenyUsers`): its password and
//!   `/sbin/nologin` would otherwise still let `ssh -N -L` forward ports to
//!   loopback services while Remote Login is on. macOS's `sshd_config`
//!   includes `sshd_config.d/*`, and `sshd` starts per connection, so the
//!   drop-in holds from the next login.
//!
//! # Reboot modes
//!
//! After a reboot the account has no session until someone or something logs
//! it in, and without one no macOS VM runs. The operator picks how, with
//! `--macos-session <MODE>`, and changes it by running setup again:
//!
//! - **unlock**: the account gets a SecureToken, so it is listed at the
//!   FileVault screen, and unlocking the disk as it starts its session.
//!   Pre-selected when FileVault is on. `sysadminctl` asks the operator's
//!   password and the account's on the terminal itself; faber sees neither.
//! - **auto-login**: macOS logs it in at boot, behind the lock screen
//!   (`autoLoginUser`, `autoLoginUserScreenLocked`, and `/etc/kcpassword`,
//!   which setup writes itself because `sysadminctl -autologin` fails over
//!   SSH on macOS 27). FileVault off only, since macOS ignores auto-login
//!   while it is on. Never pre-selected: an explicit opt-in, as ADR-0012
//!   treats macOS auto-login.
//! - **manual**: a password only; the operator logs it in. Pre-selected when
//!   FileVault is off.
//!
//! Visibility follows the mode (visible for unlock and manual, hidden for
//! auto-login), and `--macos-session-visibility` overrides it. A hidden
//! account that a person logs in by hand needs the login window's "Other…"
//! field, so only then is `SHOWOTHERUSERS_MANAGED` turned on, its previous
//! value kept in `/etc/faber` for uninstall to put back.
//!
//! The password is typed by the operator straight into `dscl -passwd`, or
//! generated (`--macos-session-password generate`, pre-selected for
//! auto-login), set through `dscl`'s command loop on its stdin so it never
//! sits in an argument list, and shown once. faber stores it only where
//! auto-login needs it, `/etc/kcpassword`.
//!
//! # Its LaunchAgent
//!
//! The agent is the macOS VM supervisor in that session
//! ([`crate::platform::service::render_session_agent_plist`]): the
//! launcher's `supervise`, its directories in the account's home, no Linux VM.
//! Those directories mirror `_faber_vm`'s ([`SessionDir`]): `run/`
//! (`_faber_mac:_faber` `0710`, where the supervisor's socket is), `state/` and
//! `clones/` (the account's own `0700`, as the launcher requires).
//!
//! Setup records the account in the setup record ([`SessionAccount`]), and
//! from then on the daemon runs every macOS VM job through this supervisor
//! ([`crate::platform::session_supervisor_socket`]), and refuses them while it
//! does not answer: ADR-0051's "no VM session" gate. A record without the
//! account keeps the old route, `_faber_vm`'s supervisor.
//!
//! # Shape
//!
//! Like [`super::vm_runtime`], a sub-plan: [`plan_steps`] is pure, over a
//! [`SessionProbe`] from read-only probes, and its [`Step`]s print with the
//! rest of the plan. A host already as asked plans only `ok` steps, so a
//! re-run changes nothing. Setup is opt-in here: without `--macos-session`
//! (or one of its two companion flags) and with no such account, there are no
//! steps at all. [`apply`] runs last, after the service answers: a failure
//! here leaves faber itself set up, and a re-run plans only what is left.

use std::path::{Path, PathBuf};

use super::accounts::{self, UserInfo};
use super::plan::{self, step, Action, DirProbe, HostState, RecordProbe, Section, Status, Step};
use super::tools::{Cmd, Tool};
use crate::platform::layout::{
    Os, SystemLayout, FABER_MAC_ACCOUNT, FABER_MAC_HOME, FABER_MAC_ID_RANGE,
};
use crate::platform::record::SessionAccount;
use crate::platform::service;

/// The account's shell: a password login without an interactive shell.
pub const SHELL: &str = "/sbin/nologin";
/// `staff`, every macOS person's primary group.
pub const STAFF_GID: u32 = 20;
/// Its `RealName`, as the login window and the FileVault screen list it.
pub const REAL_NAME: &str = "faber macOS VMs";
/// The login window's settings, root's.
pub const LOGINWINDOW: &str = "/Library/Preferences/com.apple.loginwindow";
/// The obfuscated auto-login password macOS reads at boot.
pub const KCPASSWORD: &str = "/etc/kcpassword";
/// `SHOWOTHERUSERS_MANAGED` as it was before setup turned it on, in
/// `/etc/faber`: `absent`, `true` or `false`.
pub const SHOW_OTHER_PREV: &str = "session-loginwindow.prev";
/// The sshd drop-in that keeps the account off SSH.
pub const SSH_DENY: &str = "/etc/ssh/sshd_config.d/faber-mac.conf";
/// [`SSH_DENY`]'s directory, as root's chain from `/` sees it (`/etc` is a
/// symlink).
pub const SSH_DENY_DIR: &str = "/private/etc/ssh/sshd_config.d";
/// [`SSH_DENY`]'s name in [`SSH_DENY_DIR`].
pub const SSH_DENY_FILE: &str = "faber-mac.conf";
/// `/var/db`, as root's chain from `/` sees it (`/var` is a symlink).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
const HOME_PARENT: &str = "/private/var/db";

/// [`SSH_DENY`]'s bytes.
pub fn ssh_deny_conf() -> String {
    format!(
        "# Written by `sudo faber setup` (ADR-0051); `sudo faber uninstall` removes it.\n\
         # {FABER_MAC_ACCOUNT} runs faber's macOS VMs in its login session and never uses SSH.\n\
         DenyUsers {FABER_MAC_ACCOUNT}\n"
    )
}

/// [`SSH_DENY`] as uninstall removes it: through root's chain.
pub fn ssh_deny_real() -> PathBuf {
    Path::new(SSH_DENY_DIR).join(SSH_DENY_FILE)
}

/// The LaunchAgent's file, in the account's `Library/LaunchAgents`.
pub fn agent_file() -> String {
    format!("{}.plist", service::SESSION_AGENT_LABEL)
}
/// The agent's log, in the account's home.
pub fn agent_log() -> String {
    format!("{FABER_MAC_HOME}/supervisor.log")
}
/// The agent's path.
pub fn agent_path() -> PathBuf {
    Path::new(FABER_MAC_HOME)
        .join("Library/LaunchAgents")
        .join(agent_file())
}

/// How the account's session comes back after a reboot (ADR-0051).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum RebootMode {
    /// Unlock the disk as the account at the FileVault screen.
    Unlock,
    /// macOS logs it in at boot, behind the lock screen. FileVault off only.
    AutoLogin,
    /// A person logs it in.
    Manual,
}

impl RebootMode {
    pub fn name(self) -> &'static str {
        match self {
            RebootMode::Unlock => "unlock",
            RebootMode::AutoLogin => "auto-login",
            RebootMode::Manual => "manual",
        }
    }
}

/// Whether the login window lists the account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Visibility {
    Visible,
    Hidden,
}

/// Where the account's password comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum PasswordSource {
    /// The operator types it into `dscl -passwd`; faber never sees it.
    Typed,
    /// faber generates it, sets it, and shows it once.
    Generate,
}

/// What the operator asked for on the command line.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionOpts {
    /// Any of the session flags was given: make the account if it is missing.
    pub requested: bool,
    /// `None`: keep the account's current mode, or pre-select one.
    pub mode: Option<RebootMode>,
    pub visibility: Option<Visibility>,
    /// `Some` sets the password again on an account that has one.
    pub password: Option<PasswordSource>,
}

/// The account's home, as it is.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum HomeState {
    #[default]
    Missing,
    /// The account's own directory, group `_faber`, `0710`.
    Ok,
    /// The account's own directory, with another group or mode.
    Differs { gid: u32, mode: u32 },
    /// Something else there: not a directory, or not the account's.
    Foreign(String),
    /// Could not be read.
    Unknown,
}

/// A file setup writes, compared with what it would write.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FileState {
    #[default]
    Missing,
    Same,
    Differs,
    /// Unreadable without sudo.
    Unknown,
}

/// One of the session supervisor's directories in the account's home: what
/// [`crate::setup::vm_runtime::VmDir`] is to `_faber_vm`'s, without the Linux
/// VM's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionDir {
    /// The supervisor's socket: `_faber_mac:_faber` `0710`.
    Run,
    /// Slot locks and claim markers: the account's own `0700`.
    State,
    /// The macOS clones: the account's own `0700`.
    Clones,
}

impl SessionDir {
    pub const ALL: [SessionDir; 3] = [SessionDir::Run, SessionDir::State, SessionDir::Clones];

    pub fn name(self) -> &'static str {
        match self {
            SessionDir::Run => "run",
            SessionDir::State => "state",
            SessionDir::Clones => "clones",
        }
    }

    pub fn path(self) -> PathBuf {
        Path::new(FABER_MAC_HOME).join(self.name())
    }

    /// Its group: `_faber`'s for `run/`, `staff` otherwise.
    pub fn gid(self, faber_gid: u32) -> u32 {
        match self {
            SessionDir::Run => faber_gid,
            _ => STAFF_GID,
        }
    }

    pub fn mode(self) -> u32 {
        match self {
            SessionDir::Run => 0o710,
            _ => 0o700,
        }
    }

    fn shown(self) -> String {
        format!(
            "{FABER_MAC_ACCOUNT}:{} {:o}",
            match self {
                SessionDir::Run => "_faber",
                _ => "staff",
            },
            self.mode()
        )
    }
}

/// Everything the session sub-plan reads from the host. Gathered by
/// read-only probes ([`probe::gather`]); fabricated in tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionProbe {
    pub user: Option<UserInfo>,
    /// `fdesetup status`; `None` when it could not be read.
    pub filevault: Option<bool>,
    /// The account's SecureToken; `None` when unknown or no account.
    pub secure_token: Option<bool>,
    /// The login window's `autoLoginUser`.
    pub autologin_user: Option<String>,
    /// `autoLoginUserScreenLocked` is on.
    pub autologin_locked: bool,
    /// `/etc/kcpassword` exists.
    pub kcpassword: bool,
    /// `SHOWOTHERUSERS_MANAGED`; `None` when the key is absent.
    pub show_other: Option<bool>,
    /// [`SHOW_OTHER_PREV`] exists: setup turned "Other…" on and kept the
    /// previous value.
    pub show_other_saved: bool,
    pub home: HomeState,
    /// Each of [`SessionDir::ALL`]; one left out reads as missing.
    pub dirs: Vec<(SessionDir, DirProbe)>,
    pub agent: FileState,
    /// [`SSH_DENY`].
    pub ssh_deny: FileState,
}

/// One change to the session account. Executed by [`apply`] in a fixed order,
/// not plan order: the account, its home, the SecureToken off, the password,
/// the SecureToken on, auto-login, "Other…", the agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionAction {
    /// A `dscl` command on the account's record.
    Account(Cmd),
    /// Write [`SSH_DENY`], root's `0644`.
    SshDeny,
    /// Create or fix [`FABER_MAC_HOME`], `_faber_mac:_faber` `0710`.
    EnsureHome {
        uid: u32,
    },
    /// Create or fix one of the supervisor's directories in it.
    EnsureDir {
        uid: u32,
        dir: SessionDir,
    },
    /// `sysadminctl -secureTokenOff`, interactive, then `updatePreboot`.
    TokenOff,
    SetPassword(PasswordSource),
    /// `sysadminctl -secureTokenOn`, interactive, then `updatePreboot`.
    TokenOn,
    /// Remove the login window's auto-login keys and `/etc/kcpassword`.
    AutoLoginOff,
    /// Write `/etc/kcpassword` from this run's generated password, then the
    /// login window's keys.
    AutoLoginOn,
    /// Keep `prev` (when it is not kept already), then turn "Other…" on.
    ShowOtherOn {
        prev: Option<String>,
    },
    /// Put back what [`SHOW_OTHER_PREV`] kept, then remove it.
    ShowOtherRestore,
    /// Write the LaunchAgent, owned by the account.
    InstallAgent {
        uid: u32,
    },
    /// Restart the agent in the account's session onto what this run
    /// installed: `launchctl bootout gui/<uid>/<label>`, then bootstrapping it
    /// again (#479). Nothing when no session is up: the agent starts on the
    /// new launcher at the next login.
    RestartAgent {
        uid: u32,
    },
    /// Name the account in the setup record, or `None` to drop it. Not run by
    /// [`apply`]: the main apply writes it into the record before the daemon
    /// starts ([`crate::setup::plan::fold_session_record`]), so the daemon
    /// that starts is the one that reads it.
    Record(Option<SessionAccount>),
}

/// The account's current reboot mode, read from the host; `None` without an
/// account.
pub fn current_mode(p: &SessionProbe) -> Option<RebootMode> {
    p.user.as_ref()?;
    Some(if p.autologin_user.as_deref() == Some(FABER_MAC_ACCOUNT) {
        RebootMode::AutoLogin
    } else if p.secure_token == Some(true) {
        RebootMode::Unlock
    } else {
        RebootMode::Manual
    })
}

/// The mode setup picks when none is named and there is no account yet:
/// unlock with FileVault on, manual with it off. Never auto-login.
pub fn preselected(filevault: Option<bool>) -> Option<RebootMode> {
    filevault.map(|on| {
        if on {
            RebootMode::Unlock
        } else {
            RebootMode::Manual
        }
    })
}

/// The lowest uid in [`FABER_MAC_ID_RANGE`] no account uses.
pub fn pick_uid(used: &accounts::UsedIds) -> Option<u32> {
    FABER_MAC_ID_RANGE
        .clone()
        .find(|id| !used.uids.contains(id))
}

/// The LaunchAgent's bytes for this host: the session supervisor serving
/// `faber_uid`.
pub fn agent_plist(layout: &SystemLayout, faber_uid: u32) -> String {
    let argv =
        service::session_supervisor_argv(layout, &super::vm_runtime::supervisor_args(faber_uid));
    service::render_session_agent_plist(&argv, &agent_log())
}

fn node() -> String {
    accounts::user_node(FABER_MAC_ACCOUNT)
}

/// [`FABER_MAC_HOME`]'s name in [`HOME_PARENT`].
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn home_name() -> &'static str {
    FABER_MAC_HOME.rsplit('/').next().unwrap_or("faber-mac")
}

fn dscl_create(attr: &str, value: &str) -> Cmd {
    Cmd::new(
        Tool::Dscl,
        [
            ".".to_string(),
            "-create".into(),
            node(),
            attr.into(),
            value.into(),
        ],
    )
}

/// The `dscl` commands that make the account: the record, locked
/// (`Password *`) before anything else, then each attribute. No
/// `AuthenticationAuthority` yet: setting the password replaces the `*` and
/// sets it, so a run stopped before that leaves an account nobody can log in
/// as.
pub fn create_cmds(uid: u32, hidden: bool) -> Vec<Cmd> {
    let mut cmds = vec![
        Cmd::new(Tool::Dscl, [".".to_string(), "-create".into(), node()]),
        dscl_create("Password", "*"),
        dscl_create("UniqueID", &uid.to_string()),
        dscl_create("PrimaryGroupID", &STAFF_GID.to_string()),
        dscl_create("UserShell", SHELL),
        dscl_create("RealName", REAL_NAME),
        dscl_create("NFSHomeDirectory", FABER_MAC_HOME),
    ];
    if hidden {
        cmds.push(dscl_create("IsHidden", "1"));
    }
    cmds
}

fn unhide_cmd() -> Cmd {
    Cmd::new(
        Tool::Dscl,
        [".".to_string(), "-delete".into(), node(), "IsHidden".into()],
    )
}

/// `sysadminctl` turning the account's SecureToken on or off, asking both
/// passwords on the terminal (`-password -`, `-adminPassword -`).
pub fn token_cmd(on: bool, operator: &str) -> Cmd {
    Cmd::new(
        Tool::Sysadminctl,
        [
            if on {
                "-secureTokenOn"
            } else {
                "-secureTokenOff"
            },
            FABER_MAC_ACCOUNT,
            "-password",
            "-",
            "-adminUser",
            operator,
            "-adminPassword",
            "-",
        ],
    )
}

/// Refresh the Preboot volume, which holds the FileVault screen's user list.
pub fn update_preboot() -> Cmd {
    Cmd::new(Tool::Diskutil, ["apfs", "updatePreboot", "/"])
}

/// macOS's `/etc/kcpassword` obfuscation: the password, padded with NULs to
/// a multiple of 12 bytes (at least one NUL), XORed with Apple's fixed key.
/// It hides nothing from root; the file is root's `0600`.
pub fn kcpassword(password: &str) -> Vec<u8> {
    const KEY: [u8; 11] = [
        0x7D, 0x89, 0x52, 0x23, 0xD2, 0xBC, 0xDD, 0xEA, 0xA3, 0xB9, 0x1F,
    ];
    let mut bytes = password.as_bytes().to_vec();
    bytes.push(0);
    while !bytes.len().is_multiple_of(12) {
        bytes.push(0);
    }
    bytes
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ KEY[i % KEY.len()])
        .collect()
}

fn sec() -> Section {
    Section::Session
}

fn item() -> String {
    format!("user {FABER_MAC_ACCOUNT}")
}

/// The session sub-plan for `s`, from `p` and the operator's `o`. Empty unless
/// asked for or the account exists.
pub fn plan_steps(s: &HostState, p: &SessionProbe, o: &SessionOpts) -> Vec<Step> {
    let mut out = Vec::new();
    if !o.requested && p.user.is_none() {
        // A record naming an account that is gone would route macOS VM jobs to
        // a session nobody can start.
        out.extend(record_step(s, None));
        return out;
    }
    let refuse = |detail: String| step(sec(), Status::Refused, "macOS VM session", detail);
    if s.os != Os::MacOs {
        out.push(refuse(
            "--macos-session is macOS only: a Linux host runs no macOS VMs".to_string(),
        ));
        return out;
    }
    let faber_gid = match plan::planned_ids(s) {
        Ok((faber, _)) => faber.gid,
        Err(why) => {
            out.push(refuse(format!(
                "the session account's home is group _faber, and _faber's ids cannot be \
                 planned: {why}"
            )));
            return out;
        }
    };
    let current = current_mode(p);
    let Some(mode) = o.mode.or(current).or_else(|| preselected(p.filevault)) else {
        out.push(refuse(
            "cannot tell whether FileVault is on (`fdesetup status` failed), so no reboot mode \
             can be pre-selected: name one, `--macos-session unlock` or `--macos-session manual`"
                .to_string(),
        ));
        return out;
    };
    if mode == RebootMode::AutoLogin && p.filevault != Some(false) {
        out.push(refuse(
            "auto-login needs FileVault off: macOS ignores auto-login while FileVault is on. \
             Use `--macos-session unlock`, or turn FileVault off first"
                .to_string(),
        ));
        return out;
    }
    // An explicit choice wins; a re-run in the same mode keeps the
    // account's visibility; anything else follows the mode.
    let keeps = p.user.is_some() && o.mode.is_none_or(|m| Some(m) == current);
    let hidden = match (o.visibility, &p.user) {
        (Some(v), _) => v == Visibility::Hidden,
        (None, Some(u)) if keeps => u.hidden,
        _ => mode == RebootMode::AutoLogin,
    };
    let source = o.password.unwrap_or(if mode == RebootMode::AutoLogin {
        PasswordSource::Generate
    } else {
        PasswordSource::Typed
    });
    if mode == RebootMode::AutoLogin && source == PasswordSource::Typed {
        out.push(refuse(
            "auto-login keeps the password in /etc/kcpassword, which faber writes itself, so \
             faber has to know it: use `--macos-session-password generate` (shown once)"
                .to_string(),
        ));
        return out;
    }
    // Another account's auto-login is the operator's, and its kcpassword holds
    // that account's password: never overwritten, and uninstall could not
    // put it back.
    if let Some(other) = p
        .autologin_user
        .as_deref()
        .filter(|u| mode == RebootMode::AutoLogin && *u != FABER_MAC_ACCOUNT)
    {
        out.push(refuse(format!(
            "macOS already logs {other} in automatically{}, and setup never replaces another \
             account's auto-login: turn {other}'s auto-login off first (System Settings > \
             Users & Groups), or use `--macos-session unlock` or `--macos-session manual`",
            if p.kcpassword {
                format!(" ({KCPASSWORD} holds its password)")
            } else {
                String::new()
            }
        )));
        return out;
    }

    let how = match (o.mode, current) {
        (_, None) if o.mode.is_none() => format!(
            "{} (pre-selected: FileVault is {})",
            mode.name(),
            if p.filevault == Some(true) {
                "on"
            } else {
                "off"
            }
        ),
        (Some(m), Some(c)) if m != c => format!("{}, was {}", m.name(), c.name()),
        _ => mode.name().to_string(),
    };
    let next = login_step(mode);
    out.push(step(sec(), Status::Ok, "reboot mode", format!("{how}: {next}")).item("reboot mode"));

    // The account.
    let mut creates = false;
    let uid = match &p.user {
        None => {
            let Some(uid) = pick_uid(&s.ids) else {
                out.push(refuse(format!(
                    "no free uid in {}–{} for {FABER_MAC_ACCOUNT}",
                    FABER_MAC_ID_RANGE.start(),
                    FABER_MAC_ID_RANGE.end()
                )));
                return out;
            };
            creates = true;
            let detail = format!(
                "uid {uid}, group staff, shell {SHELL}, not admin, {}, home {FABER_MAC_HOME}",
                if hidden { "hidden" } else { "visible" }
            );
            for (n, cmd) in create_cmds(uid, hidden).into_iter().enumerate() {
                let st = step(sec(), Status::WillCreate, item(), cmd.display())
                    .doing(Action::Session(SessionAction::Account(cmd)))
                    .item(item());
                out.push(if n == 0 {
                    st.brief(detail.clone())
                } else {
                    st.brief("")
                });
            }
            uid
        }
        Some(u) => {
            if !FABER_MAC_ID_RANGE.contains(&u.uid) {
                out.push(refuse(format!(
                    "{FABER_MAC_ACCOUNT} exists with uid {}, outside {}–{}: it is not the \
                     account setup makes (Background Task Management ignores uids below 500). \
                     Remove or rename it, then re-run",
                    u.uid,
                    FABER_MAC_ID_RANGE.start(),
                    FABER_MAC_ID_RANGE.end()
                )));
                return out;
            }
            if s.admins.iter().any(|a| a == FABER_MAC_ACCOUNT) {
                out.push(refuse(format!(
                    "{FABER_MAC_ACCOUNT} is an admin; the session account must not be. Remove it \
                     from group admin (`sudo dseditgroup -o edit -d {FABER_MAC_ACCOUNT} -t user \
                     admin`), then re-run"
                )));
                return out;
            }
            let mut fixes = Vec::new();
            if u.shell != Path::new(SHELL) {
                fixes.push((
                    dscl_create("UserShell", SHELL),
                    format!(
                        "shell {} -> {SHELL}: PAM refuses /usr/bin/false at the login window",
                        u.shell.display()
                    ),
                ));
            }
            if u.gid != STAFF_GID {
                fixes.push((
                    dscl_create("PrimaryGroupID", &STAFF_GID.to_string()),
                    format!("primary group {} -> staff", u.gid),
                ));
            }
            if u.home != Path::new(FABER_MAC_HOME) {
                fixes.push((
                    dscl_create("NFSHomeDirectory", FABER_MAC_HOME),
                    format!("home {} -> {FABER_MAC_HOME}", u.home.display()),
                ));
            }
            if u.hidden != hidden {
                fixes.push(if hidden {
                    (dscl_create("IsHidden", "1"), "hidden".to_string())
                } else {
                    (unhide_cmd(), "visible".to_string())
                });
            }
            if fixes.is_empty() {
                out.push(
                    step(
                        sec(),
                        Status::Ok,
                        item(),
                        format!(
                            "uid {}, shell {SHELL}, not admin, {}",
                            u.uid,
                            if hidden { "hidden" } else { "visible" }
                        ),
                    )
                    .item(item()),
                );
            }
            for (cmd, why) in fixes {
                out.push(
                    step(
                        sec(),
                        Status::WillChange,
                        item(),
                        format!("{}: {why}", cmd.display()),
                    )
                    .doing(Action::Session(SessionAction::Account(cmd)))
                    .item(item())
                    .brief(why),
                );
            }
            u.uid
        }
    };

    // Its password. Auto-login being turned on needs one faber knows.
    let autologin_ours = p.autologin_user.as_deref() == Some(FABER_MAC_ACCOUNT);
    let autologin_done = autologin_ours && p.kcpassword && p.autologin_locked;
    let turns_autologin_on = mode == RebootMode::AutoLogin && !autologin_done;
    let token_after = mode == RebootMode::Unlock;
    let password_step = |status: Status, src: PasswordSource, why: &str| {
        step(
            sec(),
            status,
            format!("password of {FABER_MAC_ACCOUNT}"),
            match src {
                PasswordSource::Typed => format!("{why}: you type it into `dscl -passwd`"),
                PasswordSource::Generate => {
                    format!("{why}: generated, set through dscl's stdin, and shown once")
                }
            },
        )
        .doing(Action::Session(SessionAction::SetPassword(src)))
        .item("password")
    };
    if creates {
        out.push(password_step(Status::WillCreate, source, "set"));
    } else if o.password.is_some() || turns_autologin_on {
        let src = if turns_autologin_on {
            PasswordSource::Generate
        } else {
            source
        };
        if p.secure_token == Some(true) && token_after {
            out.push(refuse(format!(
                "{FABER_MAC_ACCOUNT} has a SecureToken, and setting its password as root would \
                 leave FileVault unlocking with the old one: change it in System Settings > \
                 Users & Groups, as an admin"
            )));
            return out;
        }
        let why = if turns_autologin_on && o.password.is_none() {
            "set again, since /etc/kcpassword needs a password faber knows"
        } else {
            "set again"
        };
        out.push(password_step(Status::WillChange, src, why));
    } else if s.privileged && !p.user.as_ref().is_some_and(|u| u.auth_authority) {
        out.push(password_step(
            Status::WillCreate,
            source,
            "set: it has none (an earlier run stopped before setting one)",
        ));
    } else {
        out.push(
            step(
                sec(),
                Status::Ok,
                format!("password of {FABER_MAC_ACCOUNT}"),
                if s.privileged {
                    "set".to_string()
                } else {
                    "set, if it has one: readable with sudo".to_string()
                },
            )
            .item("password"),
        );
    }

    // The SecureToken: wanted in unlock mode only.
    let operator_admin = match &s.operator {
        Ok(op) if s.admins.contains(&op.name) => Ok(op.name.clone()),
        Ok(op) => Err(format!("{} is not an admin", op.name)),
        Err(e) => Err(e.clone()),
    };
    let token_step = |status: Status, on: bool| {
        let detail = if on {
            "a SecureToken, so the account is listed at the FileVault screen and unlocking the \
             disk as it starts its session: sysadminctl asks your password, then the account's; \
             then `diskutil apfs updatePreboot /`"
        } else {
            "its SecureToken removed: it no longer unlocks the disk. sysadminctl asks your \
             password, then the account's; then `diskutil apfs updatePreboot /`"
        };
        step(
            sec(),
            status,
            format!("SecureToken of {FABER_MAC_ACCOUNT}"),
            detail,
        )
        .doing(Action::Session(if on {
            SessionAction::TokenOn
        } else {
            SessionAction::TokenOff
        }))
        .item("SecureToken")
    };
    let token_change = if token_after && p.secure_token != Some(true) {
        Some(token_step(Status::WillCreate, true))
    } else if !token_after && p.secure_token == Some(true) {
        Some(token_step(Status::WillChange, false))
    } else {
        None
    };
    match (token_change, &operator_admin) {
        (Some(_), Err(why)) => {
            out.push(refuse(format!(
                "changing {FABER_MAC_ACCOUNT}'s SecureToken needs an admin's password, and the \
                 operator cannot give one ({why}): run setup with sudo from an admin account"
            )));
            return out;
        }
        (Some(st), Ok(_)) => out.push(st),
        (None, _) if token_after => out.push(
            step(
                sec(),
                Status::Ok,
                format!("SecureToken of {FABER_MAC_ACCOUNT}"),
                "on",
            )
            .item("SecureToken"),
        ),
        (None, _) => {}
    }

    // Its home.
    let home_label = format!("home {FABER_MAC_HOME}");
    match &p.home {
        HomeState::Foreign(why) => {
            out.push(refuse(format!(
                "{FABER_MAC_HOME} {why}: setup gives the account only a directory that is \
                 missing or already its own. Move it aside, then re-run"
            )));
            return out;
        }
        HomeState::Missing => out.push(
            step(
                sec(),
                Status::WillCreate,
                home_label.clone(),
                format!("{FABER_MAC_ACCOUNT}:_faber 710"),
            )
            .doing(Action::Session(SessionAction::EnsureHome { uid }))
            .item(home_label.clone()),
        ),
        HomeState::Differs { gid, mode } => out.push(
            step(
                sec(),
                Status::WillChange,
                home_label.clone(),
                format!(
                    "group {gid} mode {mode:o} -> {FABER_MAC_ACCOUNT}:_faber 710, so _faber \
                     reaches the supervisor's socket"
                ),
            )
            .doing(Action::Session(SessionAction::EnsureHome { uid }))
            .item(home_label.clone()),
        ),
        HomeState::Ok | HomeState::Unknown => out.push(
            step(
                sec(),
                Status::Ok,
                home_label.clone(),
                format!("{FABER_MAC_ACCOUNT}:_faber 710"),
            )
            .item(home_label.clone()),
        ),
    }

    // The supervisor's directories in it.
    let home_new = creates || p.home == HomeState::Missing;
    for dir in SessionDir::ALL {
        let probe = if home_new {
            DirProbe::Missing
        } else {
            p.dirs
                .iter()
                .find(|(d, _)| *d == dir)
                .map(|(_, probe)| probe.clone())
                .unwrap_or(DirProbe::Missing)
        };
        match dir_step(s, dir, &probe, uid, faber_gid) {
            Ok(st) => out.push(st.item(home_label.clone())),
            Err(why) => {
                out.push(refuse(why));
                return out;
            }
        }
    }

    // Auto-login.
    if turns_autologin_on {
        out.push(
            step(
                sec(),
                Status::WillChange,
                "auto-login",
                format!(
                    "macOS logs {FABER_MAC_ACCOUNT} in at boot behind the lock screen: \
                     autoLoginUser, autoLoginUserScreenLocked and {KCPASSWORD} (root's \
                     600)"
                ),
            )
            .doing(Action::Session(SessionAction::AutoLoginOn))
            .item("auto-login"),
        );
    } else if mode != RebootMode::AutoLogin && autologin_ours {
        out.push(
            step(
                sec(),
                Status::WillChange,
                "auto-login",
                format!("off: autoLoginUser, autoLoginUserScreenLocked and {KCPASSWORD} removed"),
            )
            .doing(Action::Session(SessionAction::AutoLoginOff))
            .item("auto-login"),
        );
    } else if mode == RebootMode::AutoLogin {
        out.push(step(sec(), Status::Ok, "auto-login", "on").item("auto-login"));
    }

    // The login window's "Other…" field, for a hidden account logged in by
    // hand.
    let wants_other = hidden && mode != RebootMode::AutoLogin;
    if wants_other && p.show_other != Some(true) {
        let prev = (!p.show_other_saved).then(|| {
            match p.show_other {
                None => "absent",
                Some(true) => "true",
                Some(false) => "false",
            }
            .to_string()
        });
        out.push(
            step(
                sec(),
                Status::WillChange,
                "login window",
                "\"Other…\" shown, so the hidden account can be logged in by name; uninstall \
                 puts the previous setting back",
            )
            .doing(Action::Session(SessionAction::ShowOtherOn { prev }))
            .item("login window"),
        );
    } else if !wants_other && p.show_other_saved {
        out.push(
            step(
                sec(),
                Status::WillChange,
                "login window",
                "\"Other…\" set back as it was before setup",
            )
            .doing(Action::Session(SessionAction::ShowOtherRestore))
            .item("login window"),
        );
    }

    // SSH: a real password and /sbin/nologin still allow `ssh -N -L`.
    let ssh_detail = format!(
        "DenyUsers {FABER_MAC_ACCOUNT}, root's 644: with Remote Login on, its password and \
         {SHELL} would still let `ssh -N -L` forward ports to loopback services"
    );
    let ssh = |status: Status, detail: String| {
        let st = step(sec(), status, SSH_DENY, detail).item("SSH");
        if matches!(status, Status::WillCreate | Status::WillChange) {
            st.doing(Action::Session(SessionAction::SshDeny))
        } else {
            st
        }
    };
    out.push(match p.ssh_deny {
        FileState::Missing => ssh(Status::WillCreate, ssh_detail),
        FileState::Differs => ssh(Status::WillChange, ssh_detail),
        FileState::Same => ssh(Status::Ok, ssh_detail),
        FileState::Unknown => ssh(Status::Ok, format!("{ssh_detail}; readable with sudo")),
    });

    // The LaunchAgent.
    let agent = agent_path().display().to_string();
    let agent_state = if creates || p.home == HomeState::Missing {
        FileState::Missing
    } else {
        p.agent
    };
    let detail = "the macOS VM supervisor in the account's session: it starts when the account \
                  logs in, and the daemon runs macOS VM jobs through it";
    match agent_state {
        FileState::Missing => out.push(
            step(sec(), Status::WillCreate, agent.clone(), detail)
                .doing(Action::Session(SessionAction::InstallAgent { uid }))
                .item("LaunchAgent"),
        ),
        FileState::Differs => out.push(
            step(sec(), Status::WillChange, agent.clone(), detail)
                .doing(Action::Session(SessionAction::InstallAgent { uid }))
                .item("LaunchAgent"),
        ),
        FileState::Same => {
            out.push(step(sec(), Status::Ok, agent.clone(), detail).item("LaunchAgent"))
        }
        FileState::Unknown => out.push(
            step(
                sec(),
                Status::Ok,
                agent.clone(),
                format!("{detail}; readable with sudo"),
            )
            .item("LaunchAgent"),
        ),
    }

    out.extend(record_step(
        s,
        Some(SessionAccount {
            uid,
            gid: STAFF_GID,
            reboot_mode: Some(mode.name().to_string()),
        }),
    ));
    out
}

/// The session supervisor's restart, when this run changes what it runs: the
/// launcher, the network plan it reads at start, or its own agent file (the
/// same triggers as `_faber_vm`'s supervisor, plus the agent). `main` is the
/// whole plan, the session sub-plan included; nothing without an account that
/// stays. Planned after [`plan_steps`].
pub fn restart_steps(main: &[Step], p: &SessionProbe) -> Vec<Step> {
    use crate::setup::vm_runtime::VmAction;
    let Some(user) = p
        .user
        .as_ref()
        .filter(|u| FABER_MAC_ID_RANGE.contains(&u.uid))
    else {
        return Vec::new();
    };
    let changing = |st: &&Step| matches!(st.status, Status::WillCreate | Status::WillChange);
    let refused = main.iter().any(|st| st.status == Status::Refused);
    let why: Vec<&str> = main
        .iter()
        .filter(changing)
        .filter_map(|st| match &st.action {
            Some(Action::Vm(VmAction::InstallLauncher)) => Some("a new launcher"),
            Some(Action::Vm(VmAction::WritePlan)) => Some("a new network plan"),
            Some(Action::Session(SessionAction::InstallAgent { .. })) => Some("a new agent file"),
            _ => None,
        })
        .collect();
    if why.is_empty() || refused {
        return Vec::new();
    }
    vec![step(
        sec(),
        Status::WillChange,
        "session supervisor",
        format!(
            "restarted (bootout + bootstrap gui/{}/{}) if {FABER_MAC_ACCOUNT} has a session, onto {}; \
             without one it starts on them at the next login. Its VMs keep running",
            user.uid,
            service::SESSION_AGENT_LABEL,
            why.join(" and ")
        ),
    )
    .doing(Action::Session(SessionAction::RestartAgent {
        uid: user.uid,
    }))
    .item("LaunchAgent")]
}

/// What the operator does so the account has a session, by reboot mode: the
/// plan's and setup's last word on it.
pub fn login_step(mode: RebootMode) -> String {
    match mode {
        RebootMode::Unlock => format!(
            "after a reboot, unlock the disk as {REAL_NAME} at the FileVault screen; that \
             starts its session"
        ),
        RebootMode::AutoLogin => "macOS logs it in at boot, behind the lock screen".to_string(),
        RebootMode::Manual => {
            format!("log {REAL_NAME} in yourself, now and after every reboot")
        }
    }
}

/// The step for one of the supervisor's directories, or why it is refused.
fn dir_step(
    s: &HostState,
    dir: SessionDir,
    probe: &DirProbe,
    uid: u32,
    faber_gid: u32,
) -> Result<Step, String> {
    let subject = dir.path().display().to_string();
    let action = Action::Session(SessionAction::EnsureDir { uid, dir });
    Ok(match probe {
        DirProbe::Missing => step(sec(), Status::WillCreate, subject, dir.shown()).doing(action),
        DirProbe::Dir {
            uid: have,
            gid,
            mode,
        } => {
            if *have != uid {
                return Err(format!(
                    "{subject} is owned by uid {have}, not {FABER_MAC_ACCOUNT}: something else \
                     put it there. If nothing in it is yours, remove it (`sudo rm -rf \
                     \"{subject}\"`) and re-run"
                ));
            }
            if (*gid, mode & 0o7777) == (dir.gid(faber_gid), dir.mode()) {
                step(sec(), Status::Ok, subject, dir.shown())
            } else {
                step(
                    sec(),
                    Status::WillChange,
                    subject,
                    format!("{have}:{gid} {:o} -> {}", mode & 0o7777, dir.shown()),
                )
                .doing(action)
            }
        }
        DirProbe::NotADir(what) => {
            return Err(format!(
                "{subject}: {what} where a directory belongs; move it aside and re-run"
            ))
        }
        DirProbe::Unknown(e) => plan::unknown(s, sec(), subject, e),
    })
}

/// The setup record's step for the session account, `None` when the record
/// already says `want`. A record this run writes afresh starts with whatever
/// the previous one said ([`crate::setup::plan::record_for`]).
fn record_step(s: &HostState, want: Option<SessionAccount>) -> Option<Step> {
    let have = match &s.record {
        RecordProbe::Trusted(r) => r.accounts.faber_mac.clone(),
        _ => None,
    };
    if have == want {
        return None;
    }
    let detail = match &want {
        Some(a) => format!(
            "names {FABER_MAC_ACCOUNT} (uid {}, reboot mode {}): the daemon runs macOS VM jobs \
             through its session's supervisor, and refuses them while that does not answer",
            a.uid,
            a.reboot_mode.as_deref().unwrap_or("unknown")
        ),
        None => format!(
            "no longer names {FABER_MAC_ACCOUNT}, which is gone: macOS VM jobs go back to \
             {}'s supervisor",
            crate::platform::layout::FABER_VM_ACCOUNT
        ),
    };
    Some(
        step(
            sec(),
            if have.is_none() {
                Status::WillCreate
            } else {
                Status::WillChange
            },
            "setup record",
            detail,
        )
        .doing(Action::Session(SessionAction::Record(want)))
        .item("setup record"),
    )
}

/// What the plan says at a glance about the session account.
pub fn glance(steps: &[&Step]) -> Option<(bool, String)> {
    let changing = steps
        .iter()
        .any(|st| matches!(st.status, Status::WillCreate | Status::WillChange));
    if !changing {
        return None;
    }
    let creates = steps.iter().any(|st| {
        st.status == Status::WillCreate
            && matches!(&st.action, Some(Action::Session(SessionAction::Account(_))))
    });
    let mode = steps
        .iter()
        .find(|st| st.subject == "reboot mode")
        .map(|st| st.detail.split(':').next().unwrap_or("").to_string())
        .unwrap_or_default();
    Some((
        creates,
        if creates {
            format!(
                "{FABER_MAC_ACCOUNT}, the account whose login session runs macOS VMs \
                 (ADR-0051), reboot mode {mode}"
            )
        } else {
            format!("the macOS VM session account is brought into shape, reboot mode {mode}")
        },
    ))
}

/// What `sudo faber uninstall` finds of the session account.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionUninstall {
    pub user: Option<UserInfo>,
    /// The account is in group `admin`, so not one setup made.
    pub admin: bool,
    /// Its GUI session is up (`launchctl print gui/<uid>`).
    pub session_up: bool,
    /// The login window logs it in at boot.
    pub autologin_ours: bool,
    /// [`SHOW_OTHER_PREV`] exists.
    pub show_other_saved: bool,
    /// [`FABER_MAC_HOME`] exists.
    pub home: bool,
    /// It holds a SecureToken, so the FileVault screen lists it.
    pub secure_token: bool,
    /// Its `GeneratedUID`, read before it is deleted: the APFS crypto user
    /// uninstall checks is gone afterwards.
    pub generated_uid: Option<String>,
    /// [`SSH_DENY`] exists.
    pub ssh_deny: bool,
}

/// One of uninstall's session steps that is not a plain file or account step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionUndo {
    /// `launchctl bootout gui/<uid>`: the account's session ends, and with it
    /// anything its agent started.
    Logout { uid: u32 },
    /// The login window's auto-login keys and `/etc/kcpassword`.
    AutoLoginOff,
    /// "Other…" back as [`SHOW_OTHER_PREV`] kept it.
    RestoreShowOther,
    /// `diskutil apfs updatePreboot /`, once a SecureToken holder is gone.
    UpdatePreboot,
    /// Once the account is gone, check [`list_crypto_users`] no longer lists
    /// its `GeneratedUID`, and fail with the `fdesetup remove` that would
    /// remove it if it does: an APFS crypto user left behind could still
    /// unlock the disk with the account's password.
    CheckCryptoUser { uuid: String },
}

/// The volume's APFS crypto users: the accounts that can unlock it.
pub fn list_crypto_users() -> Cmd {
    Cmd::new(Tool::Diskutil, ["apfs", "listCryptoUsers", "/"])
}

/// The `GeneratedUID` in `dscl . -read <node> GeneratedUID`'s output, when it
/// is a UUID.
pub fn parse_generated_uid(text: &str) -> Option<String> {
    let uuid = text
        .lines()
        .find_map(|l| l.strip_prefix("GeneratedUID:"))?
        .trim();
    let shape = uuid.len() == 36
        && uuid.chars().enumerate().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        });
    shape.then(|| uuid.to_ascii_uppercase())
}

/// Whether `listing`, [`list_crypto_users`]'s output, names `uuid`.
pub fn lists_crypto_user(listing: &str, uuid: &str) -> bool {
    listing
        .to_ascii_uppercase()
        .contains(&uuid.to_ascii_uppercase())
}

/// Why `user` is not the session account setup makes, if it is not: the name,
/// a uid from [`FABER_MAC_ID_RANGE`], and not admin. Uninstall deletes only
/// that one.
pub fn account_is_ours(user: &UserInfo, admin: bool) -> Result<(), String> {
    if user.name != FABER_MAC_ACCOUNT {
        return Err(format!("user {}: not faber's session account", user.name));
    }
    if !FABER_MAC_ID_RANGE.contains(&user.uid) {
        return Err(format!(
            "user {} has uid {}, outside {}-{}: not the account setup creates",
            user.name,
            user.uid,
            FABER_MAC_ID_RANGE.start(),
            FABER_MAC_ID_RANGE.end()
        ));
    }
    if admin {
        return Err(format!(
            "user {} is an admin: not the account setup creates",
            user.name
        ));
    }
    Ok(())
}

/// The probes, read-only, macOS only.
#[cfg(target_os = "macos")]
pub mod probe {
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;

    use anyhow::{bail, Context, Result};

    use super::*;
    use crate::setup::fsx::{self, Dir};
    use crate::setup::tools::Runner;

    fn read_default(runner: &mut dyn Runner, key: &str) -> Option<String> {
        runner
            .run(&Cmd::new(Tool::Defaults, ["read", LOGINWINDOW, key]))
            .ok()
            .filter(|o| o.success)
            .map(|o| o.stdout.trim().to_string())
    }

    /// Whether `fdesetup status` says FileVault is on.
    pub fn filevault(text: &str) -> Option<bool> {
        if text.contains("FileVault is On") {
            Some(true)
        } else if text.contains("FileVault is Off") {
            Some(false)
        } else {
            None
        }
    }

    /// Whether `sysadminctl -secureTokenStatus` says the token is on.
    pub fn token(text: &str) -> Option<bool> {
        if text.contains("is ENABLED") {
            Some(true)
        } else if text.contains("is DISABLED") {
            Some(false)
        } else {
            None
        }
    }

    pub fn secure_token(runner: &mut dyn Runner) -> Option<bool> {
        let out = runner
            .run(&Cmd::new(
                Tool::Sysadminctl,
                ["-secureTokenStatus", FABER_MAC_ACCOUNT],
            ))
            .ok()?;
        token(&out.text())
    }

    /// The home's state, read without following a symlink: the account's,
    /// group `faber_gid`, `0710`.
    pub fn home(uid: Option<u32>, faber_gid: Option<u32>) -> HomeState {
        match std::fs::symlink_metadata(FABER_MAC_HOME) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HomeState::Missing,
            Err(_) => HomeState::Unknown,
            Ok(m) if !m.file_type().is_dir() => {
                HomeState::Foreign("is not a directory".to_string())
            }
            Ok(m) if Some(m.uid()) != uid => {
                HomeState::Foreign(format!("is owned by uid {}, not the account", m.uid()))
            }
            Ok(m) if Some(m.gid()) != faber_gid || m.mode() & 0o7777 != 0o710 => {
                HomeState::Differs {
                    gid: m.gid(),
                    mode: m.mode() & 0o7777,
                }
            }
            Ok(_) => HomeState::Ok,
        }
    }

    /// The account's `GeneratedUID`; `None` when the record has none (dscl
    /// exits non-zero with "No such key") or is gone. Any other failure is an
    /// error.
    pub fn generated_uid(runner: &mut dyn Runner) -> Result<Option<String>> {
        let cmd = Cmd::new(
            Tool::Dscl,
            [
                ".".to_string(),
                "-read".into(),
                node(),
                "GeneratedUID".into(),
            ],
        );
        let out = runner.run(&cmd)?;
        if !out.success {
            let text = out.text();
            if text.contains("No such key") || text.contains("eDSRecordNotFound") {
                return Ok(None);
            }
            bail!("`{}` failed: {}", cmd.display(), out.sanitized(400));
        }
        Ok(parse_generated_uid(&out.stdout))
    }

    /// The account, or `None` when there is none. Any other failure to read
    /// it is an error: read as "no account", it would make setup create the
    /// record again with another uid, and uninstall leave it.
    fn user(runner: &mut dyn Runner) -> Result<Option<UserInfo>> {
        accounts::probe_user(Os::MacOs, runner, FABER_MAC_ACCOUNT)
            .with_context(|| format!("reading the session account {FABER_MAC_ACCOUNT}"))
    }

    /// What uninstall needs to know.
    pub fn gather_uninstall(
        layout: &SystemLayout,
        runner: &mut dyn Runner,
    ) -> Result<SessionUninstall> {
        /// macOS's `admin` group.
        const ADMIN_GID: u32 = 80;
        let user = user(runner)?;
        let (admin, session_up, secure_token, generated_uid) = match &user {
            Some(u) => (
                accounts::probe_groups_of(runner, FABER_MAC_ACCOUNT).contains(&ADMIN_GID),
                service::launchd::is_loaded(runner, &format!("gui/{}", u.uid)),
                secure_token(runner) == Some(true),
                generated_uid(runner)?,
            ),
            None => (false, false, false, None),
        };
        Ok(SessionUninstall {
            user,
            admin,
            session_up,
            autologin_ours: read_default(runner, "autoLoginUser").as_deref()
                == Some(FABER_MAC_ACCOUNT),
            show_other_saved: layout.etc_dir.join(SHOW_OTHER_PREV).exists(),
            home: std::fs::symlink_metadata(FABER_MAC_HOME).is_ok(),
            secure_token,
            generated_uid,
            ssh_deny: std::fs::symlink_metadata(SSH_DENY).is_ok(),
        })
    }

    /// The regular file `name` in `dir` compared with `want`, never following
    /// a symlink there (`O_NOFOLLOW | O_NONBLOCK`) and reading at most one
    /// byte more than `want`: root reads it, and the directory may be another
    /// account's, which could otherwise point the read at a FIFO or
    /// `/dev/zero`.
    pub fn file_state_in(dir: &Dir, name: &str, want: &[u8]) -> FileState {
        use std::io::Read;
        let file = match dir.open_file(name) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return FileState::Missing,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                return FileState::Unknown
            }
            // A symlink (ELOOP) or anything else there: replaced on apply.
            Err(_) => return FileState::Differs,
        };
        match file.metadata() {
            Ok(m) if m.file_type().is_file() && m.len() == want.len() as u64 => {}
            _ => return FileState::Differs,
        }
        let mut bytes = Vec::with_capacity(want.len() + 1);
        match file.take(want.len() as u64 + 1).read_to_end(&mut bytes) {
            Ok(_) if bytes == want => FileState::Same,
            _ => FileState::Differs,
        }
    }

    /// The LaunchAgent in `home`, the account's own directory, compared with
    /// `want`: each directory opened beneath the last without following a
    /// symlink.
    pub fn agent_state_in(home: &Dir, want: &[u8]) -> FileState {
        let dir = home
            .subdir("Library")
            .and_then(|lib| lib.subdir("LaunchAgents"));
        match dir {
            Ok(dir) => file_state_in(&dir, &agent_file(), want),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileState::Missing,
            Err(_) => FileState::Differs,
        }
    }

    fn agent_state(want: &[u8]) -> FileState {
        let home =
            fsx::root_dir_chain_existing(Path::new(HOME_PARENT)).and_then(|parent| match parent {
                Some(p) => p.subdir(home_name()).map(Some),
                None => Ok(None),
            });
        match home {
            Ok(Some(home)) => agent_state_in(&home, want),
            Ok(None) => FileState::Missing,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileState::Missing,
            Err(_) => FileState::Differs,
        }
    }

    fn ssh_deny_state() -> FileState {
        match fsx::root_dir_chain_existing(Path::new(SSH_DENY_DIR)) {
            Ok(Some(dir)) => file_state_in(&dir, SSH_DENY_FILE, ssh_deny_conf().as_bytes()),
            Ok(None) => FileState::Missing,
            // Not root's alone: apply refuses it too, and says why.
            Err(_) => FileState::Differs,
        }
    }

    pub fn gather(
        s: &HostState,
        faber_uid: Option<u32>,
        runner: &mut dyn Runner,
    ) -> Result<SessionProbe> {
        let user = user(runner)?;
        let filevault = runner
            .run(&Cmd::new(Tool::Fdesetup, ["status"]))
            .ok()
            .and_then(|o| filevault(&o.stdout));
        let secure_token = user.as_ref().and_then(|_| secure_token(runner));
        let faber_gid = crate::setup::plan::planned_ids(s)
            .ok()
            .map(|(faber, _)| faber.gid);
        let home = home(user.as_ref().map(|u| u.uid), faber_gid);
        let dirs = SessionDir::ALL
            .iter()
            .map(|d| (*d, crate::setup::probe::probe_dir(&d.path())))
            .collect();
        let agent = match (&home, faber_uid) {
            (HomeState::Missing, _) => FileState::Missing,
            (_, None) => FileState::Unknown,
            (_, Some(faber)) if s.privileged => {
                agent_state(agent_plist(&s.layout, faber).as_bytes())
            }
            _ => FileState::Unknown,
        };
        Ok(SessionProbe {
            user,
            filevault,
            secure_token,
            autologin_user: read_default(runner, "autoLoginUser"),
            autologin_locked: read_default(runner, "autoLoginUserScreenLocked").as_deref()
                == Some("1"),
            kcpassword: std::fs::symlink_metadata(KCPASSWORD).is_ok(),
            show_other: read_default(runner, "SHOWOTHERUSERS_MANAGED").map(|v| v == "1"),
            show_other_saved: Path::new(&s.layout.etc_dir).join(SHOW_OTHER_PREV).exists(),
            home,
            dirs,
            agent,
            ssh_deny: ssh_deny_state(),
        })
    }
}

/// Executing the session steps, macOS only: the last thing setup changes
/// before the Linux image, once the service answers.
#[cfg(target_os = "macos")]
pub mod apply {
    use std::io::Cursor;
    use std::path::Path;

    use anyhow::{anyhow, bail, Context, Result};

    use super::*;
    use crate::setup::fsx::{self, Account, Dir, Staging, ROOT};
    use crate::setup::tools::Runner;

    /// `/etc`, as root's chain from `/` sees it (`/etc` is a symlink).
    const ETC: &str = "/private/etc";

    fn account(uid: u32) -> Account {
        Account {
            uid,
            gid: STAFF_GID,
        }
    }

    fn defaults(args: &[&str]) -> Cmd {
        Cmd::new(Tool::Defaults, args.iter().copied())
    }

    /// Execute the session steps of `steps`, in [`SessionAction`]'s order.
    pub fn apply(s: &HostState, steps: &[Step], runner: &mut dyn Runner) -> Result<()> {
        let actions: Vec<&SessionAction> = steps
            .iter()
            .filter(|st| matches!(st.status, Status::WillCreate | Status::WillChange))
            .filter_map(|st| match &st.action {
                // The main apply wrote the record already.
                Some(Action::Session(SessionAction::Record(_))) => None,
                Some(Action::Session(a)) => Some(a),
                _ => None,
            })
            .collect();
        if actions.is_empty() {
            return Ok(());
        }
        println!("\n  the macOS VM session account, {FABER_MAC_ACCOUNT} (ADR-0051)");
        let operator = || -> Result<String> {
            s.operator
                .as_ref()
                .map(|op| op.name.clone())
                .map_err(|e| anyhow!("{e}"))
        };
        let rank = |a: &SessionAction| match a {
            SessionAction::Account(_) => 0,
            // Off SSH before it has a password.
            SessionAction::SshDeny => 1,
            SessionAction::EnsureHome { .. } | SessionAction::EnsureDir { .. } => 2,
            SessionAction::TokenOff => 3,
            SessionAction::SetPassword(_) => 4,
            SessionAction::TokenOn => 5,
            SessionAction::AutoLoginOff => 6,
            SessionAction::AutoLoginOn => 7,
            SessionAction::ShowOtherOn { .. } | SessionAction::ShowOtherRestore => 8,
            SessionAction::InstallAgent { .. } | SessionAction::Record(_) => 9,
            SessionAction::RestartAgent { .. } => 10,
        };
        let mut ordered = actions;
        // Stable: account commands keep plan order.
        ordered.sort_by_key(|a| rank(a));
        let mut generated: Option<String> = None;
        for action in ordered {
            match action {
                SessionAction::Account(cmd) => {
                    println!("  {}", cmd.display());
                    runner.run_ok(cmd)?;
                }
                SessionAction::SshDeny => ssh_deny()?,
                SessionAction::EnsureHome { uid } => ensure_home(s, *uid)?,
                SessionAction::EnsureDir { uid, dir } => ensure_session_dir(s, *uid, *dir)?,
                SessionAction::TokenOff => set_token(runner, false, &operator()?)?,
                SessionAction::SetPassword(src) => {
                    generated = set_password(runner, *src)?;
                }
                SessionAction::TokenOn => set_token(runner, true, &operator()?)?,
                SessionAction::AutoLoginOff => autologin_off(runner)?,
                SessionAction::AutoLoginOn => {
                    let password = generated.as_deref().ok_or_else(|| {
                        anyhow!(
                            "auto-login needs the password this run generated, and none was: \
                             a bug in faber's plan"
                        )
                    })?;
                    autologin_on(runner, password)?;
                }
                SessionAction::ShowOtherOn { prev } => show_other_on(s, runner, prev.as_deref())?,
                SessionAction::ShowOtherRestore => show_other_restore(s, runner)?,
                SessionAction::InstallAgent { uid } => install_agent(s, *uid)?,
                SessionAction::Record(_) => {}
                SessionAction::RestartAgent { uid } => restart_agent(runner, *uid)?,
            }
        }
        // The operator's step, from what the host now says rather than from
        // the plan.
        let after = probe::gather(s, None, runner).ok();
        if let Some(mode) = after.as_ref().and_then(current_mode) {
            println!(
                "  reboot mode {}: {}. Until {FABER_MAC_ACCOUNT} has a session, faber refuses \
                 macOS VM jobs (\"no VM session\"); one already logged in starts the \
                 supervisor at its next login",
                mode.name(),
                login_step(mode)
            );
        }
        Ok(())
    }

    fn ssh_deny() -> Result<()> {
        let dir = fsx::root_dir_chain(Path::new(SSH_DENY_DIR))
            .with_context(|| format!("opening {SSH_DENY_DIR}"))?;
        write_ssh_deny_in(&dir, ROOT)?;
        println!("  {SSH_DENY}: DenyUsers {FABER_MAC_ACCOUNT}");
        Ok(())
    }

    /// Write [`SSH_DENY`] in `dir`, owned by `owner` at `0644`.
    pub fn write_ssh_deny_in(dir: &Dir, owner: Account) -> Result<()> {
        dir.write_atomic(
            SSH_DENY_FILE,
            &mut Cursor::new(ssh_deny_conf().into_bytes()),
            owner,
            0o644,
        )
        .with_context(|| format!("writing {SSH_DENY}"))?;
        Ok(())
    }

    /// Restart the agent in `uid`'s GUI session, if there is one.
    fn restart_agent(runner: &mut dyn Runner, uid: u32) -> Result<()> {
        let domain = format!("gui/{uid}");
        if !service::launchd::is_loaded(runner, &domain) {
            println!(
                "  {FABER_MAC_ACCOUNT} has no session: its supervisor starts on this launcher at \
                 the next login"
            );
            return Ok(());
        }
        let target = format!("{domain}/{}", service::SESSION_AGENT_LABEL);
        // An earlier release installed the agent disabled, so a session that
        // was already up never loaded it.
        service::launchd::enable(runner, &target)?;
        // Booted out and in again, never `kickstart -k`: the launcher it runs
        // was just replaced, and launchd would keep it pinned to the old code
        // (#479). Its VMs outlive it (`AbandonProcessGroup`).
        service::launchd::reload(
            runner,
            &domain,
            &target,
            &agent_path(),
            std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(1),
        )?;
        service::launchd::wait_running(
            runner,
            &target,
            std::time::Duration::from_secs(15),
            std::time::Duration::from_millis(500),
        )?;
        println!("  restarted {target}");
        Ok(())
    }

    fn faber_gid(s: &HostState) -> Result<u32> {
        crate::setup::plan::planned_ids(s)
            .map(|(faber, _)| faber.gid)
            .map_err(|e| anyhow!("{e}"))
    }

    fn ensure_home(s: &HostState, uid: u32) -> Result<()> {
        let parent = fsx::root_dir_chain(Path::new(HOME_PARENT))
            .with_context(|| format!("opening {HOME_PARENT}"))?;
        let owner = Account {
            uid,
            gid: faber_gid(s)?,
        };
        fsx::ensure_dir(&parent, home_name(), owner, 0o710, &[uid])
            .with_context(|| format!("creating {FABER_MAC_HOME}"))?;
        println!("  {FABER_MAC_HOME}: {FABER_MAC_ACCOUNT}:_faber 710");
        Ok(())
    }

    /// One of the supervisor's directories, placed from root's staging as the
    /// LaunchAgent's are: the home is the account's.
    fn ensure_session_dir(s: &HostState, uid: u32, dir: SessionDir) -> Result<()> {
        let parent = fsx::root_dir_chain(Path::new(HOME_PARENT))?;
        let home = parent
            .subdir(home_name())
            .with_context(|| format!("opening {FABER_MAC_HOME}"))?;
        let staging = Staging::new_in(fsx::root_dir_chain(Path::new(HOME_PARENT))?)?;
        let owner = Account {
            uid,
            gid: dir.gid(faber_gid(s)?),
        };
        fsx::ensure_dir_beneath(&staging, &home, dir.name(), owner, dir.mode())
            .with_context(|| format!("preparing {}", dir.path().display()))?;
        println!("  {}: {}", dir.path().display(), dir.shown());
        Ok(())
    }

    /// `pw` replaced by `***` wherever it appears in `text`.
    fn redact(text: &str, pw: &str) -> String {
        text.replace(pw, "***")
    }

    fn set_password(runner: &mut dyn Runner, src: PasswordSource) -> Result<Option<String>> {
        let generated = match src {
            PasswordSource::Typed => {
                println!("  type the new password for {FABER_MAC_ACCOUNT} ({REAL_NAME}):");
                let cmd = Cmd::new(Tool::Dscl, [".".to_string(), "-passwd".into(), node()]);
                let out = runner.run_interactive(&cmd)?;
                if !out.success {
                    bail!("`{}` failed: the password was not set", cmd.display());
                }
                None
            }
            PasswordSource::Generate => {
                let pw = crate::services::random_password()?;
                let cmd = Cmd::new(Tool::Dscl, ["."]);
                let input = format!("passwd {} {pw}\n", node());
                let out = runner.run_with_input(&cmd, input.as_bytes())?;
                // dscl's command loop exits 0 whatever a command did: its
                // output says.
                let text = out.text();
                if !out.success || text.contains("DS Error") || text.contains("passwd:") {
                    bail!(
                        "setting {FABER_MAC_ACCOUNT}'s password through dscl failed: {}",
                        crate::setup::tools::sanitize_child_output(&redact(&text, &pw), 400)
                    );
                }
                Some(pw)
            }
        };
        let user = accounts::probe_user(Os::MacOs, runner, FABER_MAC_ACCOUNT)?;
        if !user.is_some_and(|u| u.auth_authority) {
            bail!(
                "{FABER_MAC_ACCOUNT} still has no password after it was set (no \
                 AuthenticationAuthority)"
            );
        }
        if let Some(pw) = &generated {
            println!(
                "  {FABER_MAC_ACCOUNT}'s password, shown once: {pw}\n  Keep it: you type it at \
                 the login window or the FileVault screen, and faber does not show it again."
            );
        } else {
            println!("  {FABER_MAC_ACCOUNT}'s password is set");
        }
        Ok(generated)
    }

    fn set_token(runner: &mut dyn Runner, on: bool, operator: &str) -> Result<()> {
        let cmd = token_cmd(on, operator);
        println!(
            "  {} (asks your password, then {FABER_MAC_ACCOUNT}'s)",
            cmd.display()
        );
        runner.run_interactive(&cmd)?;
        // sysadminctl's exit status does not say whether the token changed;
        // its status report does.
        if probe::secure_token(runner) != Some(on) {
            bail!(
                "{FABER_MAC_ACCOUNT}'s SecureToken is still {} after `{}`",
                if on { "off" } else { "on" },
                cmd.display()
            );
        }
        runner.run_ok(&update_preboot())?;
        println!("  {}", update_preboot().display());
        Ok(())
    }

    fn etc_dir() -> Result<Dir> {
        fsx::root_dir_chain(Path::new(ETC)).with_context(|| format!("opening {ETC}"))
    }

    fn autologin_on(runner: &mut dyn Runner, password: &str) -> Result<()> {
        autologin_on_in(&etc_dir()?, ROOT, runner, password)
    }

    /// Auto-login on, with `etc` as `/etc` and `owner` owning kcpassword.
    /// When a login window key cannot be written, kcpassword is removed
    /// again: it holds the account's password, and with no `autoLoginUser`
    /// pointing at it, nothing would ever remove it.
    pub fn autologin_on_in(
        etc: &Dir,
        owner: Account,
        runner: &mut dyn Runner,
        password: &str,
    ) -> Result<()> {
        etc.write_atomic(
            "kcpassword",
            &mut Cursor::new(kcpassword(password)),
            owner,
            0o600,
        )
        .with_context(|| format!("writing {KCPASSWORD}"))?;
        let keys = (|| -> Result<()> {
            runner.run_ok(&defaults(&[
                "write",
                LOGINWINDOW,
                "autoLoginUser",
                "-string",
                FABER_MAC_ACCOUNT,
            ]))?;
            runner.run_ok(&defaults(&[
                "write",
                LOGINWINDOW,
                "autoLoginUserScreenLocked",
                "-bool",
                "true",
            ]))?;
            Ok(())
        })();
        if let Err(e) = keys {
            return Err(match etc.unlink("kcpassword") {
                Ok(()) => e.context(format!(
                    "turning auto-login on failed; {KCPASSWORD} was removed again"
                )),
                Err(u) if u.kind() == std::io::ErrorKind::NotFound => {
                    e.context("turning auto-login on failed")
                }
                Err(u) => e.context(format!(
                    "turning auto-login on failed, and removing {KCPASSWORD} failed too ({u}): \
                     remove it yourself, `sudo rm {KCPASSWORD}`"
                )),
            });
        }
        println!("  auto-login as {FABER_MAC_ACCOUNT}, behind the lock screen");
        Ok(())
    }

    /// Turn auto-login off: the keys, then the file. Shared with uninstall.
    pub fn autologin_off(runner: &mut dyn Runner) -> Result<()> {
        for key in ["autoLoginUser", "autoLoginUserScreenLocked"] {
            // A key already gone is the state asked for.
            let _ = runner.run(&defaults(&["delete", LOGINWINDOW, key]));
        }
        match etc_dir()?.unlink("kcpassword") {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => bail!("removing {KCPASSWORD}: {e}"),
        }
        println!("  auto-login off; {KCPASSWORD} removed");
        Ok(())
    }

    fn faber_etc(s: &HostState) -> Result<Dir> {
        faber_etc_of(&s.layout)
    }

    fn faber_etc_of(layout: &SystemLayout) -> Result<Dir> {
        let real = std::fs::canonicalize(&layout.etc_dir)
            .with_context(|| format!("resolving {}", layout.etc_dir.display()))?;
        fsx::root_dir_chain(&real).with_context(|| format!("opening {}", real.display()))
    }

    fn show_other_on(s: &HostState, runner: &mut dyn Runner, prev: Option<&str>) -> Result<()> {
        if let Some(prev) = prev {
            faber_etc(s)?
                .write_atomic(
                    SHOW_OTHER_PREV,
                    &mut Cursor::new(format!("{prev}\n")),
                    ROOT,
                    0o644,
                )
                .with_context(|| format!("keeping the previous setting in {SHOW_OTHER_PREV}"))?;
        }
        runner.run_ok(&defaults(&[
            "write",
            LOGINWINDOW,
            "SHOWOTHERUSERS_MANAGED",
            "-bool",
            "true",
        ]))?;
        println!("  the login window shows \"Other…\"");
        Ok(())
    }

    /// Put "Other…" back as it was, and forget the kept value.
    pub fn show_other_restore(s: &HostState, runner: &mut dyn Runner) -> Result<()> {
        restore_show_other_in(&faber_etc(s)?, runner)
    }

    /// One of uninstall's session steps.
    pub fn undo(step: &SessionUndo, layout: &SystemLayout, runner: &mut dyn Runner) -> Result<()> {
        match step {
            SessionUndo::Logout { uid } => {
                service::launchd::bootout(runner, &format!("gui/{uid}"))?;
                println!("  logged out {FABER_MAC_ACCOUNT}'s session");
            }
            SessionUndo::AutoLoginOff => autologin_off(runner)?,
            SessionUndo::RestoreShowOther => restore_show_other_in(&faber_etc_of(layout)?, runner)?,
            SessionUndo::UpdatePreboot => {
                runner.run_ok(&update_preboot())?;
                println!("  {}", update_preboot().display());
            }
            SessionUndo::CheckCryptoUser { uuid } => check_crypto_user(runner, uuid)?,
        }
        Ok(())
    }

    /// Fail unless [`list_crypto_users`] no longer lists `uuid`.
    pub fn check_crypto_user(runner: &mut dyn Runner, uuid: &str) -> Result<()> {
        let remove = format!("sudo fdesetup remove -uuid {uuid}");
        let out = runner.run(&list_crypto_users())?;
        if !out.success {
            bail!(
                "`{}` failed, so whether {FABER_MAC_ACCOUNT} ({uuid}) can still unlock the disk \
                 is unknown: run it yourself, and if it lists {uuid}, `{remove}`. {}",
                list_crypto_users().display(),
                out.sanitized(400)
            );
        }
        if lists_crypto_user(&out.text(), uuid) {
            bail!(
                "{FABER_MAC_ACCOUNT} is deleted, but the volume still lists it as a crypto user \
                 ({uuid}), so its password may still unlock the disk: remove it with `{remove}`"
            );
        }
        println!("  {FABER_MAC_ACCOUNT} ({uuid}) is no longer a crypto user of the volume");
        Ok(())
    }

    pub fn restore_show_other_in(etc: &Dir, runner: &mut dyn Runner) -> Result<()> {
        use std::io::Read;
        let mut text = String::new();
        match etc.open_file(SHOW_OTHER_PREV) {
            Ok(mut f) => {
                f.read_to_string(&mut text)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => bail!("reading {SHOW_OTHER_PREV}: {e}"),
        }
        match text.trim() {
            "true" => runner.run_ok(&defaults(&[
                "write",
                LOGINWINDOW,
                "SHOWOTHERUSERS_MANAGED",
                "-bool",
                "true",
            ]))?,
            "false" => runner.run_ok(&defaults(&[
                "write",
                LOGINWINDOW,
                "SHOWOTHERUSERS_MANAGED",
                "-bool",
                "false",
            ]))?,
            _ => runner.run(&defaults(&[
                "delete",
                LOGINWINDOW,
                "SHOWOTHERUSERS_MANAGED",
            ]))?,
        };
        etc.unlink(SHOW_OTHER_PREV)?;
        println!("  the login window's \"Other…\" setting is as it was before setup");
        Ok(())
    }

    fn install_agent(s: &HostState, uid: u32) -> Result<()> {
        let (faber, _) = crate::setup::plan::planned_ids(s).map_err(|e| anyhow!("{e}"))?;
        let bytes = agent_plist(&s.layout, faber.uid);
        let parent = fsx::root_dir_chain(Path::new(HOME_PARENT))?;
        let home = parent
            .subdir(home_name())
            .with_context(|| format!("opening {FABER_MAC_HOME}"))?;
        // The home is the account's: root makes nothing in it and reopens it,
        // so each directory is placed from root's own staging.
        let staging = Staging::new_in(fsx::root_dir_chain(Path::new(HOME_PARENT))?)?;
        let (library, _) =
            fsx::ensure_dir_beneath(&staging, &home, "Library", account(uid), 0o700)?;
        let (agents, _) =
            fsx::ensure_dir_beneath(&staging, &library, "LaunchAgents", account(uid), 0o755)?;
        agents
            .write_atomic(
                &agent_file(),
                &mut Cursor::new(bytes.into_bytes()),
                account(uid),
                0o644,
            )
            .with_context(|| format!("writing {}", agent_path().display()))?;
        println!(
            "  {}: the macOS VM supervisor, started when {FABER_MAC_ACCOUNT} logs in",
            agent_path().display()
        );
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::setup::tools::{FakeRunner, Output};

        fn ok(stdout: &str) -> Output {
            Output {
                success: true,
                code: Some(0),
                stdout: stdout.into(),
                ..Output::default()
            }
        }

        fn read_user() -> String {
            Cmd::new(
                Tool::Dscl,
                [
                    ".".to_string(),
                    "-read".into(),
                    node(),
                    "UniqueID".into(),
                    "PrimaryGroupID".into(),
                    "NFSHomeDirectory".into(),
                    "UserShell".into(),
                    "IsHidden".into(),
                    "Password".into(),
                    "AuthenticationAuthority".into(),
                ],
            )
            .display()
        }

        const SET: &str = "UniqueID: 600\nPrimaryGroupID: 20\nNFSHomeDirectory: \
                           /var/db/faber-mac\nUserShell: /sbin/nologin\nPassword: \
                           ********\nAuthenticationAuthority: ;ShadowHash;\n";

        #[test]
        fn a_generated_password_goes_through_stdin_and_is_checked() {
            let mut runner = FakeRunner {
                answers: vec![(read_user(), ok(SET))],
                ..FakeRunner::default()
            };
            let pw = set_password(&mut runner, PasswordSource::Generate)
                .unwrap()
                .unwrap();
            assert_eq!(pw.len(), 32);
            assert_eq!(runner.ran[0].display(), "/usr/bin/dscl .");
            assert!(
                runner
                    .ran
                    .iter()
                    .all(|c| !c.args.iter().any(|a| a.contains(&pw))),
                "the password is never an argument"
            );
            // dscl's loop exits 0 on a failed command; its output decides.
            let mut runner = FakeRunner {
                answers: vec![(
                    "/usr/bin/dscl .".into(),
                    ok("<dscl_cmd> DS Error: -14009 (eDSUnknownNodeName)\npasswd: Invalid Path"),
                )],
                ..FakeRunner::default()
            };
            assert!(set_password(&mut runner, PasswordSource::Generate).is_err());
            // Set, but no AuthenticationAuthority to show for it: refused.
            let mut runner = FakeRunner {
                answers: vec![(
                    read_user(),
                    ok("UniqueID: 600\nPrimaryGroupID: 20\nNFSHomeDirectory: /x\n"),
                )],
                ..FakeRunner::default()
            };
            assert!(set_password(&mut runner, PasswordSource::Generate).is_err());
        }

        #[test]
        fn a_typed_password_is_read_by_dscl_itself() {
            let mut runner = FakeRunner {
                answers: vec![(read_user(), ok(SET))],
                ..FakeRunner::default()
            };
            assert_eq!(
                set_password(&mut runner, PasswordSource::Typed).unwrap(),
                None
            );
            assert_eq!(
                runner.ran[0].display(),
                format!("/usr/bin/dscl . -passwd {}", node())
            );
        }

        const UUID: &str = "0A1B2C3D-4E5F-6789-ABCD-EF0123456789";

        /// A crypto user the deleted account left is reported, with the
        /// command that removes it; none left passes.
        #[test]
        fn the_deleted_accounts_crypto_user_is_checked_gone() {
            let listing = |uuids: &[&str]| {
                let mut text = format!("Cryptographic users for disk3s5 ({} found)\n", uuids.len());
                for u in uuids {
                    text.push_str(&format!(
                        "|\n+-- {u}\n|   Type: Local Open Directory User\n"
                    ));
                }
                text
            };
            let operator = "11111111-2222-3333-4444-555555555555";
            let mut runner = FakeRunner {
                answers: vec![(
                    list_crypto_users().display(),
                    ok(&listing(&[operator, UUID])),
                )],
                ..FakeRunner::default()
            };
            let err = check_crypto_user(&mut runner, UUID)
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(&format!("`sudo fdesetup remove -uuid {UUID}`")),
                "{err}"
            );
            let mut runner = FakeRunner {
                answers: vec![(list_crypto_users().display(), ok(&listing(&[operator])))],
                ..FakeRunner::default()
            };
            check_crypto_user(&mut runner, UUID).unwrap();
            // The listing itself failing is not a pass.
            let mut runner = FakeRunner {
                answers: vec![(list_crypto_users().display(), Output::default())],
                ..FakeRunner::default()
            };
            let err = check_crypto_user(&mut runner, UUID)
                .unwrap_err()
                .to_string();
            assert!(err.contains("fdesetup remove -uuid"), "{err}");
        }

        /// A failed login window write leaves no kcpassword behind; a
        /// successful one leaves it, the account's 0600.
        #[test]
        fn a_failed_auto_login_takes_kcpassword_back() {
            use crate::setup::fsx::tests::{me, Scratch};
            use std::os::unix::fs::PermissionsExt;
            let scratch = Scratch::new("session-autologin");
            let owner = me(&scratch);
            let locked = defaults(&[
                "write",
                LOGINWINDOW,
                "autoLoginUserScreenLocked",
                "-bool",
                "true",
            ])
            .display();
            let mut runner = FakeRunner {
                answers: vec![(locked, Output::default())],
                ..FakeRunner::default()
            };
            let err = autologin_on_in(&scratch.dir(), owner, &mut runner, "pw")
                .unwrap_err()
                .to_string();
            assert!(err.contains("removed again"), "{err}");
            assert!(!scratch.path("kcpassword").exists());

            let mut runner = FakeRunner::default();
            autologin_on_in(&scratch.dir(), owner, &mut runner, "pw").unwrap();
            let meta = std::fs::metadata(scratch.path("kcpassword")).unwrap();
            assert_eq!(meta.permissions().mode() & 0o7777, 0o600);
            assert_eq!(
                std::fs::read(scratch.path("kcpassword")).unwrap(),
                kcpassword("pw")
            );
        }

        #[test]
        fn the_ssh_drop_in_is_written_644() {
            use crate::setup::fsx::tests::{me, Scratch};
            use std::os::unix::fs::PermissionsExt;
            let scratch = Scratch::new("session-ssh");
            write_ssh_deny_in(&scratch.dir(), me(&scratch)).unwrap();
            let path = scratch.path(SSH_DENY_FILE);
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
                0o644
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), ssh_deny_conf());
            assert_eq!(
                probe::file_state_in(&scratch.dir(), SSH_DENY_FILE, ssh_deny_conf().as_bytes()),
                FileState::Same
            );
        }

        /// The agent probe, root's, never follows a symlink the account planted
        /// in its own home, and reads no more than the file it expects.
        #[test]
        fn the_agent_probe_follows_no_symlink_and_reads_a_bounded_size() {
            use crate::setup::fsx::tests::Scratch;
            use std::os::unix::fs::symlink;
            let want = b"<plist/>";
            let scratch = Scratch::new("session-agent");
            let home = || scratch.dir();
            assert_eq!(probe::agent_state_in(&home(), want), FileState::Missing);
            std::fs::create_dir_all(scratch.path("Library/LaunchAgents")).unwrap();
            let agent = scratch.path(&format!("Library/LaunchAgents/{}", agent_file()));
            assert_eq!(probe::agent_state_in(&home(), want), FileState::Missing);
            std::fs::write(&agent, want).unwrap();
            assert_eq!(probe::agent_state_in(&home(), want), FileState::Same);
            // A symlink to the very same bytes is not the file.
            std::fs::write(scratch.path("elsewhere"), want).unwrap();
            std::fs::remove_file(&agent).unwrap();
            symlink(scratch.path("elsewhere"), &agent).unwrap();
            assert_eq!(probe::agent_state_in(&home(), want), FileState::Differs);
            // Nor a symlink to an endless device.
            std::fs::remove_file(&agent).unwrap();
            symlink("/dev/zero", &agent).unwrap();
            assert_eq!(probe::agent_state_in(&home(), want), FileState::Differs);
            // A file larger than expected is not read.
            std::fs::remove_file(&agent).unwrap();
            std::fs::write(&agent, [b'x'; 4096]).unwrap();
            assert_eq!(probe::agent_state_in(&home(), want), FileState::Differs);
            // A symlinked directory on the way is not followed either.
            std::fs::remove_dir_all(scratch.path("Library")).unwrap();
            std::fs::create_dir_all(scratch.path("real/LaunchAgents")).unwrap();
            std::fs::write(
                scratch.path(&format!("real/LaunchAgents/{}", agent_file())),
                want,
            )
            .unwrap();
            symlink(scratch.path("real"), scratch.path("Library")).unwrap();
            assert_eq!(probe::agent_state_in(&home(), want), FileState::Differs);
        }

        /// A `dscl` failure other than "no such record" stops setup and
        /// uninstall: read as "no account", setup would make it again with
        /// another uid.
        #[test]
        fn a_failed_account_read_is_an_error_not_no_account() {
            let failed = Output {
                success: false,
                code: Some(1),
                stderr: "DS Error: -14987 (eDSPermissionError)".into(),
                ..Output::default()
            };
            let s = crate::setup::plan::tests::set_up_mac();
            let mut runner = FakeRunner {
                answers: vec![(read_user(), failed.clone())],
                ..FakeRunner::default()
            };
            let err = probe::gather(&s, Some(451), &mut runner).unwrap_err();
            assert!(format!("{err:#}").contains("eDSPermissionError"), "{err:#}");
            let mut runner = FakeRunner {
                answers: vec![(read_user(), failed)],
                ..FakeRunner::default()
            };
            assert!(probe::gather_uninstall(&s.layout, &mut runner).is_err());
            // Not found is no account.
            let mut runner = FakeRunner {
                answers: vec![(
                    read_user(),
                    Output {
                        success: false,
                        code: Some(56),
                        stderr: "DS Error: -14136 (eDSRecordNotFound)".into(),
                        ..Output::default()
                    },
                )],
                ..FakeRunner::default()
            };
            assert_eq!(
                probe::gather(&s, Some(451), &mut runner).unwrap().user,
                None
            );
        }

        /// A record without a GeneratedUID is `None`, so uninstall takes its
        /// `leave` path; any other dscl failure is an error.
        #[test]
        fn a_missing_generated_uid_is_none_and_a_failed_read_an_error() {
            let read = Cmd::new(
                Tool::Dscl,
                [
                    ".".to_string(),
                    "-read".into(),
                    node(),
                    "GeneratedUID".into(),
                ],
            )
            .display();
            let failed = |stderr: &str| Output {
                success: false,
                code: Some(181),
                stderr: stderr.into(),
                ..Output::default()
            };
            let mut runner = FakeRunner {
                answers: vec![(read.clone(), failed("No such key: GeneratedUID"))],
                ..FakeRunner::default()
            };
            assert_eq!(probe::generated_uid(&mut runner).unwrap(), None);
            let mut runner = FakeRunner {
                answers: vec![(
                    read.clone(),
                    failed("DS Error: -14987 (eDSPermissionError)"),
                )],
                ..FakeRunner::default()
            };
            assert!(probe::generated_uid(&mut runner).is_err());
            let mut runner = FakeRunner {
                answers: vec![(read, ok(&format!("GeneratedUID: {UUID}\n")))],
                ..FakeRunner::default()
            };
            assert_eq!(
                probe::generated_uid(&mut runner).unwrap(),
                Some(UUID.to_string())
            );
        }

        #[test]
        fn the_token_change_is_checked_by_its_status_then_preboot_refreshed() {
            let status = "/usr/sbin/sysadminctl -secureTokenStatus _faber_mac";
            let mut runner = FakeRunner {
                answers: vec![(
                    status.into(),
                    Output {
                        success: true,
                        code: Some(0),
                        stderr: "Secure token is ENABLED for user faber macOS VMs".into(),
                        ..Output::default()
                    },
                )],
                ..FakeRunner::default()
            };
            set_token(&mut runner, true, "owner").unwrap();
            let ran: Vec<String> = runner.ran.iter().map(Cmd::display).collect();
            assert_eq!(
                ran,
                vec![
                    token_cmd(true, "owner").display(),
                    status.to_string(),
                    update_preboot().display(),
                ]
            );
            // sysadminctl exits 0 when it changed nothing: refused, and the
            // Preboot volume is left alone.
            let mut runner = FakeRunner::default();
            assert!(set_token(&mut runner, true, "owner").is_err());
            assert!(!runner.ran.contains(&update_preboot()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::plan::tests::{fresh_mac, set_up_mac};

    fn on_mac(fv: Option<bool>) -> SessionProbe {
        SessionProbe {
            filevault: fv,
            ..SessionProbe::default()
        }
    }

    fn asked(mode: Option<RebootMode>) -> SessionOpts {
        SessionOpts {
            requested: true,
            mode,
            ..SessionOpts::default()
        }
    }

    fn privileged() -> HostState {
        let mut s = set_up_mac();
        s.privileged = true;
        s
    }

    /// The session actions but the record's, which [`record_steps`] covers.
    fn actions(steps: &[Step]) -> Vec<SessionAction> {
        steps
            .iter()
            .filter_map(|st| match &st.action {
                Some(Action::Session(SessionAction::Record(_))) => None,
                Some(Action::Session(a)) => Some(a.clone()),
                _ => None,
            })
            .collect()
    }

    /// What the plan wants the record to say, if it changes it.
    fn record_change(steps: &[Step]) -> Option<Option<SessionAccount>> {
        plan::session_record(steps)
    }

    fn refused(steps: &[Step]) -> Vec<&str> {
        steps
            .iter()
            .filter(|st| st.status == Status::Refused)
            .map(|st| st.detail.as_str())
            .collect()
    }

    /// Changes but the record's.
    fn changes(steps: &[Step]) -> usize {
        steps
            .iter()
            .filter(|st| matches!(st.status, Status::WillCreate | Status::WillChange))
            .filter(|st| !matches!(st.action, Some(Action::Session(SessionAction::Record(_)))))
            .count()
    }

    /// The account as setup leaves it in `mode`.
    fn made(mode: RebootMode, uid: u32) -> SessionProbe {
        let hidden = mode == RebootMode::AutoLogin;
        SessionProbe {
            user: Some(UserInfo {
                name: FABER_MAC_ACCOUNT.into(),
                uid,
                gid: STAFF_GID,
                home: FABER_MAC_HOME.into(),
                shell: SHELL.into(),
                hidden,
                password: Some("********".into()),
                auth_authority: true,
            }),
            filevault: Some(mode == RebootMode::Unlock),
            secure_token: Some(mode == RebootMode::Unlock),
            autologin_user: (mode == RebootMode::AutoLogin).then(|| FABER_MAC_ACCOUNT.into()),
            autologin_locked: mode == RebootMode::AutoLogin,
            kcpassword: mode == RebootMode::AutoLogin,
            show_other: None,
            show_other_saved: false,
            home: HomeState::Ok,
            dirs: dirs_as_made(&set_up_mac(), uid),
            agent: FileState::Same,
            ssh_deny: FileState::Same,
        }
    }

    /// The supervisor's directories as setup leaves them on `s`.
    fn dirs_as_made(s: &HostState, uid: u32) -> Vec<(SessionDir, DirProbe)> {
        let faber_gid = plan::planned_ids(s).unwrap().0.gid;
        SessionDir::ALL
            .iter()
            .map(|d| {
                (
                    *d,
                    DirProbe::Dir {
                        uid,
                        gid: d.gid(faber_gid),
                        mode: d.mode(),
                    },
                )
            })
            .collect()
    }

    #[test]
    fn without_the_flag_or_the_account_nothing_is_planned() {
        let steps = plan_steps(&fresh_mac(), &on_mac(Some(true)), &SessionOpts::default());
        assert!(steps.is_empty(), "{steps:?}");
    }

    #[test]
    fn linux_refuses_the_flag() {
        let mut s = fresh_mac();
        s.os = Os::Linux;
        let steps = plan_steps(&s, &on_mac(None), &asked(None));
        assert_eq!(refused(&steps).len(), 1);
        assert!(refused(&steps)[0].contains("macOS only"));
    }

    #[test]
    fn the_mode_is_pre_selected_from_filevault() {
        assert_eq!(preselected(Some(true)), Some(RebootMode::Unlock));
        assert_eq!(preselected(Some(false)), Some(RebootMode::Manual));
        assert_eq!(preselected(None), None);
        let s = privileged();
        let steps = plan_steps(&s, &on_mac(None), &asked(None));
        assert!(refused(&steps)[0].contains("cannot tell whether FileVault is on"));
    }

    #[test]
    fn unlock_creates_a_visible_account_with_a_password_and_a_secure_token() {
        let s = privileged();
        let steps = plan_steps(&s, &on_mac(Some(true)), &asked(None));
        assert!(refused(&steps).is_empty(), "{:?}", refused(&steps));
        let a = actions(&steps);
        let uid = *FABER_MAC_ID_RANGE.start();
        let cmds: Vec<String> = a
            .iter()
            .filter_map(|a| match a {
                SessionAction::Account(c) => Some(c.display()),
                _ => None,
            })
            .collect();
        assert_eq!(
            cmds,
            create_cmds(uid, false)
                .iter()
                .map(Cmd::display)
                .collect::<Vec<_>>()
        );
        assert!(cmds.iter().all(|c| !c.contains("IsHidden")), "visible");
        assert!(cmds.iter().any(|c| c.ends_with("UserShell /sbin/nologin")));
        assert!(cmds.iter().all(|c| !c.contains("/usr/bin/false")));
        assert!(cmds.iter().all(|c| !c.contains("admin")));
        assert!(a.contains(&SessionAction::SetPassword(PasswordSource::Typed)));
        assert!(a.contains(&SessionAction::TokenOn));
        assert!(a.contains(&SessionAction::EnsureHome { uid }));
        assert!(a.contains(&SessionAction::InstallAgent { uid }));
        assert!(!a.contains(&SessionAction::AutoLoginOn));
        assert!(!a
            .iter()
            .any(|a| matches!(a, SessionAction::ShowOtherOn { .. })));
        let mode = steps.iter().find(|st| st.subject == "reboot mode").unwrap();
        assert!(mode
            .detail
            .starts_with("unlock (pre-selected: FileVault is on)"));
    }

    /// The record is locked (`Password *`) before any other attribute, so a
    /// run stopped before the password is set leaves no passwordless account;
    /// setting the password comes after every account command.
    #[test]
    fn a_new_account_is_locked_until_its_password_is_set() {
        let cmds = create_cmds(600, false);
        assert_eq!(cmds[0].args, [".", "-create", node().as_str()]);
        // The literal `*`, never through a shell.
        assert_eq!(
            cmds[1].args,
            [".", "-create", node().as_str(), "Password", "*"]
        );
        let s = privileged();
        let steps = plan_steps(&s, &on_mac(Some(false)), &asked(None));
        let a = actions(&steps);
        let last_account = a
            .iter()
            .rposition(|a| matches!(a, SessionAction::Account(_)))
            .unwrap();
        let password = a
            .iter()
            .position(|a| matches!(a, SessionAction::SetPassword(_)))
            .unwrap();
        assert!(last_account < password);
    }

    /// The account is kept off SSH by a drop-in, planned with the account and
    /// left alone once it is as written.
    #[test]
    fn the_account_is_denied_ssh() {
        let s = privileged();
        let steps = plan_steps(&s, &on_mac(Some(true)), &asked(None));
        assert!(actions(&steps).contains(&SessionAction::SshDeny));
        let st = steps.iter().find(|st| st.subject == SSH_DENY).unwrap();
        assert_eq!(st.status, Status::WillCreate);
        assert!(st.detail.contains("DenyUsers _faber_mac"), "{}", st.detail);
        assert!(st.detail.contains("ssh -N -L"), "{}", st.detail);
        let mut p = made(RebootMode::Manual, 600);
        p.ssh_deny = FileState::Differs;
        let steps = plan_steps(&s, &p, &SessionOpts::default());
        assert_eq!(actions(&steps), vec![SessionAction::SshDeny]);
        assert!(ssh_deny_conf().ends_with("\nDenyUsers _faber_mac\n"));
        assert_eq!(
            ssh_deny_real(),
            Path::new("/private/etc/ssh/sshd_config.d/faber-mac.conf")
        );
    }

    #[test]
    fn the_generated_uid_is_read_only_when_it_is_a_uuid() {
        let uuid = "0A1B2C3D-4E5F-6789-ABCD-EF0123456789";
        assert_eq!(
            parse_generated_uid(&format!("GeneratedUID: {}\n", uuid.to_lowercase())),
            Some(uuid.to_string())
        );
        assert_eq!(parse_generated_uid("GeneratedUID: $(touch x)\n"), None);
        assert_eq!(parse_generated_uid("No such key: GeneratedUID\n"), None);
        let listing = format!(
            "Cryptographic users for disk3s5 (2 found)\n|\n+-- {}\n|   Type: Local Open \
             Directory User\n",
            uuid.to_lowercase()
        );
        assert!(lists_crypto_user(&listing, uuid));
        assert!(!lists_crypto_user(
            &listing,
            "11111111-2222-3333-4444-555555555555"
        ));
    }

    #[test]
    fn manual_with_filevault_off_has_no_token_and_no_auto_login() {
        let s = privileged();
        let steps = plan_steps(&s, &on_mac(Some(false)), &asked(None));
        let a = actions(&steps);
        assert!(a.contains(&SessionAction::SetPassword(PasswordSource::Typed)));
        assert!(!a.contains(&SessionAction::TokenOn));
        assert!(!a.contains(&SessionAction::AutoLoginOn));
        assert!(
            steps
                .iter()
                .any(|st| st.subject == "reboot mode"
                    && st.detail.starts_with("manual (pre-selected"))
        );
    }

    #[test]
    fn auto_login_needs_filevault_off_and_a_generated_password() {
        let s = privileged();
        let on = plan_steps(&s, &on_mac(Some(true)), &asked(Some(RebootMode::AutoLogin)));
        assert!(refused(&on)[0].contains("needs FileVault off"));
        let typed = plan_steps(
            &s,
            &on_mac(Some(false)),
            &SessionOpts {
                password: Some(PasswordSource::Typed),
                ..asked(Some(RebootMode::AutoLogin))
            },
        );
        assert!(refused(&typed)[0].contains("--macos-session-password generate"));

        let steps = plan_steps(
            &s,
            &on_mac(Some(false)),
            &asked(Some(RebootMode::AutoLogin)),
        );
        assert!(refused(&steps).is_empty(), "{:?}", refused(&steps));
        let a = actions(&steps);
        assert!(a.contains(&SessionAction::SetPassword(PasswordSource::Generate)));
        assert!(a.contains(&SessionAction::AutoLoginOn));
        assert!(!a.contains(&SessionAction::TokenOn));
        // Hidden, and logged in by macOS, so no "Other…".
        assert!(a
            .iter()
            .any(|a| matches!(a, SessionAction::Account(c) if c.display().contains("IsHidden 1"))));
        assert!(!a
            .iter()
            .any(|a| matches!(a, SessionAction::ShowOtherOn { .. })));
    }

    /// Another account's auto-login is never replaced: auto-login mode is
    /// refused, naming it, and the other modes leave it alone.
    #[test]
    fn another_accounts_auto_login_is_never_replaced() {
        let s = privileged();
        for kcpassword in [false, true] {
            let mut p = on_mac(Some(false));
            p.autologin_user = Some("someone".into());
            p.kcpassword = kcpassword;
            let steps = plan_steps(&s, &p, &asked(Some(RebootMode::AutoLogin)));
            let why = refused(&steps);
            assert_eq!(why.len(), 1, "{steps:#?}");
            assert!(
                why[0].contains("logs someone in automatically"),
                "{}",
                why[0]
            );
            assert!(why[0].contains("turn someone's auto-login off first"));
            assert!(why[0].contains("--macos-session manual"));
            assert_eq!(why[0].contains(KCPASSWORD), kcpassword);
            assert!(!actions(&steps).contains(&SessionAction::AutoLoginOn));
            // Manual and unlock neither refuse nor touch it.
            let steps = plan_steps(&s, &p, &asked(Some(RebootMode::Manual)));
            assert!(refused(&steps).is_empty(), "{:?}", refused(&steps));
            let a = actions(&steps);
            assert!(!a.contains(&SessionAction::AutoLoginOn));
            assert!(!a.contains(&SessionAction::AutoLoginOff));
        }
        // An existing account moving to auto-login is refused the same way.
        let mut p = made(RebootMode::Manual, 600);
        p.autologin_user = Some("someone".into());
        let steps = plan_steps(&s, &p, &asked(Some(RebootMode::AutoLogin)));
        assert!(refused(&steps)[0].contains("logs someone in automatically"));
        assert_eq!(changes(&steps), 0);
    }

    #[test]
    fn a_hidden_account_logged_in_by_hand_gets_other_and_keeps_the_previous_value() {
        let s = privileged();
        let steps = plan_steps(
            &s,
            &on_mac(Some(true)),
            &SessionOpts {
                visibility: Some(Visibility::Hidden),
                ..asked(None)
            },
        );
        assert!(actions(&steps).contains(&SessionAction::ShowOtherOn {
            prev: Some("absent".into())
        }));
        // Back to visible: the kept value is put back.
        let mut p = made(RebootMode::Unlock, 600);
        p.user.as_mut().unwrap().hidden = true;
        p.show_other = Some(true);
        p.show_other_saved = true;
        let steps = plan_steps(
            &s,
            &p,
            &SessionOpts {
                visibility: Some(Visibility::Visible),
                ..asked(None)
            },
        );
        let a = actions(&steps);
        assert!(a.contains(&SessionAction::ShowOtherRestore));
        assert!(a.iter().any(|a| matches!(a, SessionAction::Account(c) if c.display().contains("-delete") && c.display().contains("IsHidden"))));
    }

    #[test]
    fn a_re_run_in_each_mode_changes_nothing() {
        let s = privileged();
        for mode in [
            RebootMode::Unlock,
            RebootMode::AutoLogin,
            RebootMode::Manual,
        ] {
            let p = made(mode, 600);
            for o in [SessionOpts::default(), asked(None), asked(Some(mode))] {
                let steps = plan_steps(&s, &p, &o);
                assert_eq!(changes(&steps), 0, "{mode:?} {o:?}: {steps:#?}");
                assert!(
                    refused(&steps).is_empty(),
                    "{mode:?}: {:?}",
                    refused(&steps)
                );
            }
        }
    }

    #[test]
    fn changing_the_mode_changes_only_what_differs() {
        let s = privileged();
        // unlock -> manual: the token goes, nothing else.
        let steps = plan_steps(
            &s,
            &made(RebootMode::Unlock, 600),
            &asked(Some(RebootMode::Manual)),
        );
        assert_eq!(actions(&steps), vec![SessionAction::TokenOff]);
        // manual -> unlock (FileVault on): the token comes.
        let mut p = made(RebootMode::Manual, 600);
        p.filevault = Some(true);
        let steps = plan_steps(&s, &p, &asked(Some(RebootMode::Unlock)));
        assert_eq!(actions(&steps), vec![SessionAction::TokenOn]);
        // manual -> auto-login: hidden, a new generated password, auto-login on.
        let steps = plan_steps(
            &s,
            &made(RebootMode::Manual, 600),
            &asked(Some(RebootMode::AutoLogin)),
        );
        let a = actions(&steps);
        assert!(a.contains(&SessionAction::SetPassword(PasswordSource::Generate)));
        assert!(a.contains(&SessionAction::AutoLoginOn));
        assert!(a
            .iter()
            .any(|a| matches!(a, SessionAction::Account(c) if c.display().contains("IsHidden 1"))));
        // auto-login -> manual: auto-login off, visible again.
        let steps = plan_steps(
            &s,
            &made(RebootMode::AutoLogin, 600),
            &asked(Some(RebootMode::Manual)),
        );
        let a = actions(&steps);
        assert!(a.contains(&SessionAction::AutoLoginOff));
        assert!(!a.iter().any(|a| matches!(a, SessionAction::SetPassword(_))));
        assert!(a
            .iter()
            .any(|a| matches!(a, SessionAction::Account(c) if c.display().contains("-delete"))));
    }

    #[test]
    fn an_account_that_is_not_setups_is_refused() {
        let s = privileged();
        let mut p = made(RebootMode::Manual, 599);
        let steps = plan_steps(&s, &p, &asked(None));
        assert!(refused(&steps)[0].contains("outside 600–699"));
        p = made(RebootMode::Manual, 600);
        let mut admin = s.clone();
        admin.admins.push(FABER_MAC_ACCOUNT.into());
        let steps = plan_steps(&admin, &p, &asked(None));
        assert!(refused(&steps)[0].contains("is an admin"));
        p.home = HomeState::Foreign("is owned by uid 0, not the account".into());
        let steps = plan_steps(&s, &p, &asked(None));
        assert!(refused(&steps)[0].contains("Move it aside"));
    }

    #[test]
    fn a_wrong_shell_is_fixed_and_a_token_holder_keeps_its_password() {
        let s = privileged();
        let mut p = made(RebootMode::Unlock, 600);
        p.user.as_mut().unwrap().shell = "/usr/bin/false".into();
        let steps = plan_steps(&s, &p, &SessionOpts::default());
        let a = actions(&steps);
        assert_eq!(a.len(), 1);
        assert!(
            matches!(&a[0], SessionAction::Account(c) if c.display().ends_with("UserShell /sbin/nologin"))
        );
        let steps = plan_steps(
            &s,
            &made(RebootMode::Unlock, 600),
            &SessionOpts {
                password: Some(PasswordSource::Typed),
                ..asked(None)
            },
        );
        assert!(refused(&steps)[0].contains("SecureToken"));
    }

    #[test]
    fn a_token_change_needs_an_admin_operator() {
        let mut s = privileged();
        s.admins.clear();
        let steps = plan_steps(&s, &on_mac(Some(true)), &asked(None));
        assert!(refused(&steps)[0].contains("needs an admin's password"));
    }

    #[test]
    fn the_uid_is_the_first_free_one_in_the_block() {
        let mut used = accounts::UsedIds::default();
        assert_eq!(pick_uid(&used), Some(600));
        used.uids.extend([600, 601]);
        assert_eq!(pick_uid(&used), Some(602));
        used.uids.extend(FABER_MAC_ID_RANGE);
        assert_eq!(pick_uid(&used), None);
    }

    #[test]
    fn the_preview_without_sudo_plans_and_reads_what_it_can() {
        let mut s = fresh_mac();
        s.privileged = false;
        let steps = plan_steps(&s, &on_mac(Some(true)), &asked(None));
        assert!(refused(&steps).is_empty(), "{:?}", refused(&steps));
        assert!(changes(&steps) > 0);
        let mut p = made(RebootMode::Manual, 600);
        p.agent = FileState::Unknown;
        p.dirs = dirs_as_made(&s, 600);
        let steps = plan_steps(&s, &p, &SessionOpts::default());
        assert_eq!(changes(&steps), 0);
        assert!(steps
            .iter()
            .any(|st| st.detail.contains("readable with sudo")));
    }

    #[test]
    fn no_password_is_ever_in_a_planned_command() {
        let s = privileged();
        let steps = plan_steps(
            &s,
            &on_mac(Some(false)),
            &asked(Some(RebootMode::AutoLogin)),
        );
        for st in &steps {
            if let Some(Action::Session(SessionAction::Account(c))) = &st.action {
                assert!(!c.args.iter().any(|a| a == "-passwd" || a == "passwd"));
            }
        }
        // Both passwords are read by sysadminctl from the terminal.
        let t = token_cmd(true, "owner").args;
        assert_eq!(t[t.iter().position(|a| a == "-password").unwrap() + 1], "-");
        assert_eq!(
            t[t.iter().position(|a| a == "-adminPassword").unwrap() + 1],
            "-"
        );
    }

    #[test]
    fn kcpassword_is_apples_padded_xor() {
        // The reference: the password, a NUL, NULs to a multiple of 12, XOR
        // the 11-byte key cycled.
        let key = [
            0x7D, 0x89, 0x52, 0x23, 0xD2, 0xBC, 0xDD, 0xEA, 0xA3, 0xB9, 0x1F,
        ];
        let enc = kcpassword("password");
        assert_eq!(enc.len(), 12);
        let dec: Vec<u8> = enc
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ key[i % 11])
            .collect();
        assert_eq!(&dec[..8], b"password");
        assert!(dec[8..].iter().all(|b| *b == 0));
        assert_eq!(enc[0], b'p' ^ 0x7D);
        // A 12-byte password still gets a NUL: 24 bytes.
        assert_eq!(kcpassword("abcdefghijkl").len(), 24);
        // A generated one: 32 hex characters, 36 bytes.
        assert_eq!(kcpassword(&"a".repeat(32)).len(), 36);
    }

    #[test]
    fn the_agent_is_the_session_supervisor_and_enabled() {
        let layout = SystemLayout::for_os(Os::MacOs);
        let plist = agent_plist(&layout, 451);
        assert!(plist.contains("<key>LimitLoadToSessionType</key>\n    <string>Aqua</string>"));
        assert!(
            !plist.contains("<key>Disabled</key>"),
            "enabled: the daemon runs macOS VM jobs through it"
        );
        assert!(plist.contains(&format!(
            "<string>--socket</string>\n        <string>{}</string>",
            crate::platform::session_supervisor_socket().display()
        )));
        assert!(plist.contains("<key>RunAtLoad</key>\n    <true/>"));
        assert!(plist.contains(&format!(
            "<string>{}</string>",
            layout.launcher_bin.display()
        )));
        assert!(plist.contains("<string>supervise</string>"));
        assert!(plist.contains(&format!("<string>{FABER_MAC_HOME}/state</string>")));
        assert!(
            !plist.contains("--linux-dir"),
            "the Linux VM stays _faber_vm's"
        );
        assert!(
            !plist.contains("UserName"),
            "a LaunchAgent runs as its session's user"
        );
        assert_eq!(
            agent_path(),
            Path::new("/var/db/faber-mac/Library/LaunchAgents")
                .join(format!("{}.plist", service::SESSION_AGENT_LABEL))
        );
    }

    /// `s` with a record that already names the account as setup made it.
    fn recorded(mut s: HostState, mode: RebootMode, uid: u32) -> HostState {
        if let RecordProbe::Trusted(r) = &mut s.record {
            r.accounts.faber_mac = Some(SessionAccount {
                uid,
                gid: STAFF_GID,
                reboot_mode: Some(mode.name().to_string()),
            });
        }
        s
    }

    #[test]
    fn the_home_and_the_supervisors_directories_mirror_faber_vms() {
        let s = privileged();
        let faber_gid = plan::planned_ids(&s).unwrap().0.gid;
        // A new account: its home, then run/, state/ and clones/.
        let steps = plan_steps(&s, &on_mac(Some(true)), &asked(None));
        let a = actions(&steps);
        assert!(a.contains(&SessionAction::EnsureHome { uid: 600 }));
        for dir in SessionDir::ALL {
            assert!(
                a.contains(&SessionAction::EnsureDir { uid: 600, dir }),
                "{dir:?}"
            );
        }
        let shown = |subject: &str| {
            steps
                .iter()
                .find(|st| st.subject == subject)
                .map(|st| st.detail.clone())
                .unwrap_or_else(|| panic!("no step for {subject}"))
        };
        assert_eq!(
            shown(&format!("home {FABER_MAC_HOME}")),
            "_faber_mac:_faber 710"
        );
        assert_eq!(shown("/var/db/faber-mac/run"), "_faber_mac:_faber 710");
        assert_eq!(shown("/var/db/faber-mac/state"), "_faber_mac:staff 700");
        assert_eq!(shown("/var/db/faber-mac/clones"), "_faber_mac:staff 700");
        assert_eq!(SessionDir::Run.gid(faber_gid), faber_gid);
        assert_eq!(SessionDir::State.gid(faber_gid), STAFF_GID);

        // A home from the previous slice (the account's own 0700) is regrouped,
        // and a missing run/ is made.
        let s = recorded(s, RebootMode::Unlock, 600);
        let mut p = made(RebootMode::Unlock, 600);
        p.home = HomeState::Differs {
            gid: STAFF_GID,
            mode: 0o700,
        };
        p.dirs.retain(|(d, _)| *d != SessionDir::Run);
        let steps = plan_steps(&s, &p, &SessionOpts::default());
        assert_eq!(
            actions(&steps),
            vec![
                SessionAction::EnsureHome { uid: 600 },
                SessionAction::EnsureDir {
                    uid: 600,
                    dir: SessionDir::Run
                },
            ]
        );
        // run/ in the wrong group is fixed; one another account owns is refused.
        let mut p = made(RebootMode::Unlock, 600);
        p.dirs[0].1 = DirProbe::Dir {
            uid: 600,
            gid: STAFF_GID,
            mode: 0o700,
        };
        let steps = plan_steps(&s, &p, &SessionOpts::default());
        assert_eq!(
            actions(&steps),
            vec![SessionAction::EnsureDir {
                uid: 600,
                dir: SessionDir::Run
            }]
        );
        p.dirs[0].1 = DirProbe::Dir {
            uid: 0,
            gid: 0,
            mode: 0o710,
        };
        let steps = plan_steps(&s, &p, &SessionOpts::default());
        assert!(refused(&steps)[0].contains("owned by uid 0, not _faber_mac"));
        p.dirs[0].1 = DirProbe::NotADir("a symlink".into());
        let steps = plan_steps(&s, &p, &SessionOpts::default());
        assert!(refused(&steps)[0].contains("a symlink where a directory belongs"));
    }

    #[test]
    fn the_record_names_the_account_and_its_mode_once() {
        let s = privileged();
        // A new account: named, with its reboot mode.
        let steps = plan_steps(&s, &on_mac(Some(false)), &asked(None));
        assert_eq!(
            record_change(&steps),
            Some(Some(SessionAccount {
                uid: 600,
                gid: STAFF_GID,
                reboot_mode: Some("manual".into()),
            }))
        );
        // Already named: nothing.
        for mode in [
            RebootMode::Unlock,
            RebootMode::AutoLogin,
            RebootMode::Manual,
        ] {
            let steps = plan_steps(
                &recorded(s.clone(), mode, 600),
                &made(mode, 600),
                &SessionOpts::default(),
            );
            assert_eq!(record_change(&steps), None, "{mode:?}");
        }
        // A new mode is recorded.
        let steps = plan_steps(
            &recorded(s.clone(), RebootMode::Unlock, 600),
            &made(RebootMode::Unlock, 600),
            &asked(Some(RebootMode::Manual)),
        );
        assert_eq!(
            record_change(&steps).flatten().and_then(|a| a.reboot_mode),
            Some("manual".into())
        );
        // The account is gone: the record stops naming it.
        let steps = plan_steps(
            &recorded(s.clone(), RebootMode::Manual, 600),
            &on_mac(Some(false)),
            &SessionOpts::default(),
        );
        assert_eq!(record_change(&steps), Some(None));
        // No account and none recorded: no step at all.
        assert!(plan_steps(&s, &on_mac(Some(false)), &SessionOpts::default()).is_empty());
    }

    /// The record change lands in the main plan: written before the daemon
    /// starts, and a running daemon restarted, since it reads the record once.
    #[test]
    fn a_record_change_rewrites_the_record_and_restarts_the_daemon() {
        let s = privileged();
        let mut steps = plan::plan(&s);
        let ok = |steps: &[Step], section: Section| {
            steps
                .iter()
                .find(|st| st.section == section)
                .map(|st| st.status)
        };
        assert_eq!(ok(&steps, Section::Record), Some(Status::Ok));
        assert_eq!(ok(&steps, Section::Service), Some(Status::Ok));
        // A re-run that changes nothing about the account moves nothing.
        let mut same = steps.clone();
        same.extend(plan_steps(
            &recorded(s.clone(), RebootMode::Manual, 600),
            &made(RebootMode::Manual, 600),
            &SessionOpts::default(),
        ));
        plan::fold_session_record(&mut same);
        assert_eq!(ok(&same, Section::Record), Some(Status::Ok));
        assert_eq!(ok(&same, Section::Service), Some(Status::Ok));

        steps.extend(plan_steps(
            &s,
            &made(RebootMode::Manual, 600),
            &SessionOpts::default(),
        ));
        plan::fold_session_record(&mut steps);
        let record = steps
            .iter()
            .find(|st| st.section == Section::Record)
            .unwrap();
        assert_eq!(record.action, Some(Action::WriteRecord));
        let service = steps
            .iter()
            .find(|st| st.section == Section::Service)
            .unwrap();
        assert_eq!(
            service.action,
            Some(Action::InstallService { restart: true })
        );
        assert!(service.detail.contains("drained"), "{}", service.detail);
    }

    /// A new launcher, network plan or agent file restarts the session's
    /// supervisor onto it, as `_faber_vm`'s is; nothing else does, and a plan
    /// with a refusal changes nothing.
    #[test]
    fn the_session_supervisor_restarts_onto_what_this_run_installs() {
        use crate::setup::vm_runtime::VmAction;
        let p = made(RebootMode::Manual, 600);
        let main = |status: Status, action: Action| {
            vec![step(Section::VmRuntime, status, "x", "").doing(action)]
        };
        let restart = |steps: &[Step]| actions(&restart_steps(steps, &p));
        assert!(restart(&[]).is_empty());
        for action in [
            Action::Vm(VmAction::InstallLauncher),
            Action::Vm(VmAction::WritePlan),
            Action::Session(SessionAction::InstallAgent { uid: 600 }),
        ] {
            assert_eq!(
                restart(&main(Status::WillChange, action.clone())),
                vec![SessionAction::RestartAgent { uid: 600 }],
                "{action:?}"
            );
            // Already in place: nothing.
            assert!(restart(&main(Status::Ok, action)).is_empty());
        }
        assert!(restart(&main(
            Status::WillChange,
            Action::Vm(VmAction::EnsureToolsDir)
        ))
        .is_empty());
        let mut refused = main(Status::WillChange, Action::Vm(VmAction::InstallLauncher));
        refused.push(step(Section::VmRuntime, Status::Refused, "y", ""));
        assert!(restart(&refused).is_empty());
        // No account: nothing to restart.
        let launcher = main(Status::WillChange, Action::Vm(VmAction::InstallLauncher));
        assert!(restart_steps(&launcher, &on_mac(Some(false))).is_empty());
        let st = &restart_steps(&launcher, &p)[0];
        assert!(
            st.detail
                .contains("bootout + bootstrap gui/600/io.github.rejifald.faber.mac-vm-supervisor"),
            "{}",
            st.detail
        );
    }

    #[test]
    fn the_login_step_names_the_modes_action() {
        assert!(login_step(RebootMode::Unlock).contains("unlock the disk as"));
        assert!(login_step(RebootMode::Manual).starts_with("log faber macOS VMs in"));
        assert!(login_step(RebootMode::AutoLogin).contains("logs it in at boot"));
    }

    #[test]
    fn the_home_is_beside_faber_vms() {
        let layout = SystemLayout::for_os(Os::MacOs);
        assert_eq!(Path::new(FABER_MAC_HOME).parent(), layout.vm_home.parent());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn probes_read_the_tools_reports() {
        assert_eq!(probe::filevault("FileVault is On.\n"), Some(true));
        assert_eq!(probe::filevault("FileVault is Off.\n"), Some(false));
        assert_eq!(probe::filevault(""), None);
        assert_eq!(
            probe::token(
                "2026-10-25 sysadminctl[1:2] Secure token is ENABLED for user faber macOS VMs"
            ),
            Some(true)
        );
        assert_eq!(
            probe::token("Secure token is DISABLED for user faber macOS VMs"),
            Some(false)
        );
        assert_eq!(probe::token("no such user"), None);
    }
}
