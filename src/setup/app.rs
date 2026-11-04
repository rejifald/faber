//! The menu-bar app's install (ADR-0034, amended 2026-10-28 and 2026-10-31),
//! macOS only: `sudo faber setup` puts the `faber.app` built with this binary
//! at [`APPLICATIONS`]`/`[`APP_NAME`], so a host's first install covers
//! everything, every update by setup moves the app along with the daemon, and
//! between them the app updates itself from releases
//! (`desktop/src-tauri/src/updater.rs`).
//!
//! - **Source.** The archive [`APP_ASSET`], a release's asset or a source
//!   build's (`docs/manual-update.md`), found where the guest agent is: in the
//!   directory `--launcher` names, else beside the running binary, judged by
//!   the same rule ([`super::vm_runtime`]'s `binary_source`). None there is a
//!   warning, not a refusal.
//! - **Owner.** The operator, all the way down, never root: the app's updater
//!   renames the bundle in place (`.faber.app.new`, `faber.app.previous`), as
//!   the operator. [`APPLICATIONS`] itself must be root's real directory.
//! - **Version.** The bundle's `CFBundleShortVersionString` must be this
//!   binary's release version, exactly ([`faber_release::check_bundle_version`]),
//!   and its [`BUILD_KEY`] stamp this binary's whole version, `X.Y.Z+g<sha>`:
//!   a source build keeps the release version it started from, so only the
//!   stamp tells two builds of one version apart. An installed app of this
//!   version whose stamp is another build's, or missing, is replaced. A binary
//!   that is not a release ([`release_version`]) installs nothing. Setup never
//!   downgrades: an app that updated itself past this binary is kept.
//!
//! Root never unpacks the archive or parses the bundle. At apply, root reads
//! the archive once through a descriptor that follows no symlink and holds it
//! to the SHA-256 the plan showed, so a swap after the plan installs nothing;
//! those bytes are unpacked by `/usr/bin/tar`, from its stdin, run as the
//! operator into a scratch directory the
//! operator owns ([`EXTRACT_NAME`], the updater's own scratch name, which the
//! app's startup housekeeping removes too), and the version is read by
//! `/usr/bin/plutil` run as the operator ([`super::tools::run_as`]). Nothing
//! from the bundle is ever executed by setup; the app is only opened, in the
//! operator's own GUI session, once it is in place.
//!
//! It is the last thing [`super::apply`] does, after the daemon answers (the
//! app talks to its socket), and a failure rolls nothing back: faber itself is
//! set up, and a re-run plans only this.

use std::path::{Path, PathBuf};

use super::exe::{self, PathFacts};
use super::plan::{self, step, unknown, Action, HostState, Section, Status, Step};
use super::vm_runtime::LauncherSource;
use crate::platform::layout::Os;
use crate::platform::record::Account;

/// Where the app is installed: the directory the updater swaps it in.
pub const APPLICATIONS: &str = "/Applications";
/// The bundle's name there.
pub const APP_NAME: &str = "faber.app";
/// Where the updater keeps the bundle it replaced (`Layout::beside`), and so
/// where setup does: the app deletes it once the new one has run healthy.
pub const PREVIOUS_NAME: &str = "faber.app.previous";
/// The scratch directory the archive is unpacked into: the updater's own
/// (`Layout::extract_dir`), in [`APPLICATIONS`] so the swap is a rename.
pub const EXTRACT_NAME: &str = ".faber.app.extract";
/// The bundle's executable, `Contents/MacOS/faber-menubar` (the desktop
/// crate's name, its `CFBundleExecutable`).
pub const APP_EXECUTABLE: &str = "faber-menubar";
/// The release asset (`release.yml`: `tar -czf faber-app-<target>.tar.gz -C
/// <parent> faber.app`).
pub const APP_ASSET: &str = "faber-app-aarch64-apple-darwin.tar.gz";

/// The `Info.plist` key carrying the build the bundle was made by, in the
/// daemon's own format (`desktop/scripts/assemble.mjs` writes it, as the root
/// `build.rs` derives `FABER_VERSION`).
pub const BUILD_KEY: &str = "FaberBuild";
/// The largest archive setup takes: the updater's own cap on the same asset
/// (`MAX_ASSET_BYTES`). The probe refuses a larger one, so the plan never
/// promises an install that apply would refuse.
pub const MAX_ARCHIVE_BYTES: u64 = 200 * 1024 * 1024;

/// `/Applications/faber.app`.
pub fn app_path() -> PathBuf {
    Path::new(APPLICATIONS).join(APP_NAME)
}

/// What is at [`app_path`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstalledApp {
    Missing,
    /// A real directory: its owner, the first entry inside it some other
    /// account owns (`None`: every one is `owner`'s), its
    /// `CFBundleShortVersionString`, or why it could not be read, and its
    /// [`BUILD_KEY`] stamp (`None`: it has none, as an app built before the
    /// stamp, or it could not be read).
    Bundle {
        owner: u32,
        foreign: Option<PathBuf>,
        version: Result<String, String>,
        build: Option<String>,
    },
    /// A symlink or a file: what it is.
    NotBundle(String),
    Unknown(String),
}

/// Everything the app's step needs. Gathered on macOS ([`probe`]); nothing
/// elsewhere, where no step is planned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppProbe {
    /// [`APP_ASSET`], in the launcher's directory.
    pub source: LauncherSource,
    /// [`APPLICATIONS`] itself, read without following a symlink; `None` when
    /// it does not exist.
    pub applications: Result<Option<PathFacts>, String>,
    pub installed: InstalledApp,
}

impl AppProbe {
    /// Nothing found: what a non-macOS probe returns.
    pub fn nothing() -> Self {
        Self {
            source: LauncherSource::None,
            applications: Ok(None),
            installed: InstalledApp::Missing,
        }
    }
}

/// The release version this binary was built for, `X.Y.Z`: its version
/// before the `+g<sha>` build metadata every build carries (`build.rs`;
/// `release.yml` checks a release reports exactly `X.Y.Z+g<sha>`). `None`
/// for anything else, a pre-release included: only a release installs the
/// app, since only a release has an app of its own version.
pub fn release_version(version: &str) -> Option<String> {
    let core = version.split('+').next().unwrap_or(version);
    let parts: Vec<&str> = core.split('.').collect();
    let strict = parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    strict.then(|| core.to_string())
}

fn sec() -> Section {
    Section::App
}

const S_IFMT: u32 = 0o170_000;
const S_IFDIR: u32 = 0o040_000;

/// Why setup may not install into [`APPLICATIONS`], from its facts.
fn applications_problem(facts: &Option<PathFacts>) -> Option<String> {
    match facts {
        None => Some(format!("{APPLICATIONS} does not exist")),
        Some(f) if f.mode & S_IFMT != S_IFDIR => Some(format!(
            "{APPLICATIONS} is not a directory (a symlink is refused)"
        )),
        Some(f) if f.uid != 0 => Some(format!(
            "{APPLICATIONS} is owned by uid {}, not root",
            f.uid
        )),
        Some(_) => None,
    }
}

/// The app's step (macOS only): kept, installed, replaced, warned about or
/// refused. `None` elsewhere, and without an operator to own it (the
/// preflight already refuses that). `ids` are both role accounts' planned
/// ids, for judging the archive's path. Pure.
pub fn plan_step(s: &HostState, ids: Option<(Account, Account)>) -> Option<Step> {
    if s.os != Os::MacOs {
        return None;
    }
    let op = s.operator.as_ref().ok()?;
    let a = &s.app;
    let subject = app_path().display().to_string();
    let name = &op.name;
    let release = release_version(&s.version);
    let fix_owner = format!("`sudo chown -R {name}:{} \"{subject}\"`", op.gid);

    // What is there, and why it is replaced; or what setup does instead.
    let why = match &a.installed {
        InstalledApp::NotBundle(what) => {
            return Some(step(
                sec(),
                Status::Refused,
                subject,
                format!("{what} where the menu-bar app belongs; move it aside and re-run"),
            ))
        }
        InstalledApp::Unknown(e) => return Some(unknown(s, sec(), subject, e)),
        InstalledApp::Missing => None,
        InstalledApp::Bundle {
            owner,
            foreign,
            version,
            build,
        } => {
            let this_build = build.as_deref() == Some(s.version.as_str());
            let theirs = *owner == op.uid && foreign.is_none();
            let not_theirs = || match foreign {
                Some(p) if *owner == op.uid => {
                    format!("{} inside it is not {name}'s", p.display())
                }
                _ => format!("owned by uid {owner}, not {name}"),
            };
            match (version, &release) {
                (Ok(v), Some(r)) if v == r && this_build && theirs => {
                    return Some(step(
                        sec(),
                        Status::Ok,
                        subject,
                        format!(
                            "{v}, this build's ({}), {name}'s: it updates itself",
                            s.version
                        ),
                    ))
                }
                (Ok(v), Some(r)) if faber_release::is_newer(v, r) => {
                    return Some(if theirs {
                        step(
                            sec(),
                            Status::Ok,
                            subject,
                            format!(
                                "kept: {v} is newer than this release ({r}): it updated itself, \
                                 and setup never downgrades it"
                            ),
                        )
                    } else {
                        step(
                            sec(),
                            Status::Skipped,
                            subject,
                            format!(
                                "warning: kept: {v} is newer than this release ({r}), and setup \
                                 never downgrades it, but it is {}: its updater cannot replace \
                                 it. Give it to {name} ({fix_owner})",
                                not_theirs()
                            ),
                        )
                    })
                }
                (_, None) if theirs => {
                    return Some(step(
                        sec(),
                        Status::Ok,
                        subject,
                        format!(
                            "kept: this binary ({}) is not a release, so setup leaves the app as \
                             it is",
                            s.version
                        ),
                    ))
                }
                (Ok(v), _) if !theirs => Some(format!("{v}, {}", not_theirs())),
                (Ok(v), Some(r)) if faber_release::is_newer(r, v) => {
                    Some(format!("{v}, older than this release"))
                }
                (Ok(v), Some(r)) if v == r => Some(match build {
                    Some(b) => format!("{v}, but built as {b}, not as this binary ({})", s.version),
                    None => format!(
                        "{v}, but with no build stamp, so not this binary's build ({})",
                        s.version
                    ),
                }),
                (Ok(v), _) => Some(format!("{v}, not this release's")),
                (Err(e), _) if !theirs => Some(format!(
                    "its version cannot be read ({e}), and it is {}",
                    not_theirs()
                )),
                (Err(e), _) => Some(format!("its version cannot be read ({e})")),
            }
        }
    };
    let what = why
        .as_deref()
        .map_or_else(|| "missing".to_string(), str::to_string);

    let Some(release) = release else {
        return Some(step(
            sec(),
            Status::Skipped,
            subject,
            format!(
                "warning: not installed ({what}): only a release installs the menu-bar app, and \
                 this binary ({}) is not one. Install it from a release's assets, or with \
                 deploy/install-app.sh for a local build",
                s.version
            ),
        ));
    };
    let (path, chain) = match &a.source {
        LauncherSource::None => {
            return Some(step(
                sec(),
                Status::Skipped,
                subject,
                format!(
                    "warning: no menu-bar app to install ({what}): no {APP_ASSET} in the \
                     launcher's directory. Re-run setup with the release's assets in one \
                     directory, or, for a source build, with its app archive beside the \
                     launcher (docs/manual-update.md)"
                ),
            ))
        }
        LauncherSource::Refused { path, why } => {
            return Some(step(
                sec(),
                Status::Refused,
                subject,
                format!("the menu-bar app to install, {}, {why}", path.display()),
            ))
        }
        LauncherSource::Found { path, chain, .. } => (path, chain),
    };
    if let Err(why) = exe::verdict(chain, &plan::untrusted(s, ids)) {
        return Some(step(
            sec(),
            Status::Refused,
            subject,
            format!(
                "the menu-bar app to install, {}: {why}. Setup installs only an archive faber's \
                 own accounts could not have replaced",
                path.display()
            ),
        ));
    }
    match &a.applications {
        Err(e) => return Some(unknown(s, sec(), subject, e)),
        Ok(facts) => {
            if let Some(problem) = applications_problem(facts) {
                return Some(step(
                    sec(),
                    Status::Refused,
                    subject,
                    format!(
                        "{problem}: setup installs the menu-bar app only into root's \
                         {APPLICATIONS}"
                    ),
                ));
            }
        }
    }
    let how = format!(
        "unpacked by /usr/bin/tar run as {name} (root never unpacks it) into \
         {APPLICATIONS}/{EXTRACT_NAME}, refused unless its CFBundleShortVersionString is \
         {release} and its {BUILD_KEY} is {}; {name}'s all the way down, so it updates itself \
         from then on. Then opened in {name}'s login session, which adds it to the login items",
        s.version
    );
    Some(
        match why {
            None => step(
                sec(),
                Status::WillCreate,
                subject,
                format!("from {}: {how}", path.display()),
            )
            .brief(format!(
                "this build's ({}), {name}'s; it updates itself",
                s.version
            )),
            Some(why) => step(
                sec(),
                Status::WillChange,
                subject,
                format!(
                    "replaced ({why}) from {}: {how}. The one it replaces is quit and kept as \
                 {PREVIOUS_NAME}, which the new one deletes once it runs",
                    path.display()
                ),
            )
            .brief(format!("replaced with this build's ({}): {why}", s.version)),
        }
        .doing(Action::InstallApp),
    )
}

/// The plan's "at a glance" line for the app's steps: the operator runs
/// setup, so the app is "yours".
pub fn glance(steps: &[&Step]) -> Option<(crate::out::Kind, String)> {
    use crate::out::Kind;
    if steps
        .iter()
        .any(|st| st.status == Status::Skipped && st.detail.contains(APP_ASSET))
    {
        return Some((
            Kind::Warning,
            "the menu-bar app is not installed: its release archive is not beside the launcher"
                .to_string(),
        ));
    }
    let st = steps
        .iter()
        .find(|st| st.action == Some(Action::InstallApp))?;
    Some(match st.status {
        Status::WillCreate => (
            Kind::Create,
            format!("the menu-bar app in {APPLICATIONS}, yours; it updates itself from then on"),
        ),
        _ => (
            Kind::Change,
            "the menu-bar app is replaced with this build's".to_string(),
        ),
    })
}

/// The read-only probes, on macOS.
#[cfg(target_os = "macos")]
pub mod probe {
    use std::ffi::OsStr;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::time::Duration;

    use super::*;
    use crate::setup::plan::OperatorInfo;
    use crate::setup::tools::{self, Tool};
    use crate::setup::vm_runtime::probe::named_in;

    /// Where the app comes from: the launcher's directory, the one
    /// `--launcher` names, else `exe`'s, as for the guest agent. A release's
    /// assets sit side by side.
    pub fn app_source(given_launcher: Option<&Path>, exe: &Path) -> LauncherSource {
        within_cap(named_in(
            given_launcher.unwrap_or(exe).parent(),
            &[APP_ASSET],
        ))
    }

    /// `source`, refused when it is larger than [`MAX_ARCHIVE_BYTES`]: apply
    /// would refuse it, so the plan does first.
    pub fn within_cap(source: LauncherSource) -> LauncherSource {
        let LauncherSource::Found { path, .. } = &source else {
            return source;
        };
        match std::fs::symlink_metadata(path) {
            Ok(m) if m.len() > MAX_ARCHIVE_BYTES => LauncherSource::Refused {
                path: path.clone(),
                why: format!(
                    "is larger than {} MiB, more than any release's app",
                    MAX_ARCHIVE_BYTES >> 20
                ),
            },
            _ => source,
        }
    }

    /// `bundle`'s `CFBundleShortVersionString`, read by `/usr/bin/plutil` run
    /// as `account` (`None`: this process's own), never as root: the bundle
    /// is the operator's, and root does not parse it.
    pub fn bundle_version(
        bundle: &Path,
        account: Option<Account>,
        home: &Path,
    ) -> Result<String, String> {
        bundle_string(bundle, "CFBundleShortVersionString", account, home)
    }

    /// `bundle`'s [`BUILD_KEY`] stamp, read as [`bundle_version`] is.
    pub fn bundle_build(
        bundle: &Path,
        account: Option<Account>,
        home: &Path,
    ) -> Result<String, String> {
        bundle_string(bundle, BUILD_KEY, account, home)
    }

    /// The plain string `key` names in `bundle`'s `Info.plist`.
    fn bundle_string(
        bundle: &Path,
        key: &str,
        account: Option<Account>,
        home: &Path,
    ) -> Result<String, String> {
        let plist = bundle.join("Contents/Info.plist");
        let out = tools::run_as(
            Path::new(Tool::Plutil.path()),
            &[
                OsStr::new("-extract"),
                OsStr::new(key),
                OsStr::new("raw"),
                OsStr::new("-o"),
                OsStr::new("-"),
                plist.as_os_str(),
            ],
            None,
            account,
            home,
            Duration::from_secs(30),
        )
        .map_err(|e| format!("could not run plutil: {e:#}"))?;
        if !out.success {
            return Err(format!(
                "plutil could not read {key} from {}: {}",
                plist.display(),
                out.sanitized(200).trim()
            ));
        }
        let v = out.stdout.trim();
        if v.is_empty() || v.len() > 64 || !v.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(format!(
                "{}'s {key} is not a plain version",
                plist.display()
            ));
        }
        Ok(v.to_string())
    }

    /// The first entry under `dir` (a real directory, walked without
    /// following a symlink) that `uid` does not own, or that cannot be read.
    pub fn foreign_entry(dir: &Path, uid: u32) -> Option<PathBuf> {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return Some(dir.to_path_buf()),
        };
        for entry in entries {
            let Ok(entry) = entry else {
                return Some(dir.to_path_buf());
            };
            let path = entry.path();
            match std::fs::symlink_metadata(&path) {
                Ok(m) if m.uid() != uid => return Some(path),
                Ok(m) if m.file_type().is_dir() => {
                    if let Some(p) = foreign_entry(&path, uid) {
                        return Some(p);
                    }
                }
                Ok(_) => {}
                Err(_) => return Some(path),
            }
        }
        None
    }

    /// What is at `bundle`. `op` is the operator, `as_op` whether to drop to
    /// them to read its version (with sudo; without it, this process is
    /// already theirs).
    pub fn installed(bundle: &Path, op: Option<&OperatorInfo>, as_op: bool) -> InstalledApp {
        match std::fs::symlink_metadata(bundle) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => InstalledApp::Missing,
            Err(e) => InstalledApp::Unknown(format!("{}: {e}", bundle.display())),
            Ok(m) if m.file_type().is_symlink() => InstalledApp::NotBundle("a symlink".into()),
            Ok(m) if !m.file_type().is_dir() => {
                InstalledApp::NotBundle("something that is not a directory".into())
            }
            Ok(m) => {
                let owner = m.uid();
                let account = op.filter(|_| as_op).map(|op| Account {
                    uid: op.uid,
                    gid: op.gid,
                });
                let home = op.map_or_else(|| PathBuf::from("/"), |op| op.home.clone());
                // With sudo but no operator to drop to, nothing is read: root
                // never parses the bundle.
                let readable = !(as_op && account.is_none());
                let version = if readable {
                    bundle_version(bundle, account, &home)
                } else {
                    Err("no operator to read it as".to_string())
                };
                // Missing on an app built before the stamp: that app is
                // simply not this build's.
                let build = readable
                    .then(|| bundle_build(bundle, account, &home).ok())
                    .flatten();
                InstalledApp::Bundle {
                    owner,
                    foreign: foreign_entry(bundle, owner),
                    version,
                    build,
                }
            }
        }
    }

    /// Everything [`AppProbe`] holds.
    pub fn gather(
        exe: &Path,
        launcher: Option<&Path>,
        op: Option<&OperatorInfo>,
        privileged: bool,
    ) -> AppProbe {
        let apps = Path::new(APPLICATIONS);
        AppProbe {
            source: app_source(launcher, exe),
            applications: match std::fs::symlink_metadata(apps) {
                Ok(m) => Ok(Some(PathFacts {
                    path: apps.to_path_buf(),
                    uid: m.uid(),
                    gid: m.gid(),
                    mode: m.mode(),
                })),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                Err(e) => Err(format!("{APPLICATIONS}: {e}")),
            },
            installed: installed(&app_path(), op, privileged),
        }
    }
}

/// Installing the app, on macOS.
#[cfg(target_os = "macos")]
pub mod apply {
    use std::ffi::OsStr;
    use std::os::unix::fs::MetadataExt;
    use std::path::Path;
    use std::time::Duration;

    use anyhow::{bail, Context, Result};

    use super::*;
    use crate::setup::fsx::Dir;
    use crate::setup::plan::OperatorInfo;
    use crate::setup::tools::{self, Cmd, Runner, Tool};

    /// How long the operator's `tar` may take to unpack the app.
    const UNPACK_LIMIT: Duration = Duration::from_secs(300);

    /// Install the app the plan found, as its step says. Never fails setup:
    /// faber itself is already set up, so a failure is reported, the scratch
    /// directory removed, and a re-run plans only this.
    pub fn install(s: &HostState, op: &OperatorInfo, runner: &mut dyn Runner) {
        let apps = Path::new(APPLICATIONS);
        let mut quit_it = false;
        let outcome = install_in(s, op, apps, runner, &mut quit_it);
        if let Err(e) = remove_any(&apps.join(EXTRACT_NAME)) {
            println!(
                "  (could not remove {}/{EXTRACT_NAME}: {e})",
                apps.display()
            );
        }
        match outcome {
            Ok(()) => open(op, runner),
            Err(e) => {
                eprintln!(
                    "\nWARNING: the menu-bar app was not installed: {e:#}\n  Everything else is \
                     in place. Fix the cause and re-run `sudo faber setup`."
                );
                // The swap failed after the app was quit: whichever bundle is
                // in place now (the old one, put back) is opened again rather
                // than left quit until the next login.
                if reopen_after_failure(quit_it, apps) {
                    open(op, runner);
                }
            }
        }
    }

    fn install_in(
        s: &HostState,
        op: &OperatorInfo,
        apps_path: &Path,
        runner: &mut dyn Runner,
        quit_it: &mut bool,
    ) -> Result<()> {
        let LauncherSource::Found {
            path: archive,
            sha256,
            ..
        } = &s.app.source
        else {
            bail!("the plan installs the menu-bar app but found none to install");
        };
        let Some(release) = release_version(&s.version) else {
            bail!("this binary ({}) is not a release", s.version);
        };
        let account = Account {
            uid: op.uid,
            gid: op.gid,
        };
        let apps = Dir::open_nofollow(apps_path)
            .with_context(|| format!("opening {} (a symlink is refused)", apps_path.display()))?;
        let meta = apps.metadata()?;
        if meta.uid() != 0 {
            bail!(
                "{} is owned by uid {}, not root",
                apps_path.display(),
                meta.uid()
            );
        }
        // The archive's bytes, read once through a descriptor that follows no
        // symlink and held to the digest the plan showed: whoever can write a
        // directory on its path cannot swap it between the plan and now. Root
        // only hashes them; `tar` reads them from its stdin, as the operator.
        let bytes = archive_bytes(archive, sha256)?;
        let scratch = fresh_scratch(&apps, apps_path, account)?;
        let scratch_path = apps_path.join(EXTRACT_NAME);
        println!(
            "  unpacking {} as {} into {}",
            archive.display(),
            op.name,
            scratch_path.display()
        );
        unpack(&bytes, archive, &scratch_path, Some(account), &op.home)?;
        drop(bytes);
        check_unpacked(&scratch, &scratch_path, op.uid)
            .with_context(|| format!("the archive is not fit to be {}'s app", op.name))?;
        let bundle_path = scratch_path.join(APP_NAME);
        let short = probe::bundle_version(&bundle_path, Some(account), &op.home)
            .map_err(anyhow::Error::msg)?;
        faber_release::check_bundle_version(&short, &format!("v{release}"))
            .map_err(anyhow::Error::msg)?;
        let build = probe::bundle_build(&bundle_path, Some(account), &op.home)
            .map_err(anyhow::Error::msg)?;
        if build != s.version {
            bail!(
                "the archive's app was built as {build}, not as this binary ({}): build them \
                 from one tree",
                s.version
            );
        }
        quit(op, runner);
        *quit_it = true;
        swap_in(&apps, &scratch)?;
        // A bundle that was not the operator's all the way down is not kept as
        // `.previous`: the new app could not delete it, and its rollback could
        // put it back. Root removes it, following no symlink.
        if let Err(e) = drop_foreign_previous(apps_path, &s.app.installed, op.uid) {
            println!(
                "  (could not remove {}/{PREVIOUS_NAME}, which was not {}'s: {e})",
                apps_path.display(),
                op.name
            );
        }
        println!(
            "  installed the menu-bar app {build} at {}, {}'s",
            app_path().display(),
            op.name
        );
        Ok(())
    }

    /// [`EXTRACT_NAME`] in `apps`, made afresh (a stale one removed first,
    /// never following a symlink) and given to `owner`, 0755.
    fn fresh_scratch(apps: &Dir, apps_path: &Path, owner: Account) -> Result<Dir> {
        let path = apps_path.join(EXTRACT_NAME);
        remove_any(&path).with_context(|| format!("removing a stale {}", path.display()))?;
        if !apps.mkdir(EXTRACT_NAME)? {
            bail!("{} reappeared while setup made it", path.display());
        }
        let scratch = apps.subdir(EXTRACT_NAME)?;
        scratch.set_owner_mode(owner, 0o755)?;
        Ok(scratch)
    }

    /// `archive`'s bytes, opened without following a symlink at the file or
    /// at its directory ([`crate::setup::fsx::open_leaf_dir`], as
    /// `file_digest` does), and refused unless they are still a regular file,
    /// at most [`MAX_ARCHIVE_BYTES`], whose SHA-256 is `sha256`, the one the
    /// plan showed.
    pub(super) fn archive_bytes(archive: &Path, sha256: &str) -> Result<Vec<u8>> {
        use std::io::Read;
        let (parent, name) = crate::setup::fsx::split(archive)?;
        let dir = crate::setup::fsx::open_leaf_dir(parent)
            .with_context(|| format!("opening {} (a symlink is refused)", parent.display()))?;
        let file = dir
            .open_file(name)
            .with_context(|| format!("opening {} (a symlink is refused)", archive.display()))?;
        if !file.metadata()?.file_type().is_file() {
            bail!("{} is no longer a regular file", archive.display());
        }
        let mut bytes = Vec::new();
        file.take(MAX_ARCHIVE_BYTES + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("reading {}", archive.display()))?;
        if bytes.len() as u64 > MAX_ARCHIVE_BYTES {
            bail!(
                "{} is larger than {} MiB",
                archive.display(),
                MAX_ARCHIVE_BYTES >> 20
            );
        }
        let got = crate::setup::fsx::sha256_of(&mut bytes.as_slice())?;
        if got != sha256 {
            bail!(
                "{} changed after the plan was made (SHA-256 {got}, the plan showed {sha256}); \
                 nothing was installed",
                archive.display()
            );
        }
        Ok(bytes)
    }

    /// Remove [`PREVIOUS_NAME`] in `apps_path` unless the bundle it holds,
    /// as the plan saw it (`replaced`), was `uid`'s all the way down. Follows
    /// no symlink; nothing there is fine.
    pub(super) fn drop_foreign_previous(
        apps_path: &Path,
        replaced: &InstalledApp,
        uid: u32,
    ) -> std::io::Result<()> {
        let theirs = matches!(
            replaced,
            InstalledApp::Bundle { owner, foreign: None, .. } if *owner == uid
        );
        if theirs {
            return Ok(());
        }
        remove_any(&apps_path.join(PREVIOUS_NAME))
    }

    /// Whether to open the app again after a failed install: only when setup
    /// had quit it, and a bundle is in place to open.
    pub(super) fn reopen_after_failure(quit_it: bool, apps_path: &Path) -> bool {
        quit_it && apps_path.join(APP_NAME).is_dir()
    }

    /// The unpacked [`APP_NAME`] in `scratch`: a real directory (a symlink
    /// is refused), and `uid`'s, with every entry in it.
    pub(super) fn check_unpacked(scratch: &Dir, scratch_path: &Path, uid: u32) -> Result<()> {
        let bundle = scratch
            .subdir(APP_NAME)
            .with_context(|| format!("it holds no {APP_NAME} directory (a symlink is refused)"))?;
        let bundle_path = scratch_path.join(APP_NAME);
        if bundle.metadata()?.uid() != uid {
            bail!("{} is not uid {uid}'s", bundle_path.display());
        }
        if let Some(p) = probe::foreign_entry(&bundle_path, uid) {
            bail!("{} inside it is not uid {uid}'s", p.display());
        }
        Ok(())
    }

    /// `/usr/bin/tar xzf - -C into`, run as `account` (`None`: this process,
    /// for tests) with the archive's `bytes` on its stdin: the files come out
    /// theirs, and root never parses the archive. `archive` only names it in
    /// an error.
    pub(super) fn unpack(
        bytes: &[u8],
        archive: &Path,
        into: &Path,
        account: Option<Account>,
        home: &Path,
    ) -> Result<()> {
        let out = tools::run_as(
            Path::new("/usr/bin/tar"),
            &[
                OsStr::new("xzf"),
                OsStr::new("-"),
                OsStr::new("-C"),
                into.as_os_str(),
            ],
            Some(bytes),
            account,
            home,
            UNPACK_LIMIT,
        )?;
        if !out.success {
            bail!(
                "/usr/bin/tar could not unpack {}:\n{}",
                archive.display(),
                out.sanitized(2000)
            );
        }
        Ok(())
    }

    /// Remove `path` and everything in it, never following a symlink
    /// (`remove_dir_all` removes a symlink in a path's place, never its
    /// target). Nothing there is fine.
    pub(super) fn remove_any(path: &Path) -> std::io::Result<()> {
        match std::fs::symlink_metadata(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
            Ok(m) if m.file_type().is_dir() => std::fs::remove_dir_all(path),
            Ok(_) => std::fs::remove_file(path),
        }
    }

    /// The updater's swap (`Layout::swap_in`), in `apps`: the installed
    /// bundle, if any, renamed to [`PREVIOUS_NAME`] (a stale one removed
    /// first), then the unpacked one moved from `scratch` to [`APP_NAME`],
    /// never over anything. If that fails, the old one is put back.
    pub(super) fn swap_in(apps: &Dir, scratch: &Dir) -> Result<()> {
        let previous = apps.path().join(PREVIOUS_NAME);
        remove_any(&previous)
            .with_context(|| format!("removing a stale {}", previous.display()))?;
        let had_old = apps.exists(APP_NAME)?;
        if had_old {
            apps.rename(APP_NAME, PREVIOUS_NAME).with_context(|| {
                format!("moving the installed app aside to {}", previous.display())
            })?;
        }
        if let Err(e) = scratch.move_new(APP_NAME, apps, APP_NAME) {
            if had_old {
                if let Err(back) = apps.rename(PREVIOUS_NAME, APP_NAME) {
                    bail!(
                        "moving the new app into place: {e}; and putting the old one back: \
                         {back}. It is at {}",
                        previous.display()
                    );
                }
            }
            bail!("moving the new app into place: {e}; the installed one is unchanged");
        }
        Ok(())
    }

    /// `launchctl asuser <uid> sudo -u #<uid> <program> <args>`: a command in
    /// the operator's own GUI session, as them (as `auth.rs` reaches their
    /// clipboard).
    pub(super) fn as_operator(uid: u32, program: &str, args: &[&str]) -> Cmd {
        let mut all = vec![
            "asuser".to_string(),
            uid.to_string(),
            "/usr/bin/sudo".to_string(),
            "-u".to_string(),
            format!("#{uid}"),
            program.to_string(),
        ];
        all.extend(args.iter().map(|a| a.to_string()));
        Cmd::new(Tool::Launchctl, all)
    }

    /// Quit a running app, so the swap does not pull its bundle from under
    /// it. Best effort: no app running is fine. A signal, not an Apple Event
    /// (`osascript`), which may wait on an Automation consent prompt nobody
    /// sees.
    fn quit(op: &OperatorInfo, runner: &mut dyn Runner) {
        let _ = runner.run(&quit_cmd(op.uid));
    }

    /// `pkill -TERM -u <uid> -x faber-menubar`: the bundle's executable
    /// (`CFBundleExecutable`, the desktop crate's name), the operator's only.
    pub(super) fn quit_cmd(uid: u32) -> Cmd {
        Cmd::new(
            Tool::Pkill,
            ["-TERM", "-u", &uid.to_string(), "-x", APP_EXECUTABLE],
        )
    }

    /// Open the installed app in the operator's session: its first launch
    /// adds it to their login items. Best effort: with nobody logged in at the
    /// console, it starts once they open it.
    fn open(op: &OperatorInfo, runner: &mut dyn Runner) {
        let app = app_path();
        let cmd = as_operator(op.uid, "/usr/bin/open", &[&app.display().to_string()]);
        match runner.run(&cmd) {
            Ok(out) if out.success => {
                println!("  opened the menu-bar app in {}'s session", op.name)
            }
            _ => println!(
                "  the menu-bar app could not be opened in {}'s session (nobody logged in at the \
                 console?): it starts once {} opens {}",
                op.name,
                op.name,
                app.display()
            ),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::setup::plan::tests::{fresh_mac, render, set_up_mac};
    use crate::setup::plan::{counts, plan};

    pub(crate) const APP_SHA: &str =
        "a99a99a99a99a99a99a99a99a99a99a99a99a99a99a99a99a99a99a99a99a99a";

    fn root_dir(path: &str) -> PathFacts {
        PathFacts {
            path: path.into(),
            uid: 0,
            gid: 0,
            mode: 0o040_755,
        }
    }

    /// The release's assets in `/tmp/v040`, the operator's, as the cut
    /// runbook stages them.
    fn source() -> LauncherSource {
        let archive = format!("/private/tmp/v040/{APP_ASSET}");
        LauncherSource::Found {
            path: archive.clone().into(),
            sha256: APP_SHA.into(),
            chain: vec![
                PathFacts {
                    path: archive.into(),
                    uid: 501,
                    gid: 0,
                    mode: 0o100_644,
                },
                PathFacts {
                    path: "/private/tmp/v040".into(),
                    uid: 501,
                    gid: 0,
                    mode: 0o040_755,
                },
                PathFacts {
                    path: "/private/tmp".into(),
                    uid: 0,
                    gid: 0,
                    mode: 0o041_777,
                },
                root_dir("/private"),
                root_dir("/"),
            ],
        }
    }

    fn applications() -> Option<PathFacts> {
        Some(PathFacts {
            path: APPLICATIONS.into(),
            uid: 0,
            gid: 80,
            mode: 0o040_775,
        })
    }

    /// A bundle reporting `version`, owned by `owner` all the way down, built
    /// from the commit `fresh_mac()`'s binary was (`+gabc1234`).
    fn bundle(owner: u32, version: &str) -> InstalledApp {
        built(owner, version, Some(&format!("{version}+gabc1234")))
    }

    /// [`bundle`], stamped `build` (`None`: unstamped).
    fn built(owner: u32, version: &str, build: Option<&str>) -> InstalledApp {
        InstalledApp::Bundle {
            owner,
            foreign: None,
            version: Ok(version.into()),
            build: build.map(str::to_string),
        }
    }

    /// A host the app was installed on by this release: the operator's.
    pub(crate) fn installed() -> AppProbe {
        AppProbe {
            source: LauncherSource::None,
            applications: Ok(applications()),
            installed: bundle(501, "0.4.0"),
        }
    }

    /// `fresh_mac()` with the release's app beside the launcher.
    fn with_source() -> HostState {
        let mut s = fresh_mac();
        s.app = AppProbe {
            source: source(),
            applications: Ok(applications()),
            installed: InstalledApp::Missing,
        };
        s
    }

    fn app_step(s: &HostState) -> Step {
        plan(s)
            .into_iter()
            .find(|st| st.section == Section::App)
            .unwrap_or_else(|| panic!("no app step:\n{}", render(s, &plan(s))))
    }

    #[test]
    fn the_release_version_is_the_stamp_before_its_build_metadata() {
        assert_eq!(release_version("0.4.0+gabc1234").as_deref(), Some("0.4.0"));
        assert_eq!(release_version("0.4.0").as_deref(), Some("0.4.0"));
        assert_eq!(release_version("1.12.30").as_deref(), Some("1.12.30"));
        for v in ["0.4.0-rc1+gabc", "0.4", "0.4.0.1", "v0.4.0", "x.y.z", ""] {
            assert_eq!(release_version(v), None, "{v}");
        }
    }

    /// A fresh host with the release's app beside the launcher installs it,
    /// as the operator's; the step runs after the service.
    #[test]
    fn a_fresh_host_installs_the_app() {
        let s = with_source();
        let st = app_step(&s);
        assert_eq!(st.status, Status::WillCreate, "{}", st.detail);
        assert_eq!(st.subject, "/Applications/faber.app");
        assert_eq!(st.action, Some(Action::InstallApp));
        assert!(
            st.detail.contains("run as owner (root never"),
            "{}",
            st.detail
        );
        assert!(st.detail.contains("CFBundleShortVersionString is 0.4.0"));
        assert!(st.detail.contains(APP_ASSET), "{}", st.detail);
        let steps = plan(&s);
        assert_eq!(counts(&steps).2, 0, "{}", render(&s, &steps));
        let pos = |f: &dyn Fn(&Step) -> bool| steps.iter().position(f).unwrap();
        assert!(pos(&|st| st.section == Section::App) > pos(&|st| st.section == Section::Service));
    }

    /// This release's app, the operator's: nothing to do, so a re-run plans
    /// nothing.
    #[test]
    fn this_releases_app_is_kept_and_a_rerun_plans_nothing() {
        let mut s = set_up_mac();
        s.app.source = source();
        let st = app_step(&s);
        assert_eq!(st.status, Status::Ok, "{}", st.detail);
        assert!(st.action.is_none());
        let s = set_up_mac();
        assert_eq!(app_step(&s).status, Status::Ok);
    }

    /// An app that updated itself past this binary is kept: setup never
    /// downgrades it, with or without an archive.
    #[test]
    fn a_newer_app_is_never_downgraded() {
        for source in [source(), LauncherSource::None] {
            let mut s = set_up_mac();
            s.app.source = source;
            s.app.installed = bundle(501, "0.4.1");
            let st = app_step(&s);
            assert_eq!(st.status, Status::Ok, "{}", st.detail);
            assert!(st.detail.contains("never downgrades"), "{}", st.detail);
            // Newer, but someone else's: still kept, with a warning.
            s.app.installed = bundle(0, "0.5.0");
            let st = app_step(&s);
            assert_eq!(st.status, Status::Skipped, "{}", st.detail);
            assert!(st.action.is_none());
            assert!(
                st.detail.contains("sudo chown -R owner:20"),
                "{}",
                st.detail
            );
        }
    }

    /// Older, someone else's, or of an unreadable version: replaced.
    #[test]
    fn an_older_foreign_or_unreadable_app_is_replaced() {
        let cases = [
            (bundle(501, "0.3.12"), "0.3.12, older than this release"),
            (bundle(0, "0.4.0"), "owned by uid 0, not owner"),
            (
                InstalledApp::Bundle {
                    owner: 501,
                    foreign: Some("/Applications/faber.app/Contents/MacOS/faber".into()),
                    version: Ok("0.4.0".into()),
                    build: None,
                },
                "Contents/MacOS/faber inside it is not owner's",
            ),
            (
                InstalledApp::Bundle {
                    owner: 501,
                    foreign: None,
                    version: Err("no Info.plist".into()),
                    build: None,
                },
                "its version cannot be read (no Info.plist)",
            ),
        ];
        for (installed, why) in cases {
            let mut s = with_source();
            s.app.installed = installed.clone();
            let st = app_step(&s);
            assert_eq!(
                st.status,
                Status::WillChange,
                "{installed:?}: {}",
                st.detail
            );
            assert_eq!(st.action, Some(Action::InstallApp));
            assert!(st.detail.contains(why), "{why}: {}", st.detail);
            assert!(st.detail.contains(PREVIOUS_NAME), "{}", st.detail);
            // Without an archive: the same verdict, as a warning.
            s.app.source = LauncherSource::None;
            let st = app_step(&s);
            assert_eq!(st.status, Status::Skipped, "{}", st.detail);
            assert!(
                st.detail.contains("no menu-bar app to install"),
                "{}",
                st.detail
            );
            assert!(st.detail.contains(why), "{}", st.detail);
        }
    }

    /// A source build keeps its release version, so an app of the same
    /// version is this build's only when its stamp says so: another build's,
    /// or none (an app built before the stamp), is replaced, and without an
    /// archive the same verdict is a warning that names the source build's
    /// recipe. One stamped as this binary is kept.
    #[test]
    fn an_app_of_this_version_but_another_build_is_replaced() {
        let cases = [
            (
                built(501, "0.4.0", Some("0.4.0+gdef5678")),
                "0.4.0, but built as 0.4.0+gdef5678, not as this binary (0.4.0+gabc1234)",
            ),
            (
                built(501, "0.4.0", None),
                "0.4.0, but with no build stamp, so not this binary's build (0.4.0+gabc1234)",
            ),
        ];
        for (installed, why) in cases {
            let mut s = with_source();
            s.app.installed = installed.clone();
            let st = app_step(&s);
            assert_eq!(
                st.status,
                Status::WillChange,
                "{installed:?}: {}",
                st.detail
            );
            assert_eq!(st.action, Some(Action::InstallApp));
            assert!(st.detail.contains(why), "{why}: {}", st.detail);
            assert!(
                st.detail.contains("its FaberBuild is 0.4.0+gabc1234"),
                "{}",
                st.detail
            );
            s.app.source = LauncherSource::None;
            let st = app_step(&s);
            assert_eq!(st.status, Status::Skipped, "{}", st.detail);
            assert!(st.detail.contains(why), "{}", st.detail);
            assert!(st.detail.contains("docs/manual-update.md"), "{}", st.detail);
        }
        let mut s = with_source();
        s.app.installed = built(501, "0.4.0", Some("0.4.0+gabc1234"));
        let st = app_step(&s);
        assert_eq!(st.status, Status::Ok, "{}", st.detail);
        assert!(st.action.is_none());
    }

    #[test]
    fn without_an_archive_setup_warns() {
        let st = app_step(&fresh_mac());
        assert_eq!(st.status, Status::Skipped);
        assert!(st.action.is_none());
        assert!(st
            .detail
            .contains("Re-run setup with the release's assets in one directory"));
    }

    /// Only a release installs the app: a binary that is not one warns and
    /// installs nothing, and keeps the operator's app as it is.
    #[test]
    fn a_binary_that_is_not_a_release_installs_nothing() {
        let mut s = with_source();
        s.version = "0.4.0-rc1+gabc1234".into();
        let st = app_step(&s);
        assert_eq!(st.status, Status::Skipped, "{}", st.detail);
        assert!(
            st.detail.contains("only a release installs"),
            "{}",
            st.detail
        );
        s.app.installed = bundle(501, "0.3.12");
        let st = app_step(&s);
        assert_eq!(st.status, Status::Ok, "{}", st.detail);
        assert!(st.action.is_none());
    }

    /// Something other than a bundle where the app belongs is refused.
    #[test]
    fn a_symlink_or_file_at_the_app_path_is_refused() {
        for what in ["a symlink", "something that is not a directory"] {
            let mut s = with_source();
            s.app.installed = InstalledApp::NotBundle(what.into());
            let st = app_step(&s);
            assert_eq!(st.status, Status::Refused, "{}", st.detail);
            assert!(st.detail.contains("move it aside and re-run"));
        }
    }

    /// `/Applications` must be root's real directory.
    #[test]
    fn applications_must_be_roots_directory() {
        let bad = [
            None,
            Some(PathFacts {
                path: APPLICATIONS.into(),
                uid: 0,
                gid: 0,
                mode: 0o120_755,
            }),
            Some(PathFacts {
                path: APPLICATIONS.into(),
                uid: 501,
                gid: 20,
                mode: 0o040_755,
            }),
        ];
        for facts in bad {
            let mut s = with_source();
            s.app.applications = Ok(facts.clone());
            let st = app_step(&s);
            assert_eq!(st.status, Status::Refused, "{facts:?}: {}", st.detail);
        }
    }

    /// An archive faber's own accounts could have replaced, or one the probe
    /// refused, is refused.
    #[test]
    fn an_untrusted_or_refused_archive_is_refused() {
        let mut s = with_source();
        s.faber_user = set_up_mac().faber_user;
        if let LauncherSource::Found { chain, .. } = &mut s.app.source {
            chain[1].uid = s.faber_user.as_ref().unwrap().uid;
        }
        let st = app_step(&s);
        assert_eq!(st.status, Status::Refused, "{}", st.detail);
        assert!(st.detail.contains("faber's own accounts"), "{}", st.detail);
        let mut s = with_source();
        s.app.source = LauncherSource::Refused {
            path: format!("/tmp/{APP_ASSET}").into(),
            why: "is a symlink; give the file itself".into(),
        };
        assert_eq!(app_step(&s).status, Status::Refused);
    }

    #[test]
    fn linux_plans_no_app_step() {
        let mut s = crate::setup::plan::tests::fresh_linux();
        s.app = with_source().app;
        assert!(plan(&s).iter().all(|st| st.section != Section::App));
    }

    #[cfg(target_os = "macos")]
    mod mac {
        use super::super::apply::{
            archive_bytes, as_operator, check_unpacked, drop_foreign_previous, quit_cmd,
            remove_any, reopen_after_failure, swap_in, unpack,
        };
        use super::super::probe::app_source;
        use super::super::probe::within_cap;
        use super::*;
        use crate::setup::fsx::Dir;

        fn scratch(name: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("fbr-app-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::canonicalize(&dir).unwrap()
        }

        fn marker(bundle: &Path) -> String {
            std::fs::read_to_string(bundle.join("marker")).unwrap()
        }

        fn make_bundle(at: &Path, text: &str) {
            std::fs::create_dir_all(at).unwrap();
            std::fs::write(at.join("marker"), text).unwrap();
        }

        /// The archive is found in the launcher's directory, else beside the
        /// binary; none is `None`; a symlink is refused.
        #[test]
        fn the_archive_is_found_where_the_agent_is() {
            let root = scratch("source");
            let (assets, bin) = (root.join("assets"), root.join("bin"));
            std::fs::create_dir_all(&assets).unwrap();
            std::fs::create_dir_all(&bin).unwrap();
            let launcher = assets.join("faber-launcher-aarch64-apple-darwin");
            let exe = bin.join("faber");
            assert_eq!(app_source(Some(&launcher), &exe), LauncherSource::None);
            assert_eq!(app_source(None, &exe), LauncherSource::None);

            std::fs::write(assets.join(APP_ASSET), b"tgz").unwrap();
            match app_source(Some(&launcher), &exe) {
                LauncherSource::Found { path, .. } => assert_eq!(path, assets.join(APP_ASSET)),
                other => panic!("{other:?}"),
            }
            // With --launcher, only its directory is looked in.
            std::fs::write(bin.join(APP_ASSET), b"tgz").unwrap();
            std::fs::remove_file(assets.join(APP_ASSET)).unwrap();
            assert_eq!(app_source(Some(&launcher), &exe), LauncherSource::None);
            // Without it, beside the binary.
            match app_source(None, &exe) {
                LauncherSource::Found { path, .. } => assert_eq!(path, bin.join(APP_ASSET)),
                other => panic!("{other:?}"),
            }
            std::fs::remove_file(bin.join(APP_ASSET)).unwrap();
            std::fs::write(assets.join(APP_ASSET), b"tgz").unwrap();
            std::os::unix::fs::symlink(assets.join(APP_ASSET), bin.join(APP_ASSET)).unwrap();
            match app_source(None, &exe) {
                LauncherSource::Refused { why, .. } => assert!(why.contains("symlink"), "{why}"),
                other => panic!("{other:?}"),
            }
            std::fs::remove_dir_all(&root).unwrap();
        }

        /// The swap keeps the old bundle as `.previous`, replacing a stale
        /// one, and moves the new one in.
        #[test]
        fn the_swap_keeps_the_old_app_as_previous() {
            let root = scratch("swap");
            let ex = root.join(EXTRACT_NAME);
            make_bundle(&root.join(APP_NAME), "old");
            make_bundle(&root.join(PREVIOUS_NAME), "ancient");
            make_bundle(&ex.join(APP_NAME), "new");
            let apps = Dir::open_nofollow(&root).unwrap();
            let sc = apps.subdir(EXTRACT_NAME).unwrap();
            swap_in(&apps, &sc).unwrap();
            assert_eq!(marker(&root.join(APP_NAME)), "new");
            assert_eq!(marker(&root.join(PREVIOUS_NAME)), "old");
            assert!(!ex.join(APP_NAME).exists());
            // A first install: nothing to keep.
            std::fs::remove_dir_all(root.join(APP_NAME)).unwrap();
            std::fs::remove_dir_all(root.join(PREVIOUS_NAME)).unwrap();
            make_bundle(&ex.join(APP_NAME), "first");
            swap_in(&apps, &sc).unwrap();
            assert_eq!(marker(&root.join(APP_NAME)), "first");
            assert!(!root.join(PREVIOUS_NAME).exists());
            std::fs::remove_dir_all(&root).unwrap();
        }

        /// A failed move puts the old bundle back.
        #[test]
        fn a_failed_swap_puts_the_old_app_back() {
            let root = scratch("rollback");
            make_bundle(&root.join(APP_NAME), "old");
            std::fs::create_dir_all(root.join(EXTRACT_NAME)).unwrap();
            let apps = Dir::open_nofollow(&root).unwrap();
            let sc = apps.subdir(EXTRACT_NAME).unwrap();
            // Nothing unpacked: the move fails.
            let e = swap_in(&apps, &sc).unwrap_err().to_string();
            assert!(e.contains("unchanged"), "{e}");
            assert_eq!(marker(&root.join(APP_NAME)), "old");
            assert!(!root.join(PREVIOUS_NAME).exists());
            std::fs::remove_dir_all(&root).unwrap();
        }

        /// Removing a stale scratch directory never follows a symlink there.
        #[test]
        fn removing_a_stale_scratch_never_follows_a_symlink() {
            let root = scratch("stale");
            let outside = root.join("outside");
            make_bundle(&outside, "kept");
            let link = root.join(EXTRACT_NAME);
            std::os::unix::fs::symlink(&outside, &link).unwrap();
            remove_any(&link).unwrap();
            assert!(std::fs::symlink_metadata(&link).is_err());
            assert_eq!(marker(&outside), "kept");
            remove_any(&link).unwrap();
            std::fs::remove_dir_all(&root).unwrap();
        }

        #[test]
        fn the_operators_session_is_reached_through_launchctl_asuser() {
            let cmd = as_operator(501, "/usr/bin/open", &["/Applications/faber.app"]);
            assert_eq!(
                cmd.display(),
                "/bin/launchctl asuser 501 /usr/bin/sudo -u \"#501\" /usr/bin/open \
                 /Applications/faber.app"
            );
        }

        /// The archive is read again at apply and held to the plan's digest:
        /// a swap after the plan, or a symlink in its place, installs nothing.
        #[test]
        fn the_archive_must_still_have_the_plans_digest() {
            let root = scratch("digest");
            let archive = root.join(APP_ASSET);
            std::fs::write(&archive, b"the release's bytes").unwrap();
            let sha = crate::setup::fsx::sha256_of(&mut &b"the release's bytes"[..]).unwrap();
            assert_eq!(
                archive_bytes(&archive, &sha).unwrap(),
                b"the release's bytes"
            );

            std::fs::write(&archive, b"swapped after the plan").unwrap();
            let e = archive_bytes(&archive, &sha).unwrap_err().to_string();
            assert!(e.contains("changed after the plan was made"), "{e}");

            std::fs::remove_file(&archive).unwrap();
            let real = root.join("elsewhere.tar.gz");
            std::fs::write(&real, b"the release's bytes").unwrap();
            std::os::unix::fs::symlink(&real, &archive).unwrap();
            let e = format!("{:#}", archive_bytes(&archive, &sha).unwrap_err());
            assert!(e.contains("a symlink is refused"), "{e}");
            std::fs::remove_dir_all(&root).unwrap();
        }

        /// What `tar` left is the operator's real `faber.app`, or nothing is
        /// installed: missing, a symlink, or anyone else's entry is refused.
        #[test]
        fn the_unpacked_bundle_must_be_a_real_directory_all_the_operators() {
            use std::os::unix::fs::MetadataExt;
            let root = scratch("unpacked");
            let me = std::fs::metadata(&root).unwrap().uid();
            let dir = crate::setup::fsx::Dir::open_nofollow(&root).unwrap();

            let e = format!("{:#}", check_unpacked(&dir, &root, me).unwrap_err());
            assert!(e.contains("holds no faber.app"), "{e}");

            let elsewhere = root.join("elsewhere");
            make_bundle(&elsewhere, "x");
            std::os::unix::fs::symlink(&elsewhere, root.join(APP_NAME)).unwrap();
            let e = format!("{:#}", check_unpacked(&dir, &root, me).unwrap_err());
            assert!(e.contains("a symlink is refused"), "{e}");

            std::fs::remove_file(root.join(APP_NAME)).unwrap();
            make_bundle(&root.join(APP_NAME), "x");
            check_unpacked(&dir, &root, me).unwrap();
            // Every entry judged against another account reads as foreign.
            let e = check_unpacked(&dir, &root, me + 1).unwrap_err().to_string();
            assert!(e.contains("is not uid"), "{e}");
            std::fs::remove_dir_all(&root).unwrap();
        }

        /// `tar` unpacks the archive from its stdin, the bytes root checked,
        /// not from the path again.
        #[test]
        fn the_archive_is_unpacked_from_the_checked_bytes_on_stdin() {
            let root = scratch("stdin");
            let src = root.join("src");
            make_bundle(&src.join(APP_NAME), "from the bytes");
            let tgz = std::process::Command::new("/usr/bin/tar")
                .args(["czf", "-", "-C"])
                .arg(&src)
                .arg(APP_NAME)
                .output()
                .unwrap();
            assert!(tgz.status.success());
            let into = root.join("into");
            std::fs::create_dir(&into).unwrap();
            // The path names a file that is not there: only the bytes are read.
            let gone = root.join("no-such.tar.gz");
            unpack(&tgz.stdout, &gone, &into, None, &root).unwrap();
            assert_eq!(marker(&into.join(APP_NAME)), "from the bytes");

            let e = format!(
                "{:#}",
                unpack(b"not gzip", &gone, &into, None, &root).unwrap_err()
            );
            assert!(e.contains("could not unpack"), "{e}");
            std::fs::remove_dir_all(&root).unwrap();
        }

        /// A replaced bundle that was the operator's all the way down stays
        /// as `.previous` for the app's own housekeeping; anyone else's, or
        /// one with a foreign entry, is removed.
        #[test]
        fn only_the_operators_own_replaced_bundle_is_kept_as_previous() {
            let root = scratch("previous");
            let previous = root.join(PREVIOUS_NAME);
            let theirs = InstalledApp::Bundle {
                owner: 501,
                foreign: None,
                version: Ok("0.4.0".into()),
                build: None,
            };
            make_bundle(&previous, "old");
            drop_foreign_previous(&root, &theirs, 501).unwrap();
            assert_eq!(marker(&previous), "old");

            for replaced in [
                InstalledApp::Bundle {
                    owner: 0,
                    foreign: None,
                    version: Ok("0.4.0".into()),
                    build: None,
                },
                InstalledApp::Bundle {
                    owner: 501,
                    foreign: Some("/Applications/faber.app/x".into()),
                    version: Ok("0.4.0".into()),
                    build: None,
                },
            ] {
                make_bundle(&previous, "old");
                drop_foreign_previous(&root, &replaced, 501).unwrap();
                assert!(
                    std::fs::symlink_metadata(&previous).is_err(),
                    "{replaced:?}"
                );
            }
            // Missing: nothing was replaced, and nothing there is fine.
            drop_foreign_previous(&root, &InstalledApp::Missing, 501).unwrap();
            std::fs::remove_dir_all(&root).unwrap();
        }

        /// The app is opened again after a failed install only if setup quit
        /// it and a bundle is back in place.
        #[test]
        fn a_failed_install_reopens_only_an_app_it_quit() {
            let root = scratch("reopen");
            assert!(!reopen_after_failure(true, &root));
            make_bundle(&root.join(APP_NAME), "old");
            assert!(reopen_after_failure(true, &root));
            assert!(!reopen_after_failure(false, &root));
            std::fs::remove_dir_all(&root).unwrap();
        }

        /// An archive larger than apply would take is refused by the plan.
        #[test]
        fn an_oversized_archive_is_refused_at_plan_time() {
            let root = scratch("cap");
            let archive = root.join(APP_ASSET);
            let f = std::fs::File::create(&archive).unwrap();
            let found = |path: &Path| LauncherSource::Found {
                path: path.to_path_buf(),
                sha256: String::new(),
                chain: vec![],
            };
            f.set_len(MAX_ARCHIVE_BYTES).unwrap();
            assert!(matches!(
                within_cap(found(&archive)),
                LauncherSource::Found { .. }
            ));
            f.set_len(MAX_ARCHIVE_BYTES + 1).unwrap();
            assert!(matches!(
                within_cap(found(&archive)),
                LauncherSource::Refused { why, .. } if why.contains("larger than 200 MiB")
            ));
            assert_eq!(within_cap(LauncherSource::None), LauncherSource::None);
            std::fs::remove_dir_all(&root).unwrap();
        }

        #[test]
        fn the_app_is_quit_by_a_signal_to_the_operators_process_only() {
            assert_eq!(
                quit_cmd(501).display(),
                "/usr/bin/pkill -TERM -u 501 -x faber-menubar"
            );
        }
    }
}
