//! Read-only probes that fill a [`HostState`]: the directory service, the
//! paths of the SYSTEM layout, the old per-user instances and their files,
//! the setup record, and the service manager.
//!
//! Nothing here writes. Without sudo the probes see less (another account's
//! home, `_faber`'s directories), and the plan says what it could not check.
//! This process's own layout is always LEGACY before the record exists, so
//! every path comes from [`crate::platform::system`] and
//! [`crate::platform::legacy`] directly, never from the cached accessors.

use std::ffi::OsStr;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;

use super::accounts;
use super::exe::{self, PathFacts};
use super::fsx::{self, Account, Dir, SourceRefusal};
use super::old::{self, Definition};
use super::plan::{
    self, dir_specs, file_specs, BinProbe, DirProbe, FileKind, FileProbe, HostState, LegacyAccount,
    LegacyDir, OldInstance, OperatorInfo, PlanOpts, RecordProbe, ServiceProbe, SrcProbe,
    DEFAULT_PORT,
};
use super::tools::{self, Cmd, Runner, Tool};
use crate::platform::layout::{Os, FABER_ACCOUNT, FABER_OPERATOR_GROUP, FABER_VM_ACCOUNT};
use crate::platform::record::{self, current_euid};
use crate::platform::service::{self, launchd, systemd, UNIT_NAME};
use crate::platform::{legacy, Env};

/// Everything the plan needs.
pub async fn gather(opts: PlanOpts, runner: &mut dyn Runner) -> Result<HostState> {
    let os = Os::HOST;
    let layout = crate::platform::system();
    let privileged = current_euid() == 0;
    let operator = operator(os, privileged, runner);

    let exe_path = std::env::current_exe()?;
    let exe_path = std::fs::canonicalize(&exe_path).unwrap_or(exe_path);
    let exe_sha256 = fsx::file_digest(&exe_path).ok().flatten().map(|d| d.sha256);
    let exe_chain = super::exe::chain_of(&exe_path).map_err(|e| e.to_string());

    let ids = accounts::probe_used_ids(os, runner)?;
    let faber_user = accounts::probe_user(os, runner, FABER_ACCOUNT)?;
    let faber_group = accounts::probe_group(os, runner, FABER_ACCOUNT)?;
    let vm_user = accounts::probe_user(os, runner, FABER_VM_ACCOUNT)?;
    let vm_group = accounts::probe_group(os, runner, FABER_VM_ACCOUNT)?;
    let operator_group = accounts::probe_group(os, runner, FABER_OPERATOR_GROUP)?;
    let faber_groups = if faber_user.is_some() {
        accounts::probe_groups_of(runner, FABER_ACCOUNT)
    } else {
        Vec::new()
    };
    let vm_groups = if vm_user.is_some() {
        accounts::probe_groups_of(runner, FABER_VM_ACCOUNT)
    } else {
        Vec::new()
    };
    let admins = match os {
        Os::MacOs => accounts::probe_group(os, runner, "admin")?
            .map(|g| g.members)
            .unwrap_or_default(),
        Os::Linux => Vec::new(),
    };
    let docker_group = match os {
        Os::MacOs => None,
        Os::Linux => accounts::probe_group(os, runner, "docker")?,
    };

    let dirs = dir_specs(os, &layout)
        .into_iter()
        .map(|spec| {
            let probe = probe_dir(&spec.path);
            (spec, probe)
        })
        .collect();
    let files = file_specs(os, &layout)
        .into_iter()
        .map(|spec| {
            let probe = probe_file(&spec.path);
            (spec, probe)
        })
        .collect();
    // Both copies are read, never run: the daemon copy is `_faber`'s file,
    // and its version comes from the marker in its bytes.
    let cli_bin = probe_bin(&layout.cli_bin);
    let daemon_bin = probe_bin(&layout.daemon_bin);

    let record = match record::load_trusted(&layout.setup_record) {
        Ok(None) => RecordProbe::Absent,
        Ok(Some(r)) => RecordProbe::Trusted(r),
        Err(e) => RecordProbe::Untrusted(e.to_string()),
    };
    let user_manager = os == Os::Linux && systemd::user_manager_supported(runner);
    let legacy = legacy_accounts(
        os,
        privileged,
        &operator,
        opts.migrate_from.as_deref(),
        user_manager,
        runner,
    )
    .await?;
    let dest_exists = FileKind::ALL
        .into_iter()
        .filter(|k| std::fs::symlink_metadata(k.dest(&layout)).is_ok())
        .collect();
    let service = ServiceProbe {
        installed: std::fs::read_to_string(&layout.service_unit).ok(),
        loaded: match os {
            Os::MacOs => launchd::is_loaded(runner, &service::system_target()),
            Os::Linux => systemd::is_enabled(runner) && systemd::is_active(runner),
        },
        // Setup always boots the daemon out and bootstraps it again.
        stuck: None,
    };
    let keep_awake = match os {
        Os::MacOs if opts.keep_awake => runner
            .run(&Cmd::new(Tool::Pmset, ["-g", "custom"]))
            .ok()
            .filter(|o| o.success)
            .map(|o| pmset_settled(&o.stdout)),
        _ => None,
    };

    let credential_files = crate::credential::token_files(&layout.credentials_dir);

    let cli_chain = layout
        .cli_bin
        .parent()
        .map(|dir| dir.ancestors().filter_map(path_facts).collect())
        .unwrap_or_default();

    let linux_image = super::linux_image::probe(os, &layout, faber_user.as_ref().map(|u| u.uid));
    #[cfg(target_os = "macos")]
    let vm_runtime = super::vm_runtime::probe::gather(
        &layout,
        &exe_path,
        &opts.vm,
        faber_user.as_ref().map(|u| u.uid),
        runner,
        probe_dir,
        probe_bin,
    );
    #[cfg(not(target_os = "macos"))]
    let vm_runtime = super::vm_runtime::VmRuntimeProbe::nothing();
    #[cfg(target_os = "macos")]
    let app = super::app::probe::gather(
        &exe_path,
        opts.vm.launcher.as_deref(),
        operator.as_ref().ok(),
        privileged,
    );
    #[cfg(not(target_os = "macos"))]
    let app = super::app::AppProbe::nothing();
    let mut state = HostState {
        os,
        layout,
        version: env!("FABER_VERSION").to_string(),
        privileged,
        operator,
        exe_path,
        exe_sha256,
        exe_chain,
        cli_chain,
        source_check: None,
        ids,
        faber_user,
        faber_group,
        vm_user,
        vm_group,
        operator_group,
        faber_groups,
        vm_groups,
        admins,
        docker_group,
        dirs,
        files,
        cli_bin,
        daemon_bin,
        legacy,
        dest_exists,
        record,
        service,
        keep_awake,
        credential_files,
        user_manager,
        linux_image,
        vm_runtime,
        app,
        opts,
    };
    // Last: which config is migrated, and whether this binary may be run,
    // are the plan's own answers.
    state.source_check = check_source_config(&state);
    Ok(state)
}

/// Owner and mode of `path` itself, a symlink included; `None` when nothing
/// is there.
fn path_facts(path: &Path) -> Option<PathFacts> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    Some(PathFacts {
        path: path.to_path_buf(),
        uid: meta.uid(),
        gid: meta.gid(),
        mode: meta.mode(),
    })
}

/// The most of a config file the source check reads: the cap `check-config
/// --config -` itself applies, so setup never sends what the checker would
/// refuse to read. A real faber config is a few KiB.
const CONFIG_CAP: usize = crate::config::FileConfig::READ_CAP;

/// Check the config setup is about to migrate with this binary's
/// `check-config`, before anything is drained or stopped: a config the new
/// daemon would refuse (on Linux, `dind` granted before jobs run behind a
/// wall) is then a refusal in the plan, not a rollback after an hour-long
/// drain. The copy in place is checked again as `_faber` after migration.
///
/// The bytes are read here, once, the way migration reads them (no symlink,
/// owned by their account, at most [`CONFIG_CAP`], a bigger file refused
/// before anything is spawned), and handed to the checker on its stdin:
/// `check-config --config -`, which reads the descriptor it inherits and
/// opens nothing by name. Nothing is staged on disk, so there is no temp name
/// to collide with or leave behind, and the operator need not be able to read
/// another account's home (`--migrate-from`) for the check to see exactly
/// what setup read. Reading stdin is safe for this check: a config names no
/// file relative to itself.
///
/// The child runs as the operator: this binary, which the plan has already
/// judged safe for root to run, is run with less. It gets a session of its
/// own and piped, bounded output ([`tools::run_as`]). Without sudo it runs as
/// this process's own account, on a config that account owns.
fn check_source_config(state: &HostState) -> Option<Result<(), String>> {
    let source = plan::migration_source(state).ok()??;
    let (_, path, probe) = source
        .files
        .iter()
        .find(|(k, _, _)| *k == FileKind::Config)?;
    if !matches!(probe, SrcProbe::Regular { .. }) {
        return None;
    }
    if state.dest_exists.contains(&FileKind::Config) && !state.opts.remigrate {
        return None;
    }
    let ids = plan::planned_ids(state).ok();
    let chain = state.exe_chain.as_ref().ok()?;
    exe::verdict(chain, &plan::untrusted(state, ids)).ok()?;
    let op = state.operator.as_ref().ok()?;
    let account = if state.privileged {
        Some(Account {
            uid: op.uid,
            gid: op.gid,
        })
    } else if source.uid == op.uid {
        None
    } else {
        return None;
    };
    Some(check_file(
        &state.exe_path,
        path,
        source.uid,
        account,
        &op.home,
    ))
}

/// Read the config at `path`, which `owner` must own, and check it with
/// `exe` run as `account`.
fn check_file(
    exe: &Path,
    path: &Path,
    owner: u32,
    account: Option<Account>,
    home: &Path,
) -> Result<(), String> {
    read_config(path, owner).and_then(|bytes| run_check(exe, path, &bytes, account, home))
}

/// The config at `path`, read without following a symlink, only if `owner`
/// owns it, and only if it is no larger than [`CONFIG_CAP`].
fn read_config(path: &Path, owner: u32) -> Result<Vec<u8>, String> {
    let (parent, name) = fsx::split(path).map_err(|e| e.to_string())?;
    let dir = Dir::open(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    let (file, _) =
        fsx::open_source(&dir, name, owner).map_err(|why| format!("{} {why}", path.display()))?;
    let mut bytes = Vec::new();
    file.take(CONFIG_CAP as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("reading {}: {e}", path.display()))?;
    if bytes.len() > CONFIG_CAP {
        return Err(format!(
            "{} is larger than 1 MiB, more than any faber config: setup does not check or \
             copy a file that size. Trim it, or pass --fresh to start from the starter config",
            path.display()
        ));
    }
    Ok(bytes)
}

/// `check-config --config -` as `account`, with `config` on its stdin, and
/// what it said if it failed, made safe to print and headed with `shown`,
/// the file the bytes came from: the checker only knows them as stdin.
fn run_check(
    exe: &Path,
    shown: &Path,
    config: &[u8],
    account: Option<Account>,
    home: &Path,
) -> Result<(), String> {
    let out = tools::run_as(
        exe,
        &[
            OsStr::new("check-config"),
            OsStr::new("--config"),
            OsStr::new("-"),
            // The child has no terminal, so `auto` would already mean no
            // colour; said anyway, since its output is shown sanitized and a
            // colour code would print as `\x1b` text (ADR-0050).
            OsStr::new("--color"),
            OsStr::new("never"),
        ],
        Some(config),
        account,
        home,
        Duration::from_secs(60),
    )
    .map_err(|e| format!("could not run check-config: {e:#}"))?;
    if out.success {
        Ok(())
    } else {
        Err(format!("{}:\n{}", shown.display(), out.sanitized(2000)))
    }
}

/// The person running setup. With sudo: `SUDO_UID` and `SUDO_USER`, never 0,
/// and both confirmed by the directory service, which also gives the home.
/// `$HOME` is never consulted: under sudo it is root's, or whatever the
/// environment says. Without sudo: this process's own account.
pub fn operator(os: Os, privileged: bool, runner: &mut dyn Runner) -> Result<OperatorInfo, String> {
    let (name, uid) = if privileged {
        let raw = std::env::var("SUDO_UID").map_err(|_| {
            "SUDO_UID is not set: run `sudo faber setup` from your own account, so setup knows \
             who the operator is"
                .to_string()
        })?;
        let uid: u32 = raw
            .trim()
            .parse()
            .map_err(|_| format!("SUDO_UID {raw:?} is not a uid"))?;
        if uid == 0 {
            return Err(
                "SUDO_UID is 0: run it with sudo from your own account, not from a root \
                        shell, so setup knows who the operator is"
                    .into(),
            );
        }
        let name = std::env::var("SUDO_USER").map_err(|_| {
            "SUDO_USER is not set: run `sudo faber setup` from your own account".to_string()
        })?;
        (name, uid)
    } else {
        let out = runner
            .run(&Cmd::new(Tool::Id, ["-un"]))
            .map_err(|e| e.to_string())?;
        if !out.success {
            return Err(format!("`id -un` failed: {}", out.text()));
        }
        (out.stdout.trim().to_string(), current_euid())
    };
    if !accounts::valid_name(&name) {
        return Err(format!("{name:?} is not an account name setup accepts"));
    }
    let user = accounts::probe_user(os, runner, &name)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("the directory service has no account named {name}"))?;
    if user.uid != uid {
        return Err(format!(
            "{name} has uid {} in the directory service, but this session says {uid}",
            user.uid
        ));
    }
    Ok(OperatorInfo {
        name,
        uid,
        gid: user.gid,
        home: user.home,
    })
}

pub(super) fn probe_dir(path: &Path) -> DirProbe {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => DirProbe::NotADir("a symlink".into()),
        Ok(m) if m.is_dir() => DirProbe::Dir {
            uid: m.uid(),
            gid: m.gid(),
            mode: m.mode(),
        },
        Ok(_) => DirProbe::NotADir("a file".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => DirProbe::Missing,
        Err(e) => DirProbe::Unknown(e.to_string()),
    }
}

pub(super) fn probe_bin(path: &Path) -> BinProbe {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BinProbe::Missing,
        Err(e) => BinProbe::Unknown(e.to_string()),
        Ok(m) if !m.file_type().is_file() => BinProbe::NotRegular,
        Ok(m) => match fsx::file_digest(path) {
            Ok(Some(digest)) => BinProbe::File {
                sha256: digest.sha256,
                uid: m.uid(),
                gid: m.gid(),
                mode: m.mode(),
                version: digest.version,
            },
            Ok(None) => BinProbe::Missing,
            Err(e) => BinProbe::Unknown(e.to_string()),
        },
    }
}

pub(super) fn probe_file(path: &Path) -> FileProbe {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() => FileProbe::NotRegular("a symlink".into()),
        Ok(m) if m.is_file() => FileProbe::File {
            uid: m.uid(),
            gid: m.gid(),
            mode: m.mode(),
        },
        Ok(_) => FileProbe::NotRegular("not a regular file".into()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => FileProbe::Missing,
        Err(e) => FileProbe::Unknown(e.to_string()),
    }
}

/// `pmset -g custom`: system sleep and Power Nap off in every power profile.
pub fn pmset_settled(text: &str) -> bool {
    let mut saw_sleep = false;
    for line in text.lines() {
        let mut words = line.split_whitespace();
        match (words.next(), words.next()) {
            (Some("sleep"), Some(v)) => {
                saw_sleep = true;
                if v != "0" {
                    return false;
                }
            }
            (Some("powernap"), Some(v)) if v != "0" => return false,
            _ => {}
        }
    }
    saw_sleep
}

/// Whether `home` is one a per-user faber could have run from.
fn per_user_home(os: Os, uid: u32, home: &Path) -> bool {
    match os {
        Os::MacOs => home.starts_with("/Users"),
        Os::Linux => (1000..65534).contains(&uid) || home.starts_with("/home"),
    }
}

/// Accounts with a per-user faber, plus the operator and the account
/// `--migrate-from` names, with their LEGACY state files.
async fn legacy_accounts(
    os: Os,
    privileged: bool,
    operator: &Result<OperatorInfo, String>,
    migrate_from: Option<&str>,
    user_manager: bool,
    runner: &mut dyn Runner,
) -> Result<Vec<LegacyAccount>> {
    let me = current_euid();
    let op_name = operator.as_ref().ok().map(|o| o.name.clone());
    let named = migrate_from.map(str::to_string);
    let mut out = Vec::new();
    for (name, uid, home) in accounts::probe_homes(os, runner)? {
        if name.starts_with('_') || !accounts::valid_name(&name) {
            continue;
        }
        let wanted = op_name.as_ref() == Some(&name) || named.as_ref() == Some(&name);
        if !wanted && !per_user_home(os, uid, &home) {
            continue;
        }
        let can_read = privileged || uid == me;
        let instance = old_instance(
            os,
            can_read,
            privileged,
            user_manager,
            &name,
            uid,
            &home,
            runner,
        )
        .await;
        if instance.is_none() && !wanted {
            continue;
        }
        let paths = legacy(&Env::with_home(&home));
        let config_override = instance.as_ref().and_then(|i| i.config_override.clone());
        let files: Vec<(FileKind, PathBuf, SrcProbe)> = FileKind::ALL
            .into_iter()
            .filter_map(|k| {
                let src = k.source(&paths, config_override.as_deref())?;
                let probe = probe_src(&src, uid);
                Some((k, src, probe))
            })
            .collect();
        let identity = identity_flags(&home, uid, &files);
        let dirs = plan::legacy_dirs(&paths)
            .into_iter()
            .filter_map(|path| {
                let meta = std::fs::symlink_metadata(&path).ok()?;
                let owner = meta.file_type().is_dir().then_some(meta.uid());
                Some(LegacyDir { path, owner })
            })
            .collect();
        out.push(LegacyAccount {
            name,
            uid,
            home,
            instance,
            files,
            identity,
            dirs,
        });
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
async fn old_instance(
    os: Os,
    can_read: bool,
    privileged: bool,
    user_manager: bool,
    name: &str,
    uid: u32,
    home: &Path,
    runner: &mut dyn Runner,
) -> Option<OldInstance> {
    let def = old::definition_path(os, home);
    let mut inst = OldInstance {
        definition: None,
        definition_problem: None,
        loaded: None,
        port: DEFAULT_PORT,
        config_override: None,
    };
    match old::read_definition(&def, uid) {
        Definition::Missing | Definition::Unknown(_) => {}
        Definition::Found(text) => {
            let args = match os {
                Os::MacOs => old::plist_program_arguments(&text),
                Os::Linux => old::unit_exec_start(&text),
            };
            let (port, config) = old::port_and_config(&args, home);
            inst.definition = Some(def);
            inst.port = port;
            inst.config_override = config;
        }
        Definition::Refused(why) => {
            inst.definition = Some(def);
            inst.definition_problem = Some(why);
        }
    }
    inst.loaded = match os {
        Os::MacOs if can_read => Some(launchd::is_loaded(runner, &service::gui_target(uid))),
        Os::Linux if privileged && user_manager => Some(
            runner
                .run(&systemd::user_cmd(
                    name,
                    &["is-active", "--quiet", UNIT_NAME],
                ))
                .is_ok_and(|o| o.success),
        ),
        _ => None,
    };
    if os == Os::Linux
        && inst.loaded != Some(true)
        && inst.definition.is_some()
        && super::api_answers(
            &super::legacy_socket(home),
            crate::control::Fallback::Tcp(inst.port),
        )
        .await
    {
        inst.loaded = Some(true);
    }
    (inst.definition.is_some() || inst.loaded == Some(true)).then_some(inst)
}

fn probe_src(src: &Path, uid: u32) -> SrcProbe {
    let Ok((parent, name)) = fsx::split(src) else {
        return SrcProbe::Unknown(format!("{} has no file name", src.display()));
    };
    let dir = match Dir::open(parent) {
        Ok(dir) => dir,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return SrcProbe::Missing,
        Err(e) => return SrcProbe::Unknown(e.to_string()),
    };
    match fsx::open_source(&dir, name, uid) {
        Ok((_, meta)) => SrcProbe::Regular { len: meta.len() },
        Err(SourceRefusal::Missing) => SrcProbe::Missing,
        Err(SourceRefusal::Unreadable(e)) => SrcProbe::Unknown(e),
        Err(other) => SrcProbe::Refused(other.to_string()),
    }
}

/// The operator's own identity that setup finds and deliberately leaves
/// behind, so the plan says so instead of the daemon failing quietly later.
fn identity_flags(home: &Path, uid: u32, files: &[(FileKind, PathBuf, SrcProbe)]) -> Vec<String> {
    let mut flags = Vec::new();
    let docker = home.join(".docker/config.json");
    if std::fs::symlink_metadata(&docker).is_ok() {
        flags.push(format!(
            "{}: your Docker credentials stay yours; faber gets a Docker endpoint of its own \
             (ADR-0026)",
            docker.display()
        ));
    }
    let gh = home.join(".config/gh/hosts.yml");
    if std::fs::symlink_metadata(&gh).is_ok() {
        flags.push(format!(
            "{}: your gh login stays yours; faber holds a fine-grained grant of its own \
             (ADR-0027), and waits in standby until it has one",
            gh.display()
        ));
    }
    if let Some((_, path, SrcProbe::Regular { .. })) =
        files.iter().find(|(k, _, _)| *k == FileKind::Config)
    {
        if let Some(lines) = home_lines_in(path, home, uid) {
            flags.push(format!(
                "{} names paths under {} on line(s) {lines}, which {FABER_ACCOUNT} cannot read; \
                 edit them in the copy after setup",
                path.display(),
                home.display()
            ));
        }
    }
    flags
}

fn home_lines_in(path: &Path, home: &Path, uid: u32) -> Option<String> {
    let (parent, name) = fsx::split(path).ok()?;
    let dir = Dir::open(parent).ok()?;
    let (file, _) = fsx::open_source(&dir, name, uid).ok()?;
    let mut text = String::new();
    file.take(1 << 20).read_to_string(&mut text).ok()?;
    let lines = config_home_lines(&text, home);
    (!lines.is_empty()).then(|| {
        lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    })
}

/// 1-based numbers of the lines that name a path under `home`, or `~/`,
/// outside a comment. A scan of bytes, not a parse: root never parses the
/// config it copies.
pub fn config_home_lines(text: &str, home: &Path) -> Vec<usize> {
    let home = format!("{}/", home.display());
    text.lines()
        .enumerate()
        .filter(|(_, line)| {
            let code = line.split('#').next().unwrap_or("");
            code.contains(&home) || code.contains("~/")
        })
        .map(|(i, _)| i + 1)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pmset_is_settled_only_when_every_profile_is() {
        let ac = "AC Power:\n Sleep On Power Button 1\n powernap             0\n \
                  displaysleep         10\n sleep                0\n disksleep            10\n";
        assert!(pmset_settled(ac));
        let asleep = ac.replace(" sleep                0", " sleep                1");
        assert!(!pmset_settled(&asleep));
        let nap = ac.replace("powernap             0", "powernap             1");
        assert!(!pmset_settled(&nap));
        let battery = format!("{ac}Battery Power:\n sleep                5\n");
        assert!(!pmset_settled(&battery));
        assert!(!pmset_settled(""));
    }

    use crate::setup::fsx::tests::{me, Scratch};

    /// A stand-in for `faber check-config --config -`: a script that first
    /// reads all of its stdin, as the real one does (`cat`, never a reader
    /// that may stop at a first match), into `$HOME/seen`, then checks its
    /// arguments and what it read, and reports the way the real one does, by
    /// exit status and stderr. `$HOME/seen` exists only if it ran.
    fn fake_faber(s: &Scratch, verdict: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = s.path("faber");
        std::fs::write(
            &bin,
            format!(
                "#!/bin/sh\ncat > \"$HOME/seen\" || exit 5\n\
                 [ \"$1\" = check-config ] && [ \"$2\" = --config ] && [ \"$3\" = - ] || exit 3\n\
                 [ \"$4\" = --color ] && [ \"$5\" = never ] || exit 6\n\
                 grep -q 'topic' \"$HOME/seen\" || exit 4\n{verdict}\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        bin
    }

    /// Fix for a review finding: the source config reaches the checker on
    /// its stdin, as `--config -`, read once by setup and never staged in a
    /// file. A failure comes back as sanitized text headed with the real
    /// file, for a Refused step.
    #[test]
    fn the_source_check_feeds_the_config_on_stdin() {
        let s = Scratch::new("check");
        let who = me(&s);
        let config = s.path("config.toml");
        std::fs::write(&config, b"topic = \"faber\"\n").unwrap();
        let ok = fake_faber(&s, "exit 0");
        assert_eq!(check_file(&ok, &config, who.uid, None, &s.0), Ok(()));
        assert_eq!(
            std::fs::read(s.path("seen")).unwrap(),
            b"topic = \"faber\"\n",
            "the checker read exactly the file's bytes"
        );
        // As an account: this test's own, standing in for the operator.
        assert_eq!(check_file(&ok, &config, who.uid, Some(who), &s.0), Ok(()));

        // What the checker reads is the file's bytes: without `topic` the
        // fake refuses it.
        std::fs::write(s.path("other.toml"), b"# nothing here\n").unwrap();
        assert!(check_file(&ok, &s.path("other.toml"), who.uid, None, &s.0).is_err());

        let bad = fake_faber(
            &s,
            "printf 'faber: parsing config from stdin: dind needs a wall\\033[2J\\n' >&2; exit 1",
        );
        let err = check_file(&bad, &config, who.uid, None, &s.0).unwrap_err();
        assert!(err.contains("dind needs a wall"), "{err}");
        assert!(
            err.starts_with(&format!("{}:\n", config.display())),
            "headed with the file: {err}"
        );
        assert!(
            !err.contains('\u{1b}'),
            "escapes never reach the terminal: {err:?}"
        );

        // A source some other account owns is refused before anything runs.
        std::fs::remove_file(s.path("seen")).unwrap();
        let err = check_file(&ok, &config, who.uid + 1, None, &s.0).unwrap_err();
        assert!(err.contains("owned by uid"), "{err}");
        assert!(!s.path("seen").exists(), "nothing ran");
    }

    /// The check reads at most 1 MiB of config, the cap `check-config --config
    /// -` applies too: a bigger file is refused in setup with a clear message,
    /// before anything is spawned or written, and exactly 1 MiB is checked
    /// whole.
    #[test]
    fn the_source_check_refuses_a_config_over_1_mib() {
        let s = Scratch::new("check-cap");
        let who = me(&s);
        let config = s.path("config.toml");
        let ok = fake_faber(&s, "exit 0");
        let mut text = b"topic = \"faber\"\n".to_vec();
        text.resize(CONFIG_CAP, b'#');
        std::fs::write(&config, &text).unwrap();
        assert_eq!(check_file(&ok, &config, who.uid, None, &s.0), Ok(()));
        assert_eq!(std::fs::read(s.path("seen")).unwrap().len(), CONFIG_CAP);

        text.push(b'#');
        std::fs::write(&config, &text).unwrap();
        std::fs::remove_file(s.path("seen")).unwrap();
        let err = check_file(&ok, &config, who.uid, None, &s.0).unwrap_err();
        assert!(err.contains("larger than 1 MiB"), "{err}");
        assert!(err.contains(&config.display().to_string()), "{err}");
        assert!(!s.path("seen").exists(), "nothing ran");
    }

    #[test]
    fn home_paths_in_a_config_are_flagged_by_line() {
        let text = "topic = \"faber\"\n# /Users/owner/in-a-comment\n\
                    [[profile]]\nimage = \"x\"\nenv = { A = \"/Users/owner/cache\" }\n\
                    dir = \"~/work\"\n";
        assert_eq!(
            config_home_lines(text, Path::new("/Users/owner")),
            vec![5, 6]
        );
        assert!(config_home_lines("topic = \"faber\"\n", Path::new("/Users/owner")).is_empty());
    }

    #[test]
    fn only_person_homes_are_searched_for_per_user_fabers() {
        assert!(per_user_home(Os::MacOs, 501, Path::new("/Users/owner")));
        assert!(!per_user_home(Os::MacOs, 441, Path::new("/var/empty")));
        assert!(per_user_home(Os::Linux, 1000, Path::new("/srv/owner")));
        assert!(per_user_home(Os::Linux, 999, Path::new("/home/ci")));
        assert!(!per_user_home(Os::Linux, 33, Path::new("/var/www")));
        assert!(!per_user_home(Os::Linux, 65534, Path::new("/nonexistent")));
    }
}
