//! The two layouts a host can run faber in, as plain values, and the pure core
//! of choosing between them.
//!
//! - [`legacy`]: today's paths, derived from one account's environment
//!   ([`Env`]). Every host ran this layout before ADR-0025, and it stays byte
//!   for byte what it was.
//! - [`SystemLayout::for_os`]: ADR-0025's fixed, root-anchored table for macOS
//!   and Linux, where faber runs as its own `_faber` account.
//!
//! Both are pure. The SYSTEM tables are keyed by an [`Os`] value instead of
//! being selected by `cfg`, so both compile on every target, and the macOS CI
//! leg pins the Linux table too. (`platform::host` gets the same effect for
//! `parse_docker_root_dir` with `cfg(any(target_os = "linux", test))`.)
//!
//! Setup needs both layouts at once: the operator's LEGACY paths are its
//! migration source and the SYSTEM paths its destination. That is why neither
//! is hidden behind the process-wide accessors in [`crate::platform`], which
//! only ever answer for the layout this process runs in.
//!
//! [`layout_from`] turns the outcome of reading the setup record into that
//! decision. Reading the record is the impure half, and it happens once per
//! process, in [`crate::platform::resolution`].

use std::path::{Path, PathBuf};

use super::record::{SetupRecord, TrustError};
use super::Env;

/// The CLI copy that `sudo faber …` runs, on both OSes. It is root-owned, so
/// root never executes a binary `_faber` can rewrite: the daemon's self-updater
/// only ever replaces its own copy, [`SystemLayout::daemon_bin`].
pub const CLI_BIN: &str = "/usr/local/bin/faber";

/// faber's root-owned directory, on both OSes. It holds the setup record and
/// ADR-0027's credential store, [`CREDENTIALS_DIR`].
pub const ETC_DIR: &str = "/etc/faber";

/// ADR-0027's credential store: one `github-<owner>.json` per owner, owned by
/// `root:_faber` at mode 0640, in a directory root owns at 0755: root writes,
/// faber reads through its group, as for `/etc/faber/grants.toml`. Nobody else
/// is in that group, the operator included ([`FABER_OPERATOR_GROUP`]). Only
/// `sudo faber auth set` writes it; the daemon only reads it, and `_faber`
/// cannot rewrite its own token. A file in the earlier form (owned by `_faber`,
/// 0600) is still read, with a warning, until `sudo faber setup` re-owns it
/// ([`crate::credential`]).
pub const CREDENTIALS_DIR: &str = "/etc/faber/credentials";

/// The setup record. A host is SYSTEM exactly when this file exists and passes
/// the trust rule in [`super::record`].
pub const SETUP_RECORD: &str = "/etc/faber/setup.json";

/// The VM images root owns (ADR-0026: "the result belongs to root"), macOS
/// only. `_faber_vm` reads them and never writes them, so the directory and
/// every one above it are root's alone: under `/var/db`, which is `root:wheel`
/// all the way up. (`/Library` would not do: it is writable by `admin`, and the
/// trust rule, [`crate::wall::trust::root_only`], refuses that.) The Linux
/// base is `linux/disk.raw` with its marker `linux/image.json`
/// ([`crate::setup::linux_image`]).
pub const IMAGE_STORE: &str = "/var/db/faber/images";

/// The tools-disk cache ([`crate::vm::tools_disk`]) on a SYSTEM macOS host:
/// `_faber`'s own directory, mode 0755, beside [`IMAGE_STORE`] and not inside
/// `_faber`'s state directory.
///
/// The supervisor, running as `_faber_vm`, attaches these disks, so it must
/// read them, and `_faber` writes them. `_faber`'s state directory is
/// `_faber:_faber` 0750 and `_faber_vm` is never in `_faber`'s group
/// (ADR-0025), so a cache in there would be unreachable unless `_faber_vm`
/// were given search on the whole state directory: its config, its control
/// socket and everything the daemon writes there. Here `_faber_vm` gets
/// nothing it did not have: the parent, `/var/db/faber`, is root's 0755, and
/// the cache holds only what a release ships (the runner tarball GitHub
/// publishes and faber's guest agent), so its 0755 hides nothing. Root never
/// reads it; `sudo faber setup` only creates it.
pub const TOOLS_DISKS: &str = "/var/db/faber/tools-disks";

/// The VM launcher's directory on a SYSTEM macOS host
/// ([`SystemLayout::launcher_bin`] is `faber-vm` in it): `_faber`'s own, mode
/// 0755, beside [`TOOLS_DISKS`] and not in `_faber`'s state directory.
///
/// The VM supervisor runs as `_faber_vm` and executes the launcher, and the
/// Linux image's build runs it as `_faber_vm` too. `_faber`'s state directory is
/// `_faber:_faber` 0750 and `_faber_vm` is never in `_faber`'s group
/// (ADR-0025), so a launcher in there could not be executed at all. Here every
/// directory above `bin/` is root's and writable by nobody else, and `bin/` is
/// `_faber`'s, so the updater can swap the launcher by rename inside it. That
/// lets `_faber` replace the code `_faber_vm` runs, which the updater already
/// required (ADR-0025's 2026-10-18 amendment).
pub const LAUNCHER_DIR: &str = "/var/db/faber/bin";

/// The Docker CLI the SYSTEM daemon runs on macOS (ADR-0026, amended
/// 2026-10-21): a pinned static build that `sudo faber setup` installs
/// ([`crate::setup::docker_cli`]), `root:wheel` 0755 in a directory that is
/// root's all the way up.
///
/// The daemon runs it by this absolute path ([`crate::platform::docker_cli`]):
/// its `PATH` holds nothing an operator can write, and the only `docker` a Mac
/// usually has is Docker Desktop's, which is the operator's, not faber's. It is
/// under `libexec`, not `bin`, so it never shadows the operator's own `docker`
/// on their `PATH`. Setup already requires `/usr/local` and `/usr/local/bin` to
/// be root's alone for [`CLI_BIN`], so this adds no new trust.
///
/// It is not quite the only program: for `docker build` the CLI also runs
/// plugins (buildx), from `_faber`'s own `~/.docker/cli-plugins` and from the
/// system plugin directories, which setup refuses unless each is root's alone
/// (`crate::setup::docker_cli::PLUGIN_DIRS`; none exists on a stock Mac).
pub const DOCKER_CLI: &str = "/usr/local/libexec/faber/docker";

/// The role account the daemon runs as. Also the name of its primary group,
/// which reads the GitHub token and has no member but `_faber` itself: the
/// operator reads faber's files through [`FABER_OPERATOR_GROUP`] instead.
pub const FABER_ACCOUNT: &str = "_faber";

/// The group through which the operator reads faber's config, logs and audit
/// trail and reaches its control socket. Its members are the operator and
/// `_faber`, which shares what it writes for the operator through it (on
/// macOS the log directory is root's, so `_faber` reaches its own logs only
/// through this group). It is never the token's group: a process the operator
/// runs reads none of faber's credentials.
pub const FABER_OPERATOR_GROUP: &str = "_faber_operator";

/// The role account that runs the host side of faber's VMs (ADR-0026). It never
/// holds a credential, and never joins [`FABER_ACCOUNT`]'s group or
/// [`FABER_OPERATOR_GROUP`].
pub const FABER_VM_ACCOUNT: &str = "_faber_vm";

/// macOS only: the account whose own login session runs faber's macOS VMs
/// (ADR-0051). Separate from [`FABER_VM_ACCOUNT`], which keeps the Linux VM, so a
/// macOS-guest escape cannot reach the Linux engine. It never holds a credential.
pub const FABER_MAC_ACCOUNT: &str = "_faber_mac";

/// Where [`FABER_MAC_ACCOUNT`]'s uid comes from: Background Task Management keeps
/// no records for uids below 500, so a LaunchAgent of one never runs, and the
/// block keeps clear of people's accounts from 501 (ADR-0051).
pub const FABER_MAC_ID_RANGE: std::ops::RangeInclusive<u32> = 600..=699;

/// macOS only: [`FABER_MAC_ACCOUNT`]'s home, `_faber_mac:_faber` `0710`, in
/// root's `/var/db` beside `_faber_vm`'s. Its login session keeps its own files
/// there, its LaunchAgent sits in `Library/LaunchAgents`, and the macOS VM
/// supervisor that agent runs keeps `run/`, `state/` and `clones/` there
/// (ADR-0051).
pub const FABER_MAC_HOME: &str = "/var/db/faber-mac";

/// Which of the two layouts a process resolves its paths in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// `sudo faber setup` has run: the setup record exists and is trusted.
    /// faber runs as `_faber`, and every path is a fixed [`SystemLayout`] one.
    System,
    /// No setup record, or one that failed the trust check. Paths hang off
    /// the running account's environment exactly as before ADR-0025.
    Legacy,
}

/// An OS that has a SYSTEM layout. Windows has none and only ever runs LEGACY.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    MacOs,
    Linux,
}

impl Os {
    /// The OS this binary was built for.
    #[cfg(target_os = "macos")]
    pub const HOST: Os = Os::MacOs;
    /// The OS this binary was built for.
    #[cfg(target_os = "linux")]
    pub const HOST: Os = Os::Linux;
}

/// ADR-0025's SYSTEM layout for one OS: where a set-up host keeps everything.
///
/// Every field is a literal for its OS, and nothing is read from an
/// environment. That is the point: `sudo faber setup` and `sudo faber
/// uninstall` act on these paths as root, and a path that followed `$HOME` or
/// an `XDG_*` variable would let whoever sets it steer root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemLayout {
    /// [`CLI_BIN`].
    pub cli_bin: PathBuf,
    /// [`ETC_DIR`].
    pub etc_dir: PathBuf,
    /// [`SETUP_RECORD`].
    pub setup_record: PathBuf,
    /// [`CREDENTIALS_DIR`].
    pub credentials_dir: PathBuf,
    /// The service definition: the LaunchDaemon plist on macOS, the systemd
    /// system unit on Linux.
    pub service_unit: PathBuf,
    /// `D`, `_faber`'s HOME: config, overrides, the host lock, the image
    /// cache, runner variants and update staging.
    pub state_dir: PathBuf,
    /// `D/bin`: the daemon's copy of faber and its `faber.prev`, which the
    /// updater swaps by same-directory rename (ADR-0006).
    pub bin_dir: PathBuf,
    /// `D/bin/faber`: what the service runs.
    pub daemon_bin: PathBuf,
    /// The VM launcher (ADR-0026) the updater swaps with the daemon: on macOS
    /// `faber-vm` in [`LAUNCHER_DIR`], outside `D`, because `_faber_vm`
    /// executes it and cannot traverse `D`. Its staged copy and its `.prev`
    /// sit beside it, so every swap is a rename in that one directory. Only
    /// macOS runs one; Linux keeps a path of the same shape for parity (a
    /// `_faber`-owned `bin/` under a root-owned directory outside `D`), which
    /// nothing creates.
    pub launcher_bin: PathBuf,
    /// `D/registry-auth`: per-job registry credential directories.
    pub registry_auth_dir: PathBuf,
    /// `stdout.log`, `stderr.log` and `audit.jsonl`.
    pub log_dir: PathBuf,
    /// The runner cache the native backend used. Nothing writes it since that
    /// backend went (ADR-0026, ADR-0052); the directory exists for parity with
    /// LEGACY.
    pub cache_dir: PathBuf,
    /// `_faber_vm`'s HOME, reserved for ADR-0026's VMs. Short and free of
    /// spaces, because Lima puts its sockets under it.
    pub vm_home: PathBuf,
}

/// The per-OS literals [`SystemLayout::for_os`] builds a table from.
///
/// Named fields rather than a positional tuple: `state`, `logs`, `cache` and
/// `vm_home` are all plain `&str`, so a tuple would let two of them be
/// transposed in one OS's arm with nothing — not the compiler, not a review
/// skimming aligned literals — catching it. A field name has to match its own
/// name to compile.
struct SystemLiterals {
    state: &'static str,
    logs: &'static str,
    cache: &'static str,
    vm_home: &'static str,
    launcher_dir: &'static str,
    unit: String,
}

impl SystemLayout {
    /// The SYSTEM table for `os`. Pure, and compiled on every target.
    pub fn for_os(os: Os) -> Self {
        let lit = match os {
            Os::MacOs => SystemLiterals {
                state: "/Library/Application Support/faber",
                logs: "/Library/Logs/faber",
                cache: "/Library/Caches/faber",
                vm_home: "/var/db/faber-vm",
                launcher_dir: LAUNCHER_DIR,
                unit: format!("/Library/LaunchDaemons/{}.plist", super::service::LABEL),
            },
            Os::Linux => SystemLiterals {
                state: "/var/lib/faber",
                logs: "/var/log/faber",
                cache: "/var/cache/faber",
                vm_home: "/var/lib/faber-vm",
                launcher_dir: "/var/lib/faber-shared/bin",
                unit: "/etc/systemd/system/faber.service".to_string(),
            },
        };
        let state_dir = PathBuf::from(lit.state);
        Self {
            cli_bin: PathBuf::from(CLI_BIN),
            etc_dir: PathBuf::from(ETC_DIR),
            setup_record: PathBuf::from(SETUP_RECORD),
            credentials_dir: PathBuf::from(CREDENTIALS_DIR),
            service_unit: PathBuf::from(lit.unit),
            bin_dir: state_dir.join("bin"),
            daemon_bin: state_dir.join("bin/faber"),
            launcher_bin: Path::new(lit.launcher_dir).join("faber-vm"),
            registry_auth_dir: state_dir.join("registry-auth"),
            log_dir: PathBuf::from(lit.logs),
            cache_dir: PathBuf::from(lit.cache),
            vm_home: PathBuf::from(lit.vm_home),
            state_dir,
        }
    }

    /// `_faber`'s HOME, which is [`Self::state_dir`].
    pub fn home(&self) -> &Path {
        &self.state_dir
    }

    /// `D/config.toml`.
    pub fn config_path(&self) -> PathBuf {
        self.state_dir.join("config.toml")
    }

    /// `D/overrides.json`.
    pub fn overrides_path(&self) -> PathBuf {
        self.state_dir.join("overrides.json")
    }

    /// `D/faber.lock`.
    pub fn lock_path(&self) -> PathBuf {
        self.state_dir.join("faber.lock")
    }

    /// `D/control.sock`: the control API's Unix socket (ADR-0028).
    pub fn control_socket(&self) -> PathBuf {
        self.state_dir.join("control.sock")
    }

    /// What the process-wide accessors return under this layout.
    ///
    /// There is no menu-bar app to find and no Docker Desktop data volume to
    /// measure. The app belongs to the operator's session, and the daemon never
    /// touches it; the operator's Docker Desktop is not faber's disk.
    pub fn paths(&self) -> Paths {
        Paths {
            home: Some(self.state_dir.clone()),
            config_path: Some(self.config_path()),
            overrides_path: Some(self.overrides_path()),
            log_dir: Some(self.log_dir.clone()),
            state_dir: Some(self.state_dir.clone()),
            cache_dir: Some(self.cache_dir.clone()),
            lock_path: Some(self.lock_path()),
            control_socket: Some(self.control_socket()),
            registry_auth_dir: Some(self.registry_auth_dir.clone()),
            daemon_bin_path: Some(self.daemon_bin.clone()),
            default_bin_path: self.cli_bin.clone(),
            installed_app_dirs: Vec::new(),
        }
    }
}

/// Every path the accessors in [`crate::platform`] resolve, for one layout.
///
/// Fields are `Option` wherever LEGACY has always treated a missing `$HOME` as
/// a handled case. SYSTEM fills every one of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Paths {
    pub home: Option<PathBuf>,
    pub config_path: Option<PathBuf>,
    pub overrides_path: Option<PathBuf>,
    pub log_dir: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
    pub cache_dir: Option<PathBuf>,
    pub lock_path: Option<PathBuf>,
    /// The control API's Unix socket (ADR-0028): the state directory's
    /// `control.sock`, under either layout.
    pub control_socket: Option<PathBuf>,
    pub registry_auth_dir: Option<PathBuf>,
    pub daemon_bin_path: Option<PathBuf>,
    pub default_bin_path: PathBuf,
    pub installed_app_dirs: Vec<PathBuf>,
}

/// The LEGACY layout for the account `env` describes: exactly what every
/// accessor returned before ADR-0025.
///
/// `env` need not be this process's own. Setup builds one for the operator
/// with [`Env::with_home`] to find the files it migrates.
pub fn legacy(env: &Env) -> Paths {
    Paths {
        home: env.home(),
        config_path: env.config_path(),
        overrides_path: env.overrides_path(),
        log_dir: env.log_dir(),
        state_dir: env.state_dir(),
        cache_dir: env.cache_dir(),
        lock_path: env.lock_path(),
        control_socket: env.state_dir().map(|d| d.join("control.sock")),
        registry_auth_dir: env.registry_auth_dir(),
        // The same derivation as `crate::update::UpdatePaths::bin`: the
        // binary the updater has always installed, run and replaced.
        daemon_bin_path: env.state_dir().map(|d| d.join("bin/faber")),
        default_bin_path: env.default_bin_path(),
        installed_app_dirs: env.installed_app_dirs(),
    }
}

/// The layout decision, and what it rests on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    /// The setup record exists and passed the trust check.
    System(SetupRecord),
    /// There is no setup record: a host that never ran `sudo faber setup`.
    Legacy,
    /// A setup record exists, but faber will not act on it. [`Self::layout`]
    /// reports LEGACY, since no SYSTEM path can be trusted; what else to do
    /// about it is each caller's decision, and each caller says so.
    Untrusted(TrustError),
}

/// Decide the layout from the outcome of reading the setup record, as
/// [`super::record::load_trusted`] reports it.
pub fn layout_from(record: Result<Option<SetupRecord>, TrustError>) -> Resolution {
    match record {
        Ok(Some(record)) => Resolution::System(record),
        Ok(None) => Resolution::Legacy,
        Err(reason) => Resolution::Untrusted(reason),
    }
}

impl Resolution {
    /// The layout this resolution selects.
    pub fn layout(&self) -> Layout {
        match self {
            Resolution::System(_) => Layout::System,
            Resolution::Legacy | Resolution::Untrusted(_) => Layout::Legacy,
        }
    }

    /// The trusted setup record, under SYSTEM.
    pub fn record(&self) -> Option<&SetupRecord> {
        match self {
            Resolution::System(record) => Some(record),
            _ => None,
        }
    }

    /// Why a setup record that exists is not used.
    pub fn untrusted(&self) -> Option<&TrustError> {
        match self {
            Resolution::Untrusted(reason) => Some(reason),
            _ => None,
        }
    }

    /// What to tell the operator about an untrusted record: which file, what
    /// is wrong with it, and the fix. It deliberately says nothing about the
    /// consequence, which differs by caller (the daemon parks, a CLI command
    /// carries on with LEGACY paths), so each caller adds its own.
    ///
    /// Each [`TrustError`] reads as a predicate of the file, hence "it".
    pub fn warning(&self) -> Option<String> {
        self.untrusted().map(|reason| {
            format!(
                "setup record {SETUP_RECORD} is untrusted (it {reason}): \
                 re-run `sudo faber setup`"
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::platform::record::{Account, Accounts, Installed, Operator, SCHEMA};

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    /// Every SYSTEM literal on macOS. Compiled and run on both CI legs. If this
    /// test has to change, a set-up host's on-disk layout changes with it, and
    /// so does what `sudo faber uninstall` removes.
    #[test]
    fn macos_system_table_pins_every_literal() {
        let s = SystemLayout::for_os(Os::MacOs);

        assert_eq!(s.cli_bin, p("/usr/local/bin/faber"));
        assert_eq!(s.etc_dir, p("/etc/faber"));
        assert_eq!(s.setup_record, p("/etc/faber/setup.json"));
        assert_eq!(s.credentials_dir, p("/etc/faber/credentials"));
        assert_eq!(
            s.service_unit,
            p("/Library/LaunchDaemons/io.github.rejifald.faber.plist")
        );
        assert_eq!(s.state_dir, p("/Library/Application Support/faber"));
        assert_eq!(s.bin_dir, p("/Library/Application Support/faber/bin"));
        assert_eq!(
            s.daemon_bin,
            p("/Library/Application Support/faber/bin/faber")
        );
        assert_eq!(s.launcher_bin, p("/var/db/faber/bin/faber-vm"));
        assert_eq!(
            s.registry_auth_dir,
            p("/Library/Application Support/faber/registry-auth")
        );
        assert_eq!(s.log_dir, p("/Library/Logs/faber"));
        assert_eq!(s.cache_dir, p("/Library/Caches/faber"));
        assert_eq!(s.vm_home, p("/var/db/faber-vm"));

        assert_eq!(s.home(), Path::new("/Library/Application Support/faber"));
        assert_eq!(
            s.control_socket(),
            p("/Library/Application Support/faber/control.sock")
        );
        assert_eq!(
            s.paths(),
            Paths {
                home: Some(p("/Library/Application Support/faber")),
                config_path: Some(p("/Library/Application Support/faber/config.toml")),
                overrides_path: Some(p("/Library/Application Support/faber/overrides.json")),
                log_dir: Some(p("/Library/Logs/faber")),
                state_dir: Some(p("/Library/Application Support/faber")),
                cache_dir: Some(p("/Library/Caches/faber")),
                lock_path: Some(p("/Library/Application Support/faber/faber.lock")),
                control_socket: Some(p("/Library/Application Support/faber/control.sock")),
                registry_auth_dir: Some(p("/Library/Application Support/faber/registry-auth")),
                daemon_bin_path: Some(p("/Library/Application Support/faber/bin/faber")),
                default_bin_path: p("/usr/local/bin/faber"),
                installed_app_dirs: vec![],
            }
        );
    }

    /// Every SYSTEM literal on Linux. Also compiled and run on both legs: this
    /// is the one place the macOS leg checks Linux paths.
    #[test]
    fn linux_system_table_pins_every_literal() {
        let s = SystemLayout::for_os(Os::Linux);

        assert_eq!(s.cli_bin, p("/usr/local/bin/faber"));
        assert_eq!(s.etc_dir, p("/etc/faber"));
        assert_eq!(s.setup_record, p("/etc/faber/setup.json"));
        assert_eq!(s.credentials_dir, p("/etc/faber/credentials"));
        assert_eq!(s.service_unit, p("/etc/systemd/system/faber.service"));
        assert_eq!(s.state_dir, p("/var/lib/faber"));
        assert_eq!(s.bin_dir, p("/var/lib/faber/bin"));
        assert_eq!(s.daemon_bin, p("/var/lib/faber/bin/faber"));
        assert_eq!(s.launcher_bin, p("/var/lib/faber-shared/bin/faber-vm"));
        assert_eq!(s.registry_auth_dir, p("/var/lib/faber/registry-auth"));
        assert_eq!(s.log_dir, p("/var/log/faber"));
        assert_eq!(s.cache_dir, p("/var/cache/faber"));
        assert_eq!(s.vm_home, p("/var/lib/faber-vm"));

        assert_eq!(s.home(), Path::new("/var/lib/faber"));
        assert_eq!(s.control_socket(), p("/var/lib/faber/control.sock"));
        assert_eq!(
            s.paths(),
            Paths {
                home: Some(p("/var/lib/faber")),
                config_path: Some(p("/var/lib/faber/config.toml")),
                overrides_path: Some(p("/var/lib/faber/overrides.json")),
                log_dir: Some(p("/var/log/faber")),
                state_dir: Some(p("/var/lib/faber")),
                cache_dir: Some(p("/var/cache/faber")),
                lock_path: Some(p("/var/lib/faber/faber.lock")),
                control_socket: Some(p("/var/lib/faber/control.sock")),
                registry_auth_dir: Some(p("/var/lib/faber/registry-auth")),
                daemon_bin_path: Some(p("/var/lib/faber/bin/faber")),
                default_bin_path: p("/usr/local/bin/faber"),
                installed_app_dirs: vec![],
            }
        );
    }

    /// The daemon's HOME is `D`, and every file it keeps there sits directly
    /// inside it, on both OSes. The service definitions set `HOME=D`, so this
    /// is what makes `home()` and the process environment agree.
    #[test]
    fn system_home_is_the_state_dir_on_both_oses() {
        for os in [Os::MacOs, Os::Linux] {
            let s = SystemLayout::for_os(os);
            assert_eq!(s.home(), s.state_dir.as_path(), "{os:?}");
            for inside in [
                s.config_path(),
                s.overrides_path(),
                s.lock_path(),
                s.control_socket(),
                s.registry_auth_dir.clone(),
                s.bin_dir.clone(),
            ] {
                assert_eq!(inside.parent(), Some(s.state_dir.as_path()), "{os:?}");
            }
            assert_eq!(s.daemon_bin.parent(), Some(s.bin_dir.as_path()), "{os:?}");
        }
    }

    /// The launcher is outside `D` on both OSes: `_faber_vm` executes it, and
    /// `D` is `_faber`'s 0750, which `_faber_vm` cannot traverse. On macOS it
    /// is in [`LAUNCHER_DIR`], beside [`TOOLS_DISKS`] and never in the image
    /// store, which uninstall keeps.
    #[test]
    fn the_launcher_is_outside_the_state_dir() {
        for os in [Os::MacOs, Os::Linux] {
            let s = SystemLayout::for_os(os);
            assert!(!s.launcher_bin.starts_with(&s.state_dir), "{os:?}");
            assert!(!s.launcher_bin.starts_with(&s.vm_home), "{os:?}");
            assert_eq!(s.launcher_bin.file_name().unwrap(), "faber-vm", "{os:?}");
        }
        let mac = SystemLayout::for_os(Os::MacOs);
        let dir = mac.launcher_bin.parent().unwrap();
        assert_eq!(dir, Path::new(LAUNCHER_DIR));
        assert_eq!(dir.parent(), Path::new(TOOLS_DISKS).parent());
        assert!(!mac.launcher_bin.starts_with(IMAGE_STORE));
    }

    /// LEGACY is a faithful projection of the [`Env`] derivations, whose
    /// literals `macos_paths_match_todays_literals` pins. The one new path is
    /// the daemon binary, which must be where the updater has always put it.
    #[test]
    fn legacy_is_exactly_the_env_derivations() {
        for env in [Env::with_home("/Users/tester"), Env::default()] {
            let l = legacy(&env);
            assert_eq!(l.home, env.home());
            assert_eq!(l.config_path, env.config_path());
            assert_eq!(l.overrides_path, env.overrides_path());
            assert_eq!(l.log_dir, env.log_dir());
            assert_eq!(l.state_dir, env.state_dir());
            assert_eq!(l.cache_dir, env.cache_dir());
            assert_eq!(l.lock_path, env.lock_path());
            assert_eq!(
                l.control_socket,
                env.state_dir().map(|d| d.join("control.sock"))
            );
            assert_eq!(l.registry_auth_dir, env.registry_auth_dir());
            assert_eq!(l.default_bin_path, env.default_bin_path());
            assert_eq!(l.installed_app_dirs, env.installed_app_dirs());
            assert_eq!(
                l.daemon_bin_path,
                env.state_dir()
                    .map(|root| crate::update::UpdatePaths::with_root(root).bin())
            );
        }
    }

    fn record() -> SetupRecord {
        SetupRecord {
            schema: SCHEMA,
            operator: Operator {
                uid: 501,
                name: "owner".into(),
            },
            accounts: Accounts {
                faber: Account { uid: 451, gid: 451 },
                faber_vm: Account { uid: 452, gid: 452 },
                faber_mac: None,
                operator_group: None,
            },
            installed: Installed {
                version: "0.4.0".into(),
            },
            migrated_from: None,
            artifacts: vec![],
        }
    }

    #[test]
    fn a_trusted_record_selects_system_and_carries_the_record() {
        let r = layout_from(Ok(Some(record())));
        assert_eq!(r.layout(), Layout::System);
        assert_eq!(r.record(), Some(&record()));
        assert_eq!(r.untrusted(), None);
        assert_eq!(r.warning(), None);
    }

    /// No record is the ordinary case for every host that has not run setup,
    /// so it is LEGACY with nothing to warn about.
    #[test]
    fn no_record_is_legacy_and_quiet() {
        let r = layout_from(Ok(None));
        assert_eq!(r, Resolution::Legacy);
        assert_eq!(r.layout(), Layout::Legacy);
        assert_eq!(r.record(), None);
        assert_eq!(r.warning(), None);
    }

    /// An untrusted record is never half-used: LEGACY, no record, and a
    /// warning that names the file, the defect and the fix.
    #[test]
    fn an_untrusted_record_is_legacy_with_a_reason() {
        let r = layout_from(Err(TrustError::NotRootOwned { uid: 501 }));
        assert_eq!(r.layout(), Layout::Legacy);
        assert_eq!(r.record(), None);
        assert_eq!(r.untrusted(), Some(&TrustError::NotRootOwned { uid: 501 }));
        assert_eq!(
            r.warning().as_deref(),
            Some(
                "setup record /etc/faber/setup.json is untrusted (it is owned by uid 501, \
                 not root): re-run `sudo faber setup`"
            )
        );
    }

    /// Every reason yields the same neutral shape. The consequence is the
    /// caller's to state (the daemon parks, a CLI command falls back to LEGACY
    /// paths), so the warning never claims one.
    #[test]
    fn the_warning_leaves_the_consequence_to_the_caller() {
        let reasons = [
            TrustError::Symlink,
            TrustError::NotRegular,
            TrustError::NotRootOwned { uid: 451 },
            TrustError::Writable { mode: 0o100664 },
            TrustError::ParentNotDir,
            TrustError::ParentNotRootOwned { uid: 501 },
            TrustError::ParentWritable { mode: 0o040777 },
            TrustError::Unreadable("Permission denied (os error 13)".into()),
            TrustError::Malformed("missing field `accounts`".into()),
            TrustError::UnsupportedSchema { found: 2 },
        ];
        for reason in reasons {
            let warning = Resolution::Untrusted(reason.clone())
                .warning()
                .expect("an untrusted record warns");
            assert_eq!(
                warning,
                format!(
                    "setup record /etc/faber/setup.json is untrusted (it {reason}): \
                     re-run `sudo faber setup`"
                )
            );
            for claim in ["LEGACY", "SYSTEM", "park", "using"] {
                assert!(!warning.contains(claim), "{warning}");
            }
        }
    }
}
