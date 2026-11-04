//! `sudo faber upgrade-cli` (ADR-0034, #81): the root-owned CLI copy follows
//! the daemon, when the operator says so.
//!
//! A SYSTEM host keeps two copies of faber (ADR-0025). The daemon's,
//! [`SystemLayout::daemon_bin`], is `_faber`'s, and its own updater replaces
//! it (ADR-0006, ADR-0031). The CLI copy that `sudo faber` runs,
//! [`SystemLayout::cli_bin`], is root's, so root never executes a file `_faber`
//! can write, and nothing updates it. This command does, from inside a `sudo`
//! the operator typed. No standing root process is added.
//!
//! # The trust argument
//!
//! Root treats every byte `_faber` wrote as data, and checks it only after it
//! has its own copy:
//!
//! 1. The daemon binary is copied **first** into a root-owned 0600 file beside
//!    the CLI copy, opened without following a symlink. From then on `_faber`
//!    can change nothing root looks at.
//! 2. The copy is read back, and every later check runs on those bytes.
//! 3. Its version marker ([`marker`]) picks which release to fetch. It is only
//!    a hint: everything after it is checked against signed data.
//! 4. That release's `SHA256SUMS` and its Sigstore bundle are fetched
//!    anonymously and verified with the **running root copy's own** policy,
//!    through the daemon updater's seam ([`update::verify_provenance_detail`]).
//! 5. The copy's digest must be in `SHA256SUMS`, and the version binding must
//!    hold: the marker's version equals the tag, and its `+g<sha>` prefixes the
//!    tag's commit. Root never runs the copy to ask its `--version`: the marker
//!    and clap's `--version` are both `FABER_VERSION`, stamped by `build.rs`,
//!    and the marker is read from bytes whose digest has just been verified.
//! 6. It must be strictly newer than this binary and than the installed copy.
//! 7. After a y/N, the very file that was checked is given mode 0755 and
//!    renamed over the CLI copy, so the result is byte-identical to it.
//!
//! There is no maturity wait: the operator asked for this release.
//!
//! # Inside `sudo faber setup`
//!
//! Setup runs this as its first step ([`before_setup`]). A host with no daemon
//! copy yet, or not set up at all, has nothing to promote; an up-to-date or
//! declined copy is left alone; any other failure is a warning, and setup goes
//! on: a failed promotion must not block the command the operator needed. When
//! a copy is promoted, setup re-executes the new CLI copy with the same
//! arguments and `--skip-cli-upgrade`. Without that, the old binary would go on
//! to install *itself* over the CLI copy and undo the promotion.
//!
//! # The wall's checker follows
//!
//! On macOS the wall's anchor checker is the CLI copy too (`faber
//! wall-daemon`, ADR-0026). After a promotion it is restarted on the new copy
//! ([`restart_wall_checker`]), if its LaunchDaemon is installed. A failure
//! there is a warning: the copy is installed, and the old checker keeps
//! enforcing the same stored rules until it is restarted.
//!
//! # The gap is made loud
//!
//! Every other CLI command asks the local daemon for its version and prints
//! [`skew_line`] when it is newer ([`skew_warning`]). The daemon reports the
//! same line in `/api/state` ([`cli_copy_view`]), reading the CLI copy as data.

use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use faber_release::{
    check_reported_version, is_newer, parse_sha256sums, parse_version, verify_digest, Release,
    Verdict,
};
use serde::Serialize;

use crate::github::Github;
use crate::platform::layout::{Layout, Os, SystemLayout, ETC_DIR};
use crate::setup::fsx::{self, Dir, ROOT};
use crate::setup::marker::{self, version_chars};
use crate::update::{self, DAEMON_ASSET_NAME, SUMS_ASSET_NAME, SUMS_BUNDLE_ASSET_NAME};

/// The root-owned file the daemon binary is copied into, beside the CLI copy:
/// in the same directory, so the final rename is atomic.
const STAGE: &str = ".faber-upgrade-stage";

/// What `upgrade-cli` did, when it did not fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The CLI copy is now the daemon's verified binary, at `version`.
    Promoted { version: String },
    /// The daemon copy is not newer: nothing to do.
    UpToDate,
    /// The operator said no: nothing changed.
    Declined,
}

/// Why there was nothing to promote. Errors of their own, so setup's first
/// step can tell them from a failed check: they are the ordinary state of a
/// host, not something to warn about.
#[derive(Debug)]
pub enum Skip {
    /// The host is not set up (no trusted setup record): there is no daemon
    /// copy to follow.
    NotSystem,
    /// No daemon copy at that path: setup installs one itself.
    NoDaemonCopy(PathBuf),
    /// The daemon copy is older than this binary or the installed copy.
    Older(String),
}

impl std::fmt::Display for Skip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Skip::NotSystem => write!(
                f,
                "this host is not set up for faber's own accounts (no trusted /etc/faber/setup.json), \
                 so there is no daemon copy to follow; run `sudo faber setup`"
            ),
            Skip::NoDaemonCopy(path) => write!(f, "there is no daemon copy at {}", path.display()),
            Skip::Older(why) => write!(f, "{why}; nothing was changed"),
        }
    }
}

impl std::error::Error for Skip {}

// --- the seams ---------------------------------------------------------------

/// The files `upgrade-cli` touches. Nothing reaches the CLI copy except
/// [`Files::promote`].
pub(crate) trait Files {
    /// Copy the daemon binary into the root-owned stage file and return the
    /// copy's bytes, read back from that file. On an error nothing is left
    /// staged.
    fn stage(&mut self) -> Result<Vec<u8>>;
    /// The version marker of the installed CLI copy, if it is readable.
    fn installed_version(&mut self) -> Option<String>;
    /// Install the staged file over the CLI copy, root-owned, mode 0755.
    fn promote(&mut self) -> Result<()>;
    /// Remove the stage file, if any.
    fn discard(&mut self);
    /// The CLI copy's path, for messages.
    fn cli_path(&self) -> &Path;
}

/// Where releases come from, and the provenance check over them.
pub(crate) trait ReleaseSource {
    /// The release published under `tag`, `None` when there is none.
    async fn release(&self, tag: &str) -> Result<Option<Release>>;
    /// One asset's bytes.
    async fn asset(&self, id: u64) -> Result<Vec<u8>>;
    /// ADR-0031's check of `SHA256SUMS` under the compiled-in policy, and the
    /// commit `tag` resolved to.
    async fn verify(
        &self,
        tag: &str,
        sums: &[u8],
        bundle: Option<&[u8]>,
    ) -> (Verdict, Option<String>);
}

/// The y/N question.
pub(crate) trait Prompt {
    fn confirm(&mut self, question: &str) -> Result<bool>;
}

// --- the flow ----------------------------------------------------------------

/// The flow above, over the seams. `own` is the running binary's version.
pub(crate) async fn upgrade<F: Files, R: ReleaseSource, P: Prompt>(
    files: &mut F,
    source: &R,
    prompt: &mut P,
    own: &str,
    yes: bool,
) -> Result<Outcome> {
    let bytes = files.stage()?;
    let outcome = async {
        let installed = files.installed_version();
        let Some(found) = verify_copy(&bytes, installed.as_deref(), source, own).await? else {
            return Ok(Outcome::UpToDate);
        };
        let cli = files.cli_path().display().to_string();
        println!(
            "daemon copy: faber {} (published {}), verified against its signed release",
            found.version, found.published_at
        );
        if !yes && !prompt.confirm(&format!("Install v{} over {cli}?", found.version))? {
            return Ok(Outcome::Declined);
        }
        files.promote()?;
        Ok(Outcome::Promoted {
            version: found.version,
        })
    }
    .await;
    if !matches!(outcome, Ok(Outcome::Promoted { .. })) {
        files.discard();
    }
    outcome
}

/// A daemon copy that passed every check.
struct Verified {
    /// `X.Y.Z`, the tag without its `v`.
    version: String,
    /// GitHub's `published_at`, sanitized for the terminal.
    published_at: String,
}

/// Every check on the copy's `bytes`. `Ok(None)` when it is not newer.
async fn verify_copy<R: ReleaseSource>(
    bytes: &[u8],
    installed: Option<&str>,
    source: &R,
    own: &str,
) -> Result<Option<Verified>> {
    let mut scanner = marker::Scanner::new();
    scanner.feed(bytes);
    let hint = scanner.finish().ok_or_else(|| {
        anyhow!(
            "the daemon copy carries no version marker, so there is no release to check it against"
        )
    })?;
    let (major, minor, patch) = parse_version(&hint)
        .ok_or_else(|| anyhow!("the daemon copy's version {hint} is not X.Y.Z"))?;
    let tag = format!("v{major}.{minor}.{patch}");
    let version = format!("{major}.{minor}.{patch}");

    // Decided before anything is fetched, on the unverified hint: every
    // answer here changes nothing, so a hint that lies can only make root do
    // less. A host that is up to date is never asked to reach GitHub, which is
    // what keeps setup's first step quiet offline. Anything this lets through
    // is bound to the tag below, so the decision stands for the verified bytes.
    if up_to_date(&hint, own, installed)? {
        return Ok(None);
    }

    let repo = crate::config::RELEASE_REPO;
    let release = source
        .release(&tag)
        .await
        .with_context(|| format!("fetching release {tag} of {repo}"))?
        .ok_or_else(|| {
            anyhow!("{repo} has no release {tag}, which the daemon copy claims to be")
        })?;
    if release.draft || release.prerelease {
        bail!("release {tag} is a draft or a pre-release, which upgrade-cli never installs");
    }
    let asset = |name: &str| {
        update::find_asset(&release, name)
            .map(|a| a.id)
            .ok_or_else(|| anyhow!("release {tag} has no {name} asset"))
    };
    let sums = source
        .asset(asset(SUMS_ASSET_NAME)?)
        .await
        .with_context(|| format!("downloading {SUMS_ASSET_NAME} of {tag}"))?;
    let bundle = match update::find_asset(&release, SUMS_BUNDLE_ASSET_NAME) {
        Some(a) => Some(
            source
                .asset(a.id)
                .await
                .with_context(|| format!("downloading {SUMS_BUNDLE_ASSET_NAME} of {tag}"))?,
        ),
        None => None,
    };

    let (verdict, commit) = source.verify(&tag, &sums, bundle.as_deref()).await;
    let commit = match verdict {
        Verdict::Verified => commit.ok_or_else(|| {
            anyhow!("internal: {tag} verified without a resolved commit; nothing was changed")
        })?,
        Verdict::Refused(why) => {
            bail!("release {tag} failed verification: {why}; nothing was changed")
        }
        Verdict::Unavailable(why) => bail!(
            "release {tag} could not be verified now: {why}; nothing was changed, retry later"
        ),
    };

    let sums = parse_sha256sums(&String::from_utf8_lossy(&sums));
    verify_digest(&sums, DAEMON_ASSET_NAME, bytes).map_err(|e| {
        anyhow!("the daemon copy is not the {tag} release: {e}; nothing was changed")
    })?;
    check_reported_version(&format!("faber {hint}"), &tag, &commit).map_err(|e| {
        anyhow!("the daemon copy is not the {tag} release: {e}; nothing was changed")
    })?;

    Ok(Some(Verified {
        version,
        published_at: release
            .published_at
            .as_deref()
            .map(|p| crate::term::sanitize_bounded(p, 64).into_owned())
            .unwrap_or_else(|| "at an unknown time".to_string()),
    }))
}

/// `false` when `candidate` is newer than both this binary (`own`) and the
/// installed copy, `true` (up to date) when it equals the newer of them, and
/// [`Skip::Older`] when it is older.
fn up_to_date(candidate: &str, own: &str, installed: Option<&str>) -> Result<bool> {
    let shown = |v: &str| version_chars(core(v));
    for (what, current) in [
        ("this faber", Some(own)),
        ("the installed CLI copy", installed),
    ] {
        let Some(current) = current else { continue };
        if is_newer(candidate, current) {
            continue;
        }
        if parse_version(candidate).is_some() && parse_version(candidate) == parse_version(current)
        {
            return Ok(true);
        }
        return Err(Skip::Older(format!(
            "the daemon copy is faber {}, older than {what} ({})",
            shown(candidate),
            shown(current)
        ))
        .into());
    }
    Ok(false)
}

/// `0.4.1` of `0.4.1+gabc1234`.
fn core(version: &str) -> &str {
    version.split('+').next().unwrap_or(version)
}

// --- the command -------------------------------------------------------------

/// `sudo faber upgrade-cli`: the preconditions, then [`upgrade`] over the real
/// filesystem, GitHub and terminal.
pub async fn run(yes: bool) -> Result<Outcome> {
    // The marker is how a later run reads this binary's version.
    marker::keep();
    if crate::platform::record::current_euid() != 0 {
        bail!("run with sudo: sudo faber upgrade-cli");
    }
    if crate::platform::layout() != Layout::System {
        return Err(Skip::NotSystem.into());
    }
    // The same refusal setup and uninstall make (ADR-0025): root never runs a
    // faber that `_faber` or `_faber_vm` could have replaced.
    crate::setup::exe::trust_running_binary()?;

    let layout = SystemLayout::for_os(Os::HOST);
    let tuf = TufCache::create()?;
    let source = GithubSource {
        gh: Github::anonymous()?,
        tuf_cache: tuf.0.clone(),
    };
    let mut files = RootFiles::new(&layout);
    let outcome = upgrade(
        &mut files,
        &source,
        &mut Terminal,
        env!("FABER_VERSION"),
        yes,
    )
    .await?;
    if matches!(outcome, Outcome::Promoted { .. }) {
        let installed = Path::new(crate::platform::service::WALL_PLIST).exists();
        if let Some(warning) =
            restart_wall_checker(&mut crate::setup::tools::SystemRunner, installed)
        {
            eprintln!("faber: warning: {}", crate::term::sanitize(&warning));
        }
    }
    Ok(outcome)
}

/// Restart the wall's anchor checker on the CLI copy just promoted, when its
/// LaunchDaemon is `installed` (macOS only). Returns a warning instead of
/// failing: the promotion stands, and the warning says how to start the
/// checker by hand.
pub(crate) fn restart_wall_checker(
    runner: &mut dyn crate::setup::tools::Runner,
    installed: bool,
) -> Option<String> {
    use crate::platform::service::{launchd, system_target_of, WALL_LABEL, WALL_PLIST};
    if !installed || Os::HOST != Os::MacOs {
        return None;
    }
    let target = system_target_of(WALL_LABEL);
    // Booted out and in again, never `kickstart -k`: launchd would keep the
    // job pinned to the copy just replaced, and it would never spawn (#479).
    let restarted = launchd::reload(
        runner,
        "system",
        &target,
        Path::new(WALL_PLIST),
        Duration::from_secs(30),
        Duration::from_secs(1),
    )
    .and_then(|()| {
        launchd::wait_running(
            runner,
            &target,
            Duration::from_secs(15),
            Duration::from_millis(500),
        )
    });
    match restarted {
        Ok(()) => {
            println!("restarted {target} on the new CLI copy");
            None
        }
        Err(e) => Some(format!(
            "the wall's checker was not restarted on the new CLI copy ({e:#}); while it is \
             not running it writes no verdict. Restart it with `sudo launchctl bootout \
             {target}`, then `sudo launchctl bootstrap system {WALL_PLIST}`"
        )),
    }
}

/// What `upgrade-cli` prints once it is done.
pub fn report(outcome: &Outcome) -> String {
    let cli = SystemLayout::for_os(Os::HOST).cli_bin;
    match outcome {
        Outcome::Promoted { version } => format!("installed v{version} at {}", cli.display()),
        Outcome::UpToDate => format!("{} is already up to date", cli.display()),
        Outcome::Declined => "nothing changed".to_string(),
    }
}

/// `sudo faber setup`'s first step (see the module docs). Returns when setup
/// should go on with this binary; on a promotion it re-executes the new CLI
/// copy instead, and returns only if that fails.
pub async fn before_setup(yes: bool) {
    match run(yes).await {
        Ok(Outcome::Promoted { version }) => {
            let cli = SystemLayout::for_os(Os::HOST).cli_bin;
            println!(
                "installed v{version} at {}; continuing setup with it",
                cli.display()
            );
            let err = reexec(&cli);
            eprintln!(
                "faber: could not run {} ({err}); continuing setup with this binary",
                cli.display()
            );
        }
        Ok(Outcome::UpToDate) | Ok(Outcome::Declined) => {}
        Err(e) => match e.downcast_ref::<Skip>() {
            Some(Skip::NotSystem | Skip::NoDaemonCopy(_)) => {}
            Some(older @ Skip::Older(_)) => println!("CLI copy: {older}"),
            None => eprintln!(
                "faber: CLI copy not upgraded: {}; continuing",
                crate::term::sanitize(&format!("{e:#}"))
            ),
        },
    }
}

/// Replace this process with `cli`, run with this process's arguments and
/// `--skip-cli-upgrade`. Returns only on failure.
fn reexec(cli: &Path) -> std::io::Error {
    use std::os::unix::process::CommandExt;
    std::process::Command::new(cli)
        .args(std::env::args_os().skip(1))
        .arg("--skip-cli-upgrade")
        .exec()
}

// --- the real seams ----------------------------------------------------------

/// [`Files`] on the real filesystem, through [`fsx`]'s descriptor-relative
/// calls.
struct RootFiles {
    daemon_bin: PathBuf,
    cli_bin: PathBuf,
    /// The CLI copy's directory, once staged into.
    dir: Option<Dir>,
}

impl RootFiles {
    fn new(layout: &SystemLayout) -> Self {
        Self {
            daemon_bin: layout.daemon_bin.clone(),
            cli_bin: layout.cli_bin.clone(),
            dir: None,
        }
    }

    /// The daemon binary, opened read-only without following a symlink in its
    /// name or its directory's (`_faber` owns both), and only as a regular file.
    fn open_daemon(&self) -> Result<(std::fs::File, u64)> {
        let path = &self.daemon_bin;
        let (parent, name) = fsx::split(path)?;
        let opened = fsx::open_leaf_dir(parent).and_then(|dir| dir.open_file(name));
        let file = match opened {
            Ok(file) => file,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Skip::NoDaemonCopy(path.clone()).into())
            }
            Err(e) => {
                let link =
                    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink());
                if link {
                    bail!(
                        "the daemon copy {} is a symlink, not a binary",
                        path.display()
                    );
                }
                bail!("could not open the daemon copy {}: {e}", path.display());
            }
        };
        let meta = file.metadata()?;
        if !meta.file_type().is_file() {
            bail!("the daemon copy {} is not a regular file", path.display());
        }
        if meta.len() > fsx::MAX_BINARY {
            bail!(
                "the daemon copy {} is {} bytes, more than a faber binary could be",
                path.display(),
                meta.len()
            );
        }
        Ok((file, meta.len()))
    }

    /// The stage file's bytes, read back through a fresh descriptor.
    fn read_stage(dir: &Dir) -> Result<Vec<u8>> {
        let file = dir.open_file(STAGE)?;
        let meta = file.metadata()?;
        if !meta.file_type().is_file() || meta.uid() != 0 {
            bail!(
                "{} is not root's regular file",
                dir.path().join(STAGE).display()
            );
        }
        let mut bytes = Vec::new();
        file.take(fsx::MAX_BINARY).read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}

impl Files for RootFiles {
    fn stage(&mut self) -> Result<Vec<u8>> {
        let (src, _) = self.open_daemon()?;
        let (parent, _) = fsx::split(&self.cli_bin)?;
        // Every directory down to it walked without following a symlink, each
        // root-owned and writable by nobody else: as setup's `install_cli`.
        let dir = fsx::root_dir_chain(parent)
            .with_context(|| format!("preparing {} for a root-owned binary", parent.display()))?;
        dir.write_atomic(STAGE, &mut src.take(fsx::MAX_BINARY), ROOT, 0o600)
            .with_context(|| {
                format!(
                    "copying the daemon copy {} into {}",
                    self.daemon_bin.display(),
                    dir.path().display()
                )
            })?;
        let read = Self::read_stage(&dir);
        if read.is_err() {
            let _ = dir.unlink(STAGE);
        }
        self.dir = Some(dir);
        read
    }

    fn installed_version(&mut self) -> Option<String> {
        fsx::file_digest(&self.cli_bin).ok().flatten()?.version
    }

    fn promote(&mut self) -> Result<()> {
        let dir = self
            .dir
            .as_ref()
            .ok_or_else(|| anyhow!("internal: nothing staged"))?;
        let (_, name) = fsx::split(&self.cli_bin)?;
        dir.promote(STAGE, name, ROOT, 0o755)
            .with_context(|| format!("installing over {}", self.cli_bin.display()))
    }

    fn discard(&mut self) {
        if let Some(dir) = &self.dir {
            let _ = dir.unlink(STAGE);
        }
    }

    fn cli_path(&self) -> &Path {
        &self.cli_bin
    }
}

/// A fresh, root-private TUF cache for one run, under root's [`ETC_DIR`] and
/// never under `_faber`'s home, removed when the run ends. The trusted root is
/// re-verified on every refresh anyway; a fresh directory means `upgrade-cli`
/// leaves nothing in `/etc/faber` for uninstall to trip over.
struct TufCache(PathBuf);

impl TufCache {
    fn create() -> Result<Self> {
        // Opened through its parent, as setup creates it: on macOS `/etc` is
        // itself a symlink (to `/private/etc`), which a no-follow walk refuses.
        let etc =
            fsx::open_leaf_dir(Path::new(ETC_DIR)).with_context(|| format!("opening {ETC_DIR}"))?;
        let meta = etc.metadata()?;
        if let Some(problem) =
            fsx::root_dir_problem(etc.path(), meta.is_dir(), meta.uid(), meta.mode())
        {
            bail!("{problem}, so it cannot hold root's TUF cache");
        }
        let name = format!(".upgrade-cli-tuf-{}", std::process::id());
        let path = etc.path().join(&name);
        let _ = std::fs::remove_dir_all(&path);
        fsx::ensure_dir(&etc, &name, ROOT, 0o700, &[0])
            .with_context(|| format!("creating the TUF cache {}", path.display()))?;
        Ok(Self(path))
    }
}

impl Drop for TufCache {
    fn drop(&mut self) {
        // `remove_dir_all` never follows a symlink inside the tree.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// [`ReleaseSource`] on GitHub, read anonymously.
struct GithubSource {
    gh: Github,
    tuf_cache: PathBuf,
}

impl ReleaseSource for GithubSource {
    async fn release(&self, tag: &str) -> Result<Option<Release>> {
        Ok(self
            .gh
            .release_by_tag(crate::config::RELEASE_REPO, tag)
            .await?)
    }

    async fn asset(&self, id: u64) -> Result<Vec<u8>> {
        Ok(self
            .gh
            .download_release_asset(crate::config::RELEASE_REPO, id)
            .await?)
    }

    async fn verify(
        &self,
        tag: &str,
        sums: &[u8],
        bundle: Option<&[u8]>,
    ) -> (Verdict, Option<String>) {
        let (verdict, commit, _) = update::verify_provenance_detail(
            &self.gh,
            crate::config::RELEASE_REPO,
            &self.tuf_cache,
            sums,
            bundle,
            tag,
        )
        .await;
        (verdict, commit)
    }
}

/// [`Prompt`] on the terminal, as setup asks.
struct Terminal;

impl Prompt for Terminal {
    fn confirm(&mut self, question: &str) -> Result<bool> {
        crate::setup::confirm(question)
    }
}

// --- the skew line -------------------------------------------------------------

/// The line a CLI older than its daemon prints, or `None`. Both versions are
/// reduced to [`version_chars`] first: the daemon's comes from `_faber`'s
/// process, and the CLI copy's from a marker `_faber` could have read.
pub fn skew_line(cli: &str, daemon: &str) -> Option<String> {
    let (cli, daemon) = (version_chars(cli), version_chars(daemon));
    let (cli, daemon) = (core(&cli), core(&daemon));
    is_newer(daemon, cli)
        .then(|| format!("CLI {cli} < daemon {daemon}: run `sudo faber upgrade-cli`"))
}

/// The daemon's own version in a `/api/state` reply: `update.current`.
pub fn daemon_version_from_state(state: &serde_json::Value) -> Option<String> {
    state
        .get("update")?
        .get("current")?
        .as_str()
        .map(str::to_string)
}

/// How long the skew check may take in all, connect included.
const SKEW_TIMEOUT: Duration = Duration::from_millis(250);

/// [`skew_line`] for this binary against the running daemon, on a SYSTEM host
/// only. Never blocks for more than [`SKEW_TIMEOUT`], and every failure is
/// `None`: a missing line must never cost the command it rides on.
pub async fn skew_warning() -> Option<String> {
    if crate::platform::layout() != Layout::System {
        return None;
    }
    // Over the control socket (ADR-0028): the loopback port answers only a
    // dashboard session. Only SYSTEM gets here, and no daemon from before the
    // socket can exist there, so there is no loopback fallback.
    let ask = async {
        let socket = crate::platform::control_socket()?;
        let (status, body) = crate::control::request(
            &socket,
            crate::control::Fallback::Never,
            reqwest::Method::GET,
            "/api/state",
            None,
        )
        .await
        .ok()?;
        if !status.is_success() {
            return None;
        }
        serde_json::from_str::<serde_json::Value>(&body).ok()
    };
    let state = tokio::time::timeout(SKEW_TIMEOUT, ask).await.ok()??;
    skew_line(env!("FABER_VERSION"), &daemon_version_from_state(&state)?)
}

// --- the daemon's view of the CLI copy ---------------------------------------

/// The CLI copy as the daemon sees it, for `/api/state`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CliCopyView {
    /// The version in the CLI copy's marker, sanitized.
    pub version: String,
    /// Whether it is strictly older than this daemon.
    pub behind: bool,
    /// The [`skew_line`] a CLI that old prints, when it is behind.
    pub line: Option<String>,
}

/// [`CliCopyView`] for a CLI copy at `cli` and a daemon at `daemon`.
pub fn cli_copy_view_for(cli: &str, daemon: &str) -> CliCopyView {
    let line = skew_line(cli, daemon);
    CliCopyView {
        version: version_chars(cli),
        behind: line.is_some(),
        line,
    }
}

/// The CLI copy's version, keyed by its `(mtime, len)` so the daemon reads the
/// tens of megabytes again only when the file changed.
type CachedVersion = Option<((i64, i64, u64), Option<String>)>;
static CLI_VERSION: Mutex<CachedVersion> = Mutex::new(None);

/// [`CliCopyView`] for this daemon, `None` under LEGACY or when the CLI copy
/// is missing, unreadable or carries no marker. The copy is read as data,
/// never executed.
pub async fn cli_copy_view() -> Option<CliCopyView> {
    if crate::platform::layout() != Layout::System {
        return None;
    }
    let cli_bin = SystemLayout::for_os(Os::HOST).cli_bin;
    let meta = std::fs::metadata(&cli_bin).ok()?;
    let key = (meta.mtime(), meta.mtime_nsec(), meta.len());
    let cached = CLI_VERSION
        .lock()
        .unwrap()
        .as_ref()
        .filter(|(k, _)| *k == key)
        .map(|(_, v)| v.clone());
    let version = match cached {
        Some(version) => version,
        None => {
            let version = tokio::task::spawn_blocking(move || {
                fsx::file_digest(&cli_bin).ok().flatten()?.version
            })
            .await
            .ok()?;
            *CLI_VERSION.lock().unwrap() = Some((key, version.clone()));
            version
        }
    };
    Some(cli_copy_view_for(&version?, env!("FABER_VERSION")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    use faber_release::{sha256_hex, ReleaseAsset};

    const COMMIT: &str = "abc1234def5678abc1234def5678abc1234def56";
    const OWN: &str = "0.4.0+g1111111";

    /// A daemon binary carrying `version` in its marker. Built from
    /// [`marker::needle`] at run time, as `marker`'s own tests do.
    fn daemon_bytes(version: &str) -> Vec<u8> {
        let mut bytes = vec![0x7f, b'E', b'L', b'F', 1, 2, 3];
        bytes.extend(marker::needle());
        bytes.extend_from_slice(version.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&[9; 64]);
        bytes
    }

    #[derive(Default)]
    struct FakeFiles {
        /// The daemon copy; `None` is a host without one.
        daemon: Option<Vec<u8>>,
        installed: Option<String>,
        staged: Option<Vec<u8>>,
        /// What was installed over the CLI copy, each time.
        promoted: Vec<Vec<u8>>,
        discarded: usize,
    }

    impl Files for FakeFiles {
        fn stage(&mut self) -> Result<Vec<u8>> {
            let bytes = self
                .daemon
                .clone()
                .ok_or_else(|| Skip::NoDaemonCopy(PathBuf::from("/srv/faber/bin/faber")))?;
            self.staged = Some(bytes.clone());
            Ok(bytes)
        }
        fn installed_version(&mut self) -> Option<String> {
            self.installed.clone()
        }
        fn promote(&mut self) -> Result<()> {
            let staged = self.staged.take().expect("promote after stage");
            self.promoted.push(staged);
            Ok(())
        }
        fn discard(&mut self) {
            self.staged = None;
            self.discarded += 1;
        }
        fn cli_path(&self) -> &Path {
            Path::new("/usr/local/bin/faber")
        }
    }

    struct FakeSource {
        release: Option<Release>,
        sums: Vec<u8>,
        verdict: Verdict,
        commit: String,
        calls: Cell<usize>,
        verified: RefCell<Vec<String>>,
    }

    impl FakeSource {
        /// A published release `tag` whose `SHA256SUMS` lists `bytes`.
        fn publishing(tag: &str, bytes: &[u8]) -> Self {
            Self {
                release: Some(Release {
                    tag_name: tag.to_string(),
                    draft: false,
                    prerelease: false,
                    published_at: Some("2026-10-20T10:00:00Z".to_string()),
                    assets: vec![
                        ReleaseAsset {
                            id: 1,
                            name: SUMS_ASSET_NAME.to_string(),
                            size: 1,
                        },
                        ReleaseAsset {
                            id: 2,
                            name: SUMS_BUNDLE_ASSET_NAME.to_string(),
                            size: 1,
                        },
                    ],
                }),
                sums: format!("{}  {DAEMON_ASSET_NAME}\n", sha256_hex(bytes)).into_bytes(),
                verdict: Verdict::Verified,
                commit: COMMIT.to_string(),
                calls: Cell::new(0),
                verified: RefCell::new(Vec::new()),
            }
        }
    }

    impl ReleaseSource for FakeSource {
        async fn release(&self, _tag: &str) -> Result<Option<Release>> {
            self.calls.set(self.calls.get() + 1);
            Ok(self.release.clone())
        }
        async fn asset(&self, id: u64) -> Result<Vec<u8>> {
            self.calls.set(self.calls.get() + 1);
            Ok(match id {
                1 => self.sums.clone(),
                _ => b"{}".to_vec(),
            })
        }
        async fn verify(
            &self,
            tag: &str,
            _sums: &[u8],
            _bundle: Option<&[u8]>,
        ) -> (Verdict, Option<String>) {
            self.calls.set(self.calls.get() + 1);
            self.verified.borrow_mut().push(tag.to_string());
            (self.verdict.clone(), Some(self.commit.clone()))
        }
    }

    struct FakePrompt {
        answer: bool,
        asked: usize,
    }

    impl Prompt for FakePrompt {
        fn confirm(&mut self, _question: &str) -> Result<bool> {
            self.asked += 1;
            Ok(self.answer)
        }
    }

    fn yes_prompt() -> FakePrompt {
        FakePrompt {
            answer: true,
            asked: 0,
        }
    }

    fn files_with(daemon: Vec<u8>) -> FakeFiles {
        FakeFiles {
            daemon: Some(daemon),
            installed: Some(OWN.to_string()),
            ..Default::default()
        }
    }

    async fn go(
        files: &mut FakeFiles,
        source: &FakeSource,
        prompt: &mut FakePrompt,
    ) -> Result<Outcome> {
        upgrade(files, source, prompt, OWN, false).await
    }

    /// Nothing reached the CLI copy, and the stage is gone.
    fn untouched(files: &FakeFiles) {
        assert!(files.promoted.is_empty(), "the CLI copy was replaced");
        assert!(files.staged.is_none(), "the stage was left behind");
    }

    #[tokio::test]
    async fn a_verified_newer_copy_is_promoted_byte_for_byte() {
        let bytes = daemon_bytes("0.4.1+gabc1234");
        let mut files = files_with(bytes.clone());
        let source = FakeSource::publishing("v0.4.1", &bytes);
        let mut prompt = yes_prompt();
        let outcome = go(&mut files, &source, &mut prompt).await.unwrap();
        assert_eq!(
            outcome,
            Outcome::Promoted {
                version: "0.4.1".to_string()
            }
        );
        assert_eq!(files.promoted, vec![bytes]);
        assert_eq!(prompt.asked, 1);
        assert_eq!(*source.verified.borrow(), vec!["v0.4.1".to_string()]);
    }

    #[tokio::test]
    async fn yes_skips_the_prompt() {
        let bytes = daemon_bytes("0.4.1+gabc1234");
        let mut files = files_with(bytes.clone());
        let source = FakeSource::publishing("v0.4.1", &bytes);
        let mut prompt = yes_prompt();
        let outcome = upgrade(&mut files, &source, &mut prompt, OWN, true)
            .await
            .unwrap();
        assert!(matches!(outcome, Outcome::Promoted { .. }));
        assert_eq!(prompt.asked, 0);
        assert_eq!(files.promoted.len(), 1);
    }

    #[tokio::test]
    async fn a_declined_prompt_changes_nothing() {
        let bytes = daemon_bytes("0.4.1+gabc1234");
        let mut files = files_with(bytes.clone());
        let source = FakeSource::publishing("v0.4.1", &bytes);
        let mut prompt = FakePrompt {
            answer: false,
            asked: 0,
        };
        let outcome = go(&mut files, &source, &mut prompt).await.unwrap();
        assert_eq!(outcome, Outcome::Declined);
        untouched(&files);
        assert_eq!(files.discarded, 1);
    }

    /// A copy `_faber` tampered with is not in the signed `SHA256SUMS`.
    #[tokio::test]
    async fn a_tampered_copy_is_refused() {
        let genuine = daemon_bytes("0.4.1+gabc1234");
        let mut tampered = genuine.clone();
        tampered.push(0xcc);
        let mut files = files_with(tampered);
        let source = FakeSource::publishing("v0.4.1", &genuine);
        let err = go(&mut files, &source, &mut yes_prompt())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("mismatch"), "{err}");
        assert!(err.contains("nothing was changed"), "{err}");
        untouched(&files);
    }

    /// The marker claims a release whose tag points at another commit: the
    /// version binding refuses it, though the digest matched.
    #[tokio::test]
    async fn a_copy_built_from_another_commit_is_refused() {
        let bytes = daemon_bytes("0.4.1+gabc1234");
        let mut files = files_with(bytes.clone());
        let mut source = FakeSource::publishing("v0.4.1", &bytes);
        source.commit = "fff1234def5678abc1234def5678abc1234def56".to_string();
        let err = go(&mut files, &source, &mut yes_prompt())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("version binding"), "{err}");
        untouched(&files);
    }

    /// A marker that names a different release than the one whose sums list
    /// the copy: the tag the marker picks does not vouch for these bytes.
    #[tokio::test]
    async fn a_marker_naming_the_wrong_release_is_refused() {
        let bytes = daemon_bytes("0.4.2+gabc1234");
        let mut files = files_with(bytes);
        let source = FakeSource::publishing("v0.4.2", &daemon_bytes("0.4.1+gabc1234"));
        let err = go(&mut files, &source, &mut yes_prompt())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("mismatch"), "{err}");
        untouched(&files);
    }

    #[tokio::test]
    async fn an_older_copy_is_refused_before_anything_is_fetched() {
        let bytes = daemon_bytes("0.3.9+gabc1234");
        let mut files = files_with(bytes.clone());
        let source = FakeSource::publishing("v0.3.9", &bytes);
        let err = go(&mut files, &source, &mut yes_prompt())
            .await
            .unwrap_err();
        assert!(
            matches!(err.downcast_ref::<Skip>(), Some(Skip::Older(_))),
            "{err}"
        );
        assert!(
            err.to_string().contains("older than this faber (0.4.0)"),
            "{err}"
        );
        assert_eq!(source.calls.get(), 0);
        untouched(&files);

        // Older than the installed copy, though newer than this binary.
        let bytes = daemon_bytes("0.4.1+gabc1234");
        let mut files = files_with(bytes.clone());
        files.installed = Some("0.5.0+g2222222".to_string());
        let source = FakeSource::publishing("v0.4.1", &bytes);
        let err = go(&mut files, &source, &mut yes_prompt())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("installed CLI copy"), "{err}");
        untouched(&files);
    }

    #[tokio::test]
    async fn an_equal_copy_is_up_to_date() {
        let bytes = daemon_bytes("0.4.0+g1111111");
        let mut files = files_with(bytes.clone());
        let source = FakeSource::publishing("v0.4.0", &bytes);
        let outcome = go(&mut files, &source, &mut yes_prompt()).await.unwrap();
        assert_eq!(outcome, Outcome::UpToDate);
        assert_eq!(source.calls.get(), 0, "nothing fetched");
        untouched(&files);

        // The installed copy already has it, though this binary is older.
        let bytes = daemon_bytes("0.4.1+gabc1234");
        let mut files = files_with(bytes.clone());
        files.installed = Some("0.4.1+gabc1234".to_string());
        let source = FakeSource::publishing("v0.4.1", &bytes);
        let outcome = go(&mut files, &source, &mut yes_prompt()).await.unwrap();
        assert_eq!(outcome, Outcome::UpToDate);
        untouched(&files);
    }

    #[tokio::test]
    async fn a_missing_daemon_copy_names_its_path() {
        let mut files = FakeFiles::default();
        let source = FakeSource::publishing("v0.4.1", b"");
        let err = go(&mut files, &source, &mut yes_prompt())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("/srv/faber/bin/faber"), "{err}");
        assert!(matches!(
            err.downcast_ref::<Skip>(),
            Some(Skip::NoDaemonCopy(_))
        ));
        untouched(&files);
        assert_eq!(source.calls.get(), 0);
    }

    #[tokio::test]
    async fn a_copy_without_a_marker_is_refused() {
        let mut files = files_with(b"\x7fELF no marker here".to_vec());
        let source = FakeSource::publishing("v0.4.1", b"");
        let err = go(&mut files, &source, &mut yes_prompt())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no version marker"), "{err}");
        untouched(&files);
    }

    /// Provenance that is refused, or cannot be checked, installs nothing,
    /// and the reason survives.
    #[tokio::test]
    async fn unverified_provenance_is_refused() {
        let bytes = daemon_bytes("0.4.1+gabc1234");
        for (verdict, want) in [
            (
                Verdict::Refused("workflow: not release.yml".to_string()),
                "failed verification: workflow: not release.yml",
            ),
            (
                Verdict::Unavailable("TUF unreachable".to_string()),
                "TUF unreachable; nothing was changed, retry later",
            ),
        ] {
            let mut files = files_with(bytes.clone());
            let mut source = FakeSource::publishing("v0.4.1", &bytes);
            source.verdict = verdict;
            let err = go(&mut files, &source, &mut yes_prompt())
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains(want), "{err}");
            untouched(&files);
        }
    }

    #[tokio::test]
    async fn a_draft_or_prerelease_is_refused() {
        let bytes = daemon_bytes("0.4.1+gabc1234");
        for draft in [true, false] {
            let mut files = files_with(bytes.clone());
            let mut source = FakeSource::publishing("v0.4.1", &bytes);
            let release = source.release.as_mut().unwrap();
            release.draft = draft;
            release.prerelease = !draft;
            let err = go(&mut files, &source, &mut yes_prompt())
                .await
                .unwrap_err()
                .to_string();
            assert!(err.contains("draft or a pre-release"), "{err}");
            assert!(source.verified.borrow().is_empty());
            untouched(&files);
        }
    }

    #[tokio::test]
    async fn a_release_that_does_not_exist_is_refused() {
        let bytes = daemon_bytes("0.4.1+gabc1234");
        let mut files = files_with(bytes.clone());
        let mut source = FakeSource::publishing("v0.4.1", &bytes);
        source.release = None;
        let err = go(&mut files, &source, &mut yes_prompt())
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("has no release v0.4.1"), "{err}");
        untouched(&files);
    }

    // --- the skew line --------------------------------------------------------

    /// After a promotion the checker is booted out and bootstrapped again,
    /// never kickstarted with `-k` (#479), only when its plist is installed,
    /// and a failure, or a checker that never runs, is a warning.
    #[test]
    fn the_wall_checker_is_restarted_on_the_new_copy() {
        use crate::setup::tools::{FakeRunner, Output};
        let print = "/bin/launchctl print system/io.github.rejifald.faber.wall";
        let shown = |out: &str| Output {
            success: true,
            code: Some(0),
            stdout: out.into(),
            ..Output::default()
        };
        let gone = Output {
            success: false,
            code: Some(113),
            ..Output::default()
        };
        let mut r = FakeRunner::default();
        assert_eq!(restart_wall_checker(&mut r, false), None);
        assert!(r.ran.is_empty());
        let mut r = FakeRunner {
            answers: vec![
                (print.into(), shown("state = running")),
                (print.into(), gone.clone()),
                (print.into(), shown("state = running")),
            ],
            ..FakeRunner::default()
        };
        let warning = restart_wall_checker(&mut r, true);
        if Os::HOST != Os::MacOs {
            assert!(r.ran.is_empty());
            return;
        }
        assert_eq!(warning, None);
        assert_eq!(
            r.ran
                .iter()
                .map(|c| c.display())
                .filter(|c| !c.starts_with("/bin/launchctl print "))
                .collect::<Vec<_>>(),
            [
                "/bin/launchctl bootout system/io.github.rejifald.faber.wall",
                "/bin/launchctl bootstrap system \
                 /Library/LaunchDaemons/io.github.rejifald.faber.wall.plist",
                "/bin/launchctl kickstart system/io.github.rejifald.faber.wall",
            ]
        );
        let mut r = FakeRunner {
            answers: vec![
                (print.into(), gone),
                (
                    "/bin/launchctl bootstrap system \
                     /Library/LaunchDaemons/io.github.rejifald.faber.wall.plist"
                        .into(),
                    Output {
                        success: false,
                        code: Some(5),
                        ..Output::default()
                    },
                ),
            ],
            ..FakeRunner::default()
        };
        let w = restart_wall_checker(&mut r, true).unwrap();
        assert!(w.contains("writes no verdict"), "{w}");
    }

    #[test]
    fn the_skew_line_appears_only_for_a_newer_daemon() {
        assert_eq!(
            skew_line("0.4.0+gabc1234", "0.4.1+gdef5678").as_deref(),
            Some("CLI 0.4.0 < daemon 0.4.1: run `sudo faber upgrade-cli`")
        );
        assert_eq!(skew_line("0.4.0", "0.4.0+gdef5678"), None);
        assert_eq!(skew_line("0.4.1", "0.4.0"), None);
        assert_eq!(skew_line("0.4.0", "not a version"), None);
        assert_eq!(skew_line("0.4.0", ""), None);
    }

    #[test]
    fn hostile_versions_are_stripped_before_printing() {
        let line = skew_line("0.4.0", "0.4.1+g\u{1b}]abc\u{7}\r").unwrap();
        assert!(!line.chars().any(char::is_control), "{line:?}");
        assert!(line.starts_with("CLI 0.4.0 < daemon 0.4.1"), "{line}");
    }

    #[test]
    fn the_daemon_version_is_read_from_update_current() {
        let state = serde_json::json!({"update": {"current": "0.4.1+gabc1234"}});
        assert_eq!(
            daemon_version_from_state(&state).as_deref(),
            Some("0.4.1+gabc1234")
        );
        for missing in [
            serde_json::json!({}),
            serde_json::json!({"update": {}}),
            serde_json::json!({"update": {"current": 4}}),
            serde_json::json!("0.4.1"),
        ] {
            assert_eq!(daemon_version_from_state(&missing), None, "{missing}");
        }
    }

    #[test]
    fn the_cli_copy_view_carries_the_same_line() {
        let view = cli_copy_view_for("0.4.0+gabc1234", "0.4.1+gdef5678");
        assert!(view.behind);
        assert_eq!(view.line, skew_line("0.4.0+gabc1234", "0.4.1+gdef5678"));
        let current = cli_copy_view_for("0.4.1+gdef5678", "0.4.1+gdef5678");
        assert!(!current.behind);
        assert_eq!(current.line, None);
    }
}
