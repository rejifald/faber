//! macOS LEGACY paths — reproduces exactly what faber's inline `$HOME`-based
//! path construction produced before this module existed. See
//! [`tests::macos_paths_match_todays_literals`] for the byte-for-byte guard.
//! The SYSTEM layout (ADR-0025) is not derived from an environment at all, and
//! lives in [`super::layout`].
//!
//! Every path is derived from an [`Env`] value rather than read out of the
//! process at the point of use. That split exists for the tests: the layout is
//! load-bearing and has to be pinned, and pinning it by *setting* `$HOME` meant
//! calling `std::env::set_var` from a test thread — unsound in a multi-threaded
//! program, and it was corrupting unrelated tests elsewhere in the binary
//! (issue #47). Constructing an `Env` asserts the same thing and touches no
//! process-wide state.

use std::path::PathBuf;

/// The environment the macOS layout is derived from.
///
/// One field today, and still worth being a struct: it is the seam that keeps
/// every derivation below pure, and Linux's equivalent needs four.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Env {
    /// `$HOME` — `None` when it isn't set (a launchd context with a broken
    /// environment, chiefly).
    pub home: Option<PathBuf>,
}

impl Env {
    /// Read the environment from the process.
    ///
    /// The only impure function in this module, and deliberately the only one
    /// with nothing to test: it reads one variable, and every rule applied to
    /// that value lives in the derivations below, where a test can reach it.
    pub fn from_process() -> Self {
        Self {
            home: std::env::var_os("HOME").map(PathBuf::from),
        }
    }

    /// The environment of an account whose home is `home`: how setup finds
    /// the operator's LEGACY files while it runs as root.
    pub fn with_home(home: impl Into<PathBuf>) -> Self {
        Self {
            home: Some(home.into()),
        }
    }

    /// faber's one directory under `~/Library/Application Support`: config,
    /// overrides, the host lock, and per-job registry credentials all live here.
    fn app_support(&self) -> Option<PathBuf> {
        self.home
            .as_ref()
            .map(|h| h.join("Library/Application Support/faber"))
    }

    /// `$HOME`.
    pub fn home(&self) -> Option<PathBuf> {
        self.home.clone()
    }

    /// `~/Library/Application Support/faber/config.toml`.
    pub fn config_path(&self) -> Option<PathBuf> {
        self.app_support().map(|d| d.join("config.toml"))
    }

    /// `~/Library/Application Support/faber/overrides.json`.
    pub fn overrides_path(&self) -> Option<PathBuf> {
        self.app_support().map(|d| d.join("overrides.json"))
    }

    /// `~/Library/Logs/faber`.
    pub fn log_dir(&self) -> Option<PathBuf> {
        self.home.as_ref().map(|h| h.join("Library/Logs/faber"))
    }

    /// `~/Library/Application Support/faber` — same directory as
    /// [`Env::config_path`]'s parent; macOS keeps state and config together,
    /// unlike XDG.
    pub fn state_dir(&self) -> Option<PathBuf> {
        self.app_support()
    }

    /// `~/Library/Caches/faber`.
    pub fn cache_dir(&self) -> Option<PathBuf> {
        self.home.as_ref().map(|h| h.join("Library/Caches/faber"))
    }

    /// `~/Library/Application Support/faber/faber.lock`.
    pub fn lock_path(&self) -> Option<PathBuf> {
        self.app_support().map(|d| d.join("faber.lock"))
    }

    /// `~/Library/Application Support/faber/registry-auth`.
    pub fn registry_auth_dir(&self) -> Option<PathBuf> {
        self.app_support().map(|d| d.join("registry-auth"))
    }

    /// `/usr/local/bin/faber` — independent of `$HOME`.
    pub fn default_bin_path(&self) -> PathBuf {
        PathBuf::from("/usr/local/bin/faber")
    }

    /// `/Applications/faber.app`, then `$HOME/Applications/faber.app` — the
    /// order [`crate::update`]'s `installed_app_dir` already checks them in.
    pub fn installed_app_dirs(&self) -> Vec<PathBuf> {
        let mut dirs = vec![PathBuf::from("/Applications/faber.app")];
        if let Some(h) = &self.home {
            dirs.push(h.join("Applications/faber.app"));
        }
        dirs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `Env` as a launchd context with a working `$HOME` presents it.
    fn env(home: &str) -> Env {
        Env {
            home: Some(PathBuf::from(home)),
        }
    }

    /// The regression guard for the whole refactor: every macOS path renders
    /// to the exact literal string the inline `std::env::var_os("HOME")`
    /// call sites produced before this module existed. If this test needs to
    /// change, an existing install's on-disk layout is changing with it.
    #[test]
    fn macos_paths_match_todays_literals() {
        let e = env("/Users/tester");

        assert_eq!(e.home(), Some(PathBuf::from("/Users/tester")));
        assert_eq!(
            e.config_path(),
            Some(PathBuf::from(
                "/Users/tester/Library/Application Support/faber/config.toml"
            ))
        );
        assert_eq!(
            e.overrides_path(),
            Some(PathBuf::from(
                "/Users/tester/Library/Application Support/faber/overrides.json"
            ))
        );
        assert_eq!(
            e.registry_auth_dir(),
            Some(PathBuf::from(
                "/Users/tester/Library/Application Support/faber/registry-auth"
            ))
        );
        assert_eq!(
            e.lock_path(),
            Some(PathBuf::from(
                "/Users/tester/Library/Application Support/faber/faber.lock"
            ))
        );
        assert_eq!(
            e.state_dir(),
            Some(PathBuf::from(
                "/Users/tester/Library/Application Support/faber"
            ))
        );
        assert_eq!(
            e.log_dir(),
            Some(PathBuf::from("/Users/tester/Library/Logs/faber"))
        );
        assert_eq!(
            e.cache_dir(),
            Some(PathBuf::from("/Users/tester/Library/Caches/faber"))
        );
        assert_eq!(
            e.installed_app_dirs(),
            vec![
                PathBuf::from("/Applications/faber.app"),
                PathBuf::from("/Users/tester/Applications/faber.app"),
            ]
        );
        assert_eq!(e.default_bin_path(), PathBuf::from("/usr/local/bin/faber"));
        assert_eq!(
            crate::platform::legacy(&e).control_socket,
            Some(PathBuf::from(
                "/Users/tester/Library/Application Support/faber/control.sock"
            ))
        );
    }

    /// The `Option`/`Vec` shape every call site already handles: a missing
    /// `$HOME` drops the home-relative paths to `None` (or, for the app
    /// dirs, drops just the user-relative entry) rather than panicking.
    #[test]
    fn missing_home_is_none_not_a_panic() {
        let e = Env::default();

        assert_eq!(e.home(), None);
        assert_eq!(e.config_path(), None);
        assert_eq!(e.overrides_path(), None);
        assert_eq!(e.log_dir(), None);
        assert_eq!(e.state_dir(), None);
        assert_eq!(e.cache_dir(), None);
        assert_eq!(e.lock_path(), None);
        assert_eq!(e.registry_auth_dir(), None);
        assert_eq!(
            e.installed_app_dirs(),
            vec![PathBuf::from("/Applications/faber.app")]
        );
        assert_eq!(e.default_bin_path(), PathBuf::from("/usr/local/bin/faber"));
    }

    /// `from_process` is the one thing here that reads the environment, so the
    /// only claim worth making about it is that it *is* the same derivation —
    /// whatever this host's `$HOME` is, the accessor and a hand-built `Env`
    /// agree. Reads the variable; never writes it.
    #[test]
    fn from_process_reads_home_and_derives_the_same_layout() {
        let live = Env::from_process();
        assert_eq!(live.home, std::env::var_os("HOME").map(PathBuf::from));
        let rebuilt = Env {
            home: live.home.clone(),
        };
        assert_eq!(live.config_path(), rebuilt.config_path());
        assert_eq!(live.state_dir(), rebuilt.state_dir());
    }

    /// `with_home` is the same `Env` a launchd context with that `$HOME`
    /// produces.
    #[test]
    fn with_home_is_that_home() {
        assert_eq!(Env::with_home("/Users/tester"), env("/Users/tester"));
    }

    /// The LEGACY daemon binary is the path `deploy/install.sh` installs, the
    /// LaunchAgent runs, and the updater replaces. Like the literals above, it
    /// is load-bearing for every host that has not run `sudo faber setup`.
    #[test]
    fn legacy_daemon_bin_is_todays_literal() {
        assert_eq!(
            super::super::legacy(&env("/Users/tester")).daemon_bin_path,
            Some(PathBuf::from(
                "/Users/tester/Library/Application Support/faber/bin/faber"
            ))
        );
        assert_eq!(super::super::legacy(&Env::default()).daemon_bin_path, None);
    }
}
