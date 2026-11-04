//! Windows paths. Nothing exercises this yet — faber does not run on Windows
//! — so this exists to keep the crate self-consistent and compiling there,
//! using the conventional `%APPDATA%` / `%LOCALAPPDATA%` split: roaming
//! config versus local machine state. Windows has only the LEGACY layout:
//! ADR-0025's SYSTEM layout exists for macOS and Linux alone.
//!
//! Same [`Env`] shape as the other two, so the seam is one thing rather than
//! three: the derivations are pure and the process is read in exactly one
//! place (see [`super::macos`], issue #47).
//!
//! Deliberately untested, and note what that means: this module is
//! `cfg(target_os = "windows")`, so neither CI leg compiles it. Tests written
//! here would not run anywhere — the same trap ADR-0012 records for the Linux
//! branches, and the reason its rule is "verify before you claim". Covering
//! these paths means either building for Windows in CI or compiling this module
//! unconditionally; both are choices to make on purpose, not side effects of a
//! test-hygiene fix.

use std::path::PathBuf;

/// The environment the Windows layout is derived from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Env {
    /// `%USERPROFILE%`, falling back to `%HOME%` (set by some POSIX-ish shells
    /// on Windows, e.g. Git Bash) — `None` when neither is set.
    pub home: Option<PathBuf>,
    /// `%APPDATA%` — the roaming-profile directory Windows conventions reserve
    /// for user settings.
    pub appdata: Option<PathBuf>,
    /// `%LOCALAPPDATA%` — local-machine data that should not roam.
    pub local_appdata: Option<PathBuf>,
}

impl Env {
    /// Read the environment from the process. The only impure function here.
    pub fn from_process() -> Self {
        let var = |k: &str| std::env::var_os(k).map(PathBuf::from);
        Self {
            home: var("USERPROFILE").or_else(|| var("HOME")),
            appdata: var("APPDATA"),
            local_appdata: var("LOCALAPPDATA"),
        }
    }

    /// An account whose home is `home`, and nothing else known about it. The
    /// same constructor as the other platforms', so code that builds an `Env`
    /// for someone else's account compiles everywhere.
    pub fn with_home(home: impl Into<PathBuf>) -> Self {
        Self {
            home: Some(home.into()),
            ..Self::default()
        }
    }

    /// `%USERPROFILE%` (or `%HOME%`).
    pub fn home(&self) -> Option<PathBuf> {
        self.home.clone()
    }

    /// `%APPDATA%\faber` — faber's config family (config, overrides, lock,
    /// registry-auth) lives here.
    fn config_home(&self) -> Option<PathBuf> {
        self.appdata.as_ref().map(|d| d.join("faber"))
    }

    /// `%LOCALAPPDATA%\faber` — faber's state, cache and logs.
    fn local_home(&self) -> Option<PathBuf> {
        self.local_appdata.as_ref().map(|d| d.join("faber"))
    }

    /// `%APPDATA%\faber\config.toml`.
    pub fn config_path(&self) -> Option<PathBuf> {
        self.config_home().map(|d| d.join("config.toml"))
    }

    /// `%APPDATA%\faber\overrides.json`.
    pub fn overrides_path(&self) -> Option<PathBuf> {
        self.config_home().map(|d| d.join("overrides.json"))
    }

    /// `%LOCALAPPDATA%\faber`.
    pub fn log_dir(&self) -> Option<PathBuf> {
        self.local_home()
    }

    /// `%LOCALAPPDATA%\faber`.
    pub fn state_dir(&self) -> Option<PathBuf> {
        self.local_home()
    }

    /// `%LOCALAPPDATA%\faber`.
    pub fn cache_dir(&self) -> Option<PathBuf> {
        self.local_home()
    }

    /// `%APPDATA%\faber\faber.lock`.
    pub fn lock_path(&self) -> Option<PathBuf> {
        self.config_home().map(|d| d.join("faber.lock"))
    }

    /// `%APPDATA%\faber\registry-auth`.
    pub fn registry_auth_dir(&self) -> Option<PathBuf> {
        self.config_home().map(|d| d.join("registry-auth"))
    }

    /// `%LOCALAPPDATA%\faber\bin\faber.exe`, falling back to a fixed path when
    /// `%LOCALAPPDATA%` isn't set — this function has no `Option` to return
    /// `None` through, matching the other platforms' `default_bin_path`.
    pub fn default_bin_path(&self) -> PathBuf {
        match self.local_home() {
            Some(dir) => dir.join("bin").join("faber.exe"),
            None => PathBuf::from(r"C:\faber\bin\faber.exe"),
        }
    }

    /// Empty — there is no menu-bar app on Windows.
    pub fn installed_app_dirs(&self) -> Vec<PathBuf> {
        Vec::new()
    }
}
