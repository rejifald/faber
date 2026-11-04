//! Linux LEGACY paths — the XDG Base Directory spec, with the spec's own
//! fallbacks when the `XDG_*` variables aren't set. Nothing here had an
//! installed base to stay byte-compatible with (unlike [`super::macos`]), so
//! these are the conventional locations rather than a reproduction of prior
//! behavior. The SYSTEM layout (ADR-0025) is not derived from an environment
//! at all, and lives in [`super::layout`].
//!
//! Every path is derived from an [`Env`] value rather than read out of the
//! process at the point of use — see [`super::macos`] for why (issue #47): the
//! four variables involved here made these tests the loudest env-mutators in
//! the suite, and `std::env::set_var` is unsound in a multi-threaded program.

use std::path::PathBuf;

/// The environment the XDG layout is derived from — `$HOME` plus the three
/// `XDG_*` variables faber consults.
///
/// Holds the **raw** values, unfiltered: "set but empty means unset" is a rule
/// of the spec, so it belongs in the derivation where a test can exercise it,
/// not in the reading.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Env {
    /// `$HOME` — `None` when it isn't set.
    pub home: Option<PathBuf>,
    pub xdg_config_home: Option<PathBuf>,
    pub xdg_state_home: Option<PathBuf>,
    pub xdg_cache_home: Option<PathBuf>,
}

impl Env {
    /// Read the environment from the process.
    ///
    /// The only impure function in this module, and deliberately the only one
    /// with nothing to test: it reads four variables and applies no rules to
    /// them.
    pub fn from_process() -> Self {
        let var = |k: &str| std::env::var_os(k).map(PathBuf::from);
        Self {
            home: var("HOME"),
            xdg_config_home: var("XDG_CONFIG_HOME"),
            xdg_state_home: var("XDG_STATE_HOME"),
            xdg_cache_home: var("XDG_CACHE_HOME"),
        }
    }

    /// The environment of an account whose home is `home`: how setup finds
    /// the operator's LEGACY files while it runs as root. Another account's
    /// `XDG_*` variables can't be known from here, so the spec's defaults
    /// apply.
    pub fn with_home(home: impl Into<PathBuf>) -> Self {
        Self {
            home: Some(home.into()),
            ..Self::default()
        }
    }

    /// `var`, if set and non-empty, else `$HOME/default_rel`. The shape every
    /// XDG variable falls back the same way.
    fn xdg_or(&self, var: &Option<PathBuf>, default_rel: &str) -> Option<PathBuf> {
        match var.as_ref().filter(|v| !v.as_os_str().is_empty()) {
            Some(dir) => Some(dir.clone()),
            None => self.home.as_ref().map(|h| h.join(default_rel)),
        }
    }

    /// `$XDG_CONFIG_HOME`, default `$HOME/.config`.
    fn config_home(&self) -> Option<PathBuf> {
        self.xdg_or(&self.xdg_config_home, ".config")
    }

    /// `$XDG_STATE_HOME`, default `$HOME/.local/state`.
    fn state_home(&self) -> Option<PathBuf> {
        self.xdg_or(&self.xdg_state_home, ".local/state")
    }

    /// `$XDG_CACHE_HOME`, default `$HOME/.cache`.
    fn cache_home(&self) -> Option<PathBuf> {
        self.xdg_or(&self.xdg_cache_home, ".cache")
    }

    /// `$HOME`.
    pub fn home(&self) -> Option<PathBuf> {
        self.home.clone()
    }

    /// `$XDG_CONFIG_HOME/faber/config.toml`.
    pub fn config_path(&self) -> Option<PathBuf> {
        self.config_home()
            .map(|d| d.join("faber").join("config.toml"))
    }

    /// `$XDG_CONFIG_HOME/faber/overrides.json`.
    pub fn overrides_path(&self) -> Option<PathBuf> {
        self.config_home()
            .map(|d| d.join("faber").join("overrides.json"))
    }

    /// `$XDG_STATE_HOME/faber` — XDG has no separate concept for logs, so this
    /// is the same directory [`Env::state_dir`] returns.
    pub fn log_dir(&self) -> Option<PathBuf> {
        self.state_home().map(|d| d.join("faber"))
    }

    /// `$XDG_STATE_HOME/faber`.
    pub fn state_dir(&self) -> Option<PathBuf> {
        self.state_home().map(|d| d.join("faber"))
    }

    /// `$XDG_CACHE_HOME/faber`.
    pub fn cache_dir(&self) -> Option<PathBuf> {
        self.cache_home().map(|d| d.join("faber"))
    }

    /// `$XDG_CONFIG_HOME/faber/faber.lock`.
    pub fn lock_path(&self) -> Option<PathBuf> {
        self.config_home()
            .map(|d| d.join("faber").join("faber.lock"))
    }

    /// `$XDG_CONFIG_HOME/faber/registry-auth`.
    pub fn registry_auth_dir(&self) -> Option<PathBuf> {
        self.config_home()
            .map(|d| d.join("faber").join("registry-auth"))
    }

    /// `/usr/local/bin/faber` — same convention as macOS.
    pub fn default_bin_path(&self) -> PathBuf {
        PathBuf::from("/usr/local/bin/faber")
    }

    /// Empty — there is no menu-bar app on Linux.
    pub fn installed_app_dirs(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A home directory and no `XDG_*` variables at all.
    fn bare(home: &str) -> Env {
        Env {
            home: Some(PathBuf::from(home)),
            ..Env::default()
        }
    }

    /// The default branch: no `XDG_*` variables set, everything falls back
    /// to the conventional `$HOME`-relative location.
    #[test]
    fn falls_back_to_conventional_dirs_when_xdg_is_unset() {
        let e = bare("/home/tester");

        assert_eq!(
            e.config_path(),
            Some(PathBuf::from("/home/tester/.config/faber/config.toml"))
        );
        assert_eq!(
            e.overrides_path(),
            Some(PathBuf::from("/home/tester/.config/faber/overrides.json"))
        );
        assert_eq!(
            e.lock_path(),
            Some(PathBuf::from("/home/tester/.config/faber/faber.lock"))
        );
        assert_eq!(
            e.registry_auth_dir(),
            Some(PathBuf::from("/home/tester/.config/faber/registry-auth"))
        );
        assert_eq!(
            e.log_dir(),
            Some(PathBuf::from("/home/tester/.local/state/faber"))
        );
        assert_eq!(
            e.state_dir(),
            Some(PathBuf::from("/home/tester/.local/state/faber"))
        );
        assert_eq!(
            e.cache_dir(),
            Some(PathBuf::from("/home/tester/.cache/faber"))
        );
        assert_eq!(e.installed_app_dirs(), Vec::<PathBuf>::new());
        assert_eq!(e.default_bin_path(), PathBuf::from("/usr/local/bin/faber"));
        assert_eq!(
            crate::platform::legacy(&e).control_socket,
            Some(PathBuf::from(
                "/home/tester/.local/state/faber/control.sock"
            ))
        );
    }

    /// The honored branch: every `XDG_*` variable, when set and non-empty,
    /// wins over the `$HOME`-relative default.
    #[test]
    fn honors_xdg_vars_when_set() {
        let e = Env {
            home: Some(PathBuf::from("/home/tester")),
            xdg_config_home: Some(PathBuf::from("/custom/config")),
            xdg_state_home: Some(PathBuf::from("/custom/state")),
            xdg_cache_home: Some(PathBuf::from("/custom/cache")),
        };

        assert_eq!(
            e.config_path(),
            Some(PathBuf::from("/custom/config/faber/config.toml"))
        );
        assert_eq!(
            e.overrides_path(),
            Some(PathBuf::from("/custom/config/faber/overrides.json"))
        );
        assert_eq!(
            e.lock_path(),
            Some(PathBuf::from("/custom/config/faber/faber.lock"))
        );
        assert_eq!(
            e.registry_auth_dir(),
            Some(PathBuf::from("/custom/config/faber/registry-auth"))
        );
        assert_eq!(e.log_dir(), Some(PathBuf::from("/custom/state/faber")));
        assert_eq!(e.state_dir(), Some(PathBuf::from("/custom/state/faber")));
        assert_eq!(e.cache_dir(), Some(PathBuf::from("/custom/cache/faber")));
    }

    /// An `XDG_*` variable set to the empty string is "unset", per spec —
    /// not a request to root faber's state at `/`.
    #[test]
    fn empty_xdg_var_is_treated_as_unset() {
        let e = Env {
            xdg_config_home: Some(PathBuf::new()),
            ..bare("/home/tester")
        };

        assert_eq!(
            e.config_path(),
            Some(PathBuf::from("/home/tester/.config/faber/config.toml"))
        );
    }

    /// No `$HOME` and no `XDG_*`: nothing to derive from, and every
    /// home-relative path is `None` rather than a panic or a bare-relative path.
    #[test]
    fn nothing_set_is_none_not_a_relative_path() {
        let e = Env::default();

        assert_eq!(e.home(), None);
        assert_eq!(e.config_path(), None);
        assert_eq!(e.state_dir(), None);
        assert_eq!(e.cache_dir(), None);
        assert_eq!(e.lock_path(), None);
        // ...but an `XDG_*` variable alone is enough for the paths it governs:
        // the spec's fallback needs `$HOME`, the variable itself does not.
        let cfg_only = Env {
            xdg_config_home: Some(PathBuf::from("/custom/config")),
            ..Env::default()
        };
        assert_eq!(
            cfg_only.config_path(),
            Some(PathBuf::from("/custom/config/faber/config.toml"))
        );
        assert_eq!(cfg_only.state_dir(), None);
    }

    /// `from_process` is the one thing here that reads the environment, so the
    /// only claim worth making about it is that it reads the four variables it
    /// says it does. Reads them; never writes them.
    #[test]
    fn from_process_reads_the_four_variables() {
        let live = Env::from_process();
        let var = |k: &str| std::env::var_os(k).map(PathBuf::from);
        assert_eq!(live.home, var("HOME"));
        assert_eq!(live.xdg_config_home, var("XDG_CONFIG_HOME"));
        assert_eq!(live.xdg_state_home, var("XDG_STATE_HOME"));
        assert_eq!(live.xdg_cache_home, var("XDG_CACHE_HOME"));
    }

    /// `with_home` is a home and nothing else: no `XDG_*` variable is invented.
    #[test]
    fn with_home_sets_only_the_home() {
        assert_eq!(Env::with_home("/home/tester"), bare("/home/tester"));
    }

    /// The LEGACY daemon binary sits under the state directory, where the
    /// updater has always put it.
    #[test]
    fn legacy_daemon_bin_is_under_the_state_dir() {
        assert_eq!(
            super::super::legacy(&bare("/home/tester")).daemon_bin_path,
            Some(PathBuf::from("/home/tester/.local/state/faber/bin/faber"))
        );
        assert_eq!(super::super::legacy(&Env::default()).daemon_bin_path, None);
    }
}
