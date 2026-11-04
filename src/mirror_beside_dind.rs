//! The mirrors that carry faber's GitHub token stay off beside dind: the
//! interim until the token leaves every VM (docs/git-mirror.md, "Not beside
//! dind").
//!
//! Both put the token on the Docker engine that runs jobs. A git mirror's
//! refresh mounts it into a container there (ADR-0035 §1), and `[mirror.lfs]`
//! keeps it in the `faber-mirror-auth` volume for as long as the mirror runs
//! (ADR-0047 §4). It is faber's one token, so it carries Administration: write on
//! every watched repo (ADR-0027). A job given dind is root on that engine
//! (ADR-0026), and root reads any volume and any container there. The owner
//! decided that no GitHub credential may enter a VM; a broker on the host, in an
//! ADR of its own, will keep it out. Until then:
//!
//! 1. **Refused where a human is looking.** `faber check-config` and the
//!    settings save refuse `git_mirror` and `[mirror.lfs]` while any docker
//!    profile grants dind: `dind = true`, or no `dind` key ([`config_error`]).
//! 2. **Off, and said so, at boot.** The daemon never refuses to start over a
//!    config (AGENTS.md). It leaves both off, warns at every boot and puts a
//!    notice on the dashboard ([`Verdict`]).
//! 3. **The engine is remembered.** Once this host's engine has granted dind, a
//!    job may have left something on it that reads the token later, so the
//!    mirrors stay off on it after every profile denies dind. The marker is a
//!    file in faber's state directory ([`MARKER_FILE`]), out of every job's
//!    reach: one kept on the engine is exactly what such a job could delete.
//!    faber has written it only since it began refusing these mirrors, so a
//!    grant made before then left none, and the operator creates it by hand
//!    ([`marked`]).
//! 4. **What the token touched goes.** While the mirrors are off, faber deletes
//!    the volumes that held the token or could steer a fetch holding it: git
//!    mirrors, their refresh credentials and `faber-mirror-auth`, selected by
//!    faber's labels, never by name ([`clean`]).
//!
//! The npm half of `[mirror]` carries no token and is untouched.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::backend::{Backend, Profile};
use crate::config::{BackendKind, FileConfig};
use crate::docker::{
    self, KIND_GIT_AUTH, KIND_GIT_MIRROR, KIND_MIRROR_AUTH, LABEL_KIND, LABEL_OWNED,
};
use crate::mirror::MirrorPolicy;
use crate::untrusted;

/// The marker that this host's Docker engine has granted dind, in faber's state
/// directory ([`marker_path`]).
pub const MARKER_FILE: &str = "engine-dind.json";

/// The dashboard notice's `kind` while a mirror the config asks for is off here.
pub const NOTICE_KIND: &str = "mirrors_beside_dind";

/// The volumes deleted while the mirrors are off: git mirrors, whose config and
/// hooks a root job could have planted for the next refresh, and the two kinds
/// that held the token itself.
pub const KINDS: [&str; 3] = [KIND_GIT_MIRROR, KIND_GIT_AUTH, KIND_MIRROR_AUTH];

/// How long a cleanup waits before trying again what it could not delete: a
/// volume a container still mounts, or a Docker that does not answer yet.
const RETRY: Duration = Duration::from_secs(60);

/// Where the docs explain all of this, for the operator-facing sentences.
const DOCS: &str = "docs/git-mirror.md, \"Not beside dind\"";

// --- the decision (pure) ---------------------------------------------------------

/// What a config asks of the two mirrors that carry faber's token.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenMirrors {
    /// A profile sets `git_mirror = true`.
    pub git: bool,
    /// `[mirror.lfs]` is written.
    pub lfs: bool,
}

impl TokenMirrors {
    /// What `cfg` asks for.
    pub fn of(cfg: &FileConfig) -> Self {
        Self {
            git: cfg.profiles.iter().any(|p| p.git_mirror),
            lfs: cfg.mirror.as_ref().is_some_and(|m| m.lfs.is_some()),
        }
    }

    /// Whether either is asked for.
    pub fn any(self) -> bool {
        self.git || self.lfs
    }

    /// The keys asked for, as a sentence names them.
    fn names(self) -> &'static str {
        match (self.git, self.lfs) {
            (true, true) => "git_mirror and [mirror.lfs]",
            (true, false) => "git_mirror",
            _ => "[mirror.lfs]",
        }
    }

    /// [`Self::names`], then the verb that agrees with them.
    fn subject_is(self) -> String {
        let verb = if self.git && self.lfs { "are" } else { "is" };
        format!("{} {verb}", self.names())
    }

    /// The pronoun and verb that start the next sentence about them.
    fn they_stay(self) -> &'static str {
        if self.git && self.lfs {
            "They stay"
        } else {
            "It stays"
        }
    }
}

/// The docker profiles that grant dind: `dind = true`, or no `dind` key, which
/// still grants (ADR-0013).
pub fn granting_profiles(cfg: &FileConfig) -> Vec<String> {
    cfg.profiles
        .iter()
        .filter(|p| p.backend == BackendKind::Docker && p.dind != Some(false))
        .map(|p| p.name.clone())
        .collect()
}

/// `profile 'a' grants`, `profiles 'a' and 'b' grant`, `profiles 'a', 'b' and
/// 'c' grant`.
fn profiles_grant(names: &[String]) -> String {
    let quoted: Vec<String> = names.iter().map(|n| format!("'{n}'")).collect();
    match quoted.as_slice() {
        [] => "no profile grants".to_string(),
        [one] => format!("profile {one} grants"),
        [rest @ .., last] => format!("profiles {} and {last} grant", rest.join(", ")),
    }
}

/// What `faber check-config` and the settings save refuse: a mirror that carries
/// faber's token on a host with a profile that grants dind. Policy, not
/// structure, so an unattended update is never held back over it.
pub fn config_error(cfg: &FileConfig) -> Option<String> {
    let asked = TokenMirrors::of(cfg);
    let granting = granting_profiles(cfg);
    if !asked.any() || granting.is_empty() {
        return None;
    }
    Some(format!(
        "{} would put faber's GitHub token, which carries Administration: write on every \
         watched repo (ADR-0027), on the Docker engine where a dind job is root: {} dind \
         (`dind = true`, or no `dind` key). Turn {} off, or set dind = false on every docker \
         profile, which is enough only on an engine that has never granted dind. A grant made \
         before faber began refusing this left no engine marker: if this host made one, create \
         the marker before the daemon restarts ({DOCS})",
        asked.names(),
        profiles_grant(&granting),
        asked.names()
    ))
}

/// Why the mirrors are off on this host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    /// These docker profiles grant dind now.
    Granted(Vec<String>),
    /// None does now, but this host's engine has: the marker is there.
    Marked,
}

/// What boot does about the mirrors that carry faber's token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// What the config asks for.
    pub asked: TokenMirrors,
    /// Why they are off, or `None`: they run as the config says.
    pub cause: Option<Cause>,
    /// This boot is the first to see the engine grant dind, so the marker is
    /// written now.
    pub mark: bool,
}

/// Decide from the config and whether the marker is there.
pub fn decide(cfg: &FileConfig, marked: bool) -> Verdict {
    let granting = granting_profiles(cfg);
    let granted = !granting.is_empty();
    let cause = if granted {
        Some(Cause::Granted(granting))
    } else if marked {
        Some(Cause::Marked)
    } else {
        None
    };
    Verdict {
        asked: TokenMirrors::of(cfg),
        cause,
        mark: granted && !marked,
    }
}

impl Verdict {
    /// Whether the mirrors are off here.
    pub fn refused(&self) -> bool {
        self.cause.is_some()
    }

    /// Leave both off in what this process runs: no profile mounts a git mirror,
    /// so the git mirror is never configured, and the pull-through mirror runs
    /// without LFS, which takes its credential away at its next pass. The file
    /// itself is left as written.
    pub fn apply(&self, profiles: &mut [Profile], mirror: &mut Option<MirrorPolicy>) {
        if !self.refused() {
            return;
        }
        for p in profiles {
            if let Backend::Docker(d) = &mut p.backend {
                d.git_mirror = false;
            }
        }
        if let Some(m) = mirror {
            m.lfs = None;
        }
    }

    /// The boot warning, also the dashboard notice: only while a mirror the
    /// config asks for is off. `marker` is where the marker is kept.
    pub fn warning(&self, marker: Option<&Path>) -> Option<String> {
        if !self.asked.any() {
            return None;
        }
        let subject = self.asked.subject_is();
        let they_stay = self.asked.they_stay();
        match self.cause.as_ref()? {
            Cause::Granted(names) => Some(format!(
                "{subject} off: {} dind (`dind = true`, or no `dind` key), and a dind job is root \
                 on the Docker engine that would hold faber's GitHub token, which carries \
                 Administration: write on every watched repo. {they_stay} off on this engine from \
                 now on, even once every profile says dind = false ({DOCS})",
                profiles_grant(names)
            )),
            Cause::Marked => Some(format!(
                "{subject} off: this host's Docker engine has granted dind before, and a job that \
                 was root on it may have left something behind that would read faber's GitHub \
                 token. {they_stay} off on this engine; {DOCS} says how to start from a fresh \
                 one. Marker: {}",
                marker.map_or_else(|| MARKER_FILE.to_string(), |p| p.display().to_string())
            )),
        }
    }
}

// --- the marker ------------------------------------------------------------------

/// What faber writes in the marker file: when it first saw this engine grant
/// dind. faber goes by the file's presence alone ([`marked`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Marker {
    pub since_ms: u64,
}

/// The marker's place: faber's state directory, which job code cannot reach.
/// `None` without one.
pub fn marker_path() -> Option<PathBuf> {
    crate::platform::state_dir().map(|dir| dir.join(MARKER_FILE))
}

/// Whether the marker is at `path`. Fails closed: anything there counts, read
/// or not, and so does a path that cannot be looked at. So an empty file an
/// operator creates to record a grant from before the marker counts too.
pub fn marked(path: &Path) -> bool {
    match std::fs::symlink_metadata(path) {
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            warn!(
                "{}: {e} — treating this host's Docker engine as one that has granted dind",
                path.display()
            );
            true
        }
    }
}

/// Write the marker at `path`, through a temporary file so a crash leaves
/// either none or a whole one.
pub fn write_marker(path: &Path, since_ms: u64) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_vec(&Marker { since_ms })?)?;
    std::fs::rename(&tmp, path)
}

/// Boot's half: read the marker at `marker`, decide from `cfg`, and write the
/// marker the first time this engine is seen granting dind.
pub fn at_boot(cfg: &FileConfig, marker: Option<&Path>, now_ms: u64) -> Verdict {
    let verdict = decide(cfg, marker.is_some_and(marked));
    if !verdict.mark {
        return verdict;
    }
    match marker.map(|path| (path, write_marker(path, now_ms))) {
        Some((path, Ok(()))) => info!(
            "this host's Docker engine grants dind: git_mirror and [mirror.lfs] stay off on it \
             from now on (marker {}; {DOCS})",
            path.display()
        ),
        Some((path, Err(e))) => warn!(
            "could not record at {} that this host's Docker engine grants dind ({e}): \
             git_mirror and [mirror.lfs] are off while a profile grants it, but would come \
             back once none does",
            path.display()
        ),
        None => warn!(
            "no state directory to record that this host's Docker engine grants dind in: \
             git_mirror and [mirror.lfs] are off while a profile grants it, but would come back \
             once none does"
        ),
    }
    verdict
}

// --- the cleanup -----------------------------------------------------------------

/// The Docker volumes the cleanup lists and deletes: faber's Docker in
/// production ([`DockerVolumes`]), a fake in tests.
pub trait Volumes: Send + Sync {
    /// The names of every volume labelled faber's (`org.faber.owned=true`) and of
    /// `kind` (`org.faber.kind`).
    fn labelled(&self, kind: &str) -> impl Future<Output = anyhow::Result<Vec<String>>> + Send;

    /// Delete the volume `name`: `Ok(false)` when it was already gone.
    fn remove(&self, name: &str) -> impl Future<Output = anyhow::Result<bool>> + Send;
}

/// faber's own Docker.
pub struct DockerVolumes;

impl Volumes for DockerVolumes {
    async fn labelled(&self, kind: &str) -> anyhow::Result<Vec<String>> {
        docker::docker_lines(&ls_args(kind), docker::CLI_TIMEOUT).await
    }

    async fn remove(&self, name: &str) -> anyhow::Result<bool> {
        docker::docker_rm(&["volume", "rm", name]).await
    }
}

/// `docker volume ls` of every volume carrying both of faber's labels for
/// `kind`. Labels, never names (AGENTS.md).
fn ls_args(kind: &str) -> Vec<String> {
    vec![
        "volume".into(),
        "ls".into(),
        "-q".into(),
        "--filter".into(),
        format!("label={LABEL_OWNED}=true"),
        "--filter".into(),
        format!("label={LABEL_KIND}={kind}"),
    ]
}

/// One pass over [`KINDS`]: each labelled volume deleted. Returns how many are
/// still there, or could not be listed; `0` means done. A volume a container
/// still mounts is refused `in use` by the daemon and left for the next pass,
/// so nothing a running job holds is pulled from under it.
pub async fn clean_once(volumes: &impl Volumes) -> usize {
    let mut left = 0;
    for kind in KINDS {
        let names = match volumes.labelled(kind).await {
            Ok(names) => names,
            Err(e) => {
                debug!("mirrors beside dind: listing {kind} volumes: {e}");
                left += 1;
                continue;
            }
        };
        for name in names {
            if !untrusted::object_name(&name) {
                warn!("mirrors beside dind: a {kind} volume's name is outside Docker's alphabet");
                continue;
            }
            match volumes.remove(&name).await {
                Ok(true) => info!(
                    "mirrors beside dind: deleted {name} ({kind}), which held or steered faber's \
                     GitHub token"
                ),
                Ok(false) => {}
                Err(e) => {
                    if e.to_string().to_lowercase().contains("in use") {
                        debug!("mirrors beside dind: {name} is in use — deleting it later");
                    } else {
                        warn!("mirrors beside dind: deleting {name}: {e}");
                    }
                    left += 1;
                }
            }
        }
    }
    left
}

/// Delete everything of [`KINDS`] faber labelled, pass after pass until one
/// leaves nothing. Spawned at boot while the mirrors are off; on a host that
/// never ran one, a single pass of three listings.
pub async fn clean(volumes: impl Volumes) {
    while clean_once(&volumes).await > 0 {
        tokio::time::sleep(RETRY).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    /// The docker profile every case starts from, with `dind` and `extra` as given.
    fn config(dind: &str, extra: &str, tail: &str) -> FileConfig {
        FileConfig::load_from_str(
            &format!(
                r#"
                [[profile]]
                name = "linux-docker"
                labels = ["faber", "docker"]
                backend = "docker"
                image = "ghcr.io/owner/my-runner:latest"
                {dind}
                {extra}

                {tail}
                "#
            ),
            "test",
        )
        .unwrap()
    }

    const BOTH: &str = "[mirror]\n[mirror.lfs]\n[mirror.npm]\n";

    fn scratch(what: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "faber-beside-dind-{what}-{}-{}",
            std::process::id(),
            crate::audit::now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // --- refused where a human is looking --------------------------------------

    #[test]
    fn validate_refuses_either_mirror_beside_an_unstated_or_granted_dind() {
        for dind in ["", "dind = true"] {
            for (extra, tail, named) in [
                ("git_mirror = true", "", "git_mirror would put"),
                ("mirror = true", BOTH, "[mirror.lfs] would put"),
                (
                    "git_mirror = true\nmirror = true",
                    BOTH,
                    "git_mirror and [mirror.lfs]",
                ),
            ] {
                let cfg = config(dind, extra, tail);
                let errs = cfg.validate();
                assert!(
                    errs.iter().any(|e| e.contains(named)
                        && e.contains("profile 'linux-docker' grants dind")
                        && e.contains("set dind = false on every docker profile")
                        && e.contains("create the marker before the daemon restarts")),
                    "{dind:?} {extra:?}: {errs:?}"
                );
            }
        }
    }

    #[test]
    fn validate_lets_them_be_beside_a_denied_dind_and_npm_beside_any() {
        let cfg = config("dind = false", "git_mirror = true\nmirror = true", BOTH);
        assert_eq!(config_error(&cfg), None);
        assert!(cfg.validate().is_empty(), "{:?}", cfg.validate());
        let npm_only = config("", "mirror = true", "[mirror]\n[mirror.npm]\n");
        assert_eq!(config_error(&npm_only), None);
    }

    #[test]
    fn every_granting_profile_is_named_and_a_macos_one_never_is() {
        let cfg = FileConfig::load_from_str(
            r#"
            [[profile]]
            name = "a"
            labels = ["faber", "a"]
            backend = "docker"
            image = "ghcr.io/owner/my-runner:latest"
            git_mirror = true

            [[profile]]
            name = "b"
            labels = ["faber", "b"]
            backend = "docker"
            image = "ghcr.io/owner/my-runner:latest"
            dind = true

            [[profile]]
            name = "c"
            labels = ["faber", "c"]
            backend = "docker"
            image = "ghcr.io/owner/my-runner:latest"
            dind = false

            [[profile]]
            name = "mac"
            labels = ["faber", "mac"]
            backend = "macos-vm"
            image = "macos-27"
            "#,
            "test",
        )
        .unwrap();
        assert_eq!(granting_profiles(&cfg), vec!["a", "b"]);
        assert!(config_error(&cfg)
            .unwrap()
            .contains("profiles 'a' and 'b' grant dind"));
        assert_eq!(
            profiles_grant(&["a".into(), "b".into(), "c".into()]),
            "profiles 'a', 'b' and 'c' grant"
        );
    }

    // --- off, and said so, at boot ---------------------------------------------

    #[test]
    fn boot_leaves_both_mirrors_off_with_a_warning_and_marks_the_engine() {
        let dir = scratch("boot");
        let marker = dir.join(MARKER_FILE);
        let cfg = config("", "git_mirror = true\nmirror = true", BOTH);

        let verdict = at_boot(&cfg, Some(&marker), 1_000);
        assert!(verdict.refused() && verdict.mark);
        assert_eq!(
            verdict.cause,
            Some(Cause::Granted(vec!["linux-docker".to_string()]))
        );
        let written: Marker = serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
        assert_eq!(written, Marker { since_ms: 1_000 });

        let mut profiles = cfg.clone().into_profiles("2.330.0").unwrap();
        let mut mirror = cfg.mirror_policy();
        assert!(mirror.as_ref().unwrap().lfs.is_some());
        verdict.apply(&mut profiles, &mut mirror);
        for p in &profiles {
            if let Backend::Docker(d) = &p.backend {
                assert!(
                    !d.git_mirror,
                    "profile {} still mounts a git mirror",
                    p.name
                );
                assert!(d.mirror, "the npm half is untouched");
            }
        }
        let mirror = mirror.expect("the pull-through mirror still runs");
        assert_eq!(mirror.lfs, None);
        assert!(mirror.npm.is_some(), "npm carries no token and stays on");

        let warning = verdict.warning(Some(&marker)).unwrap();
        assert!(
            warning.starts_with(
                "git_mirror and [mirror.lfs] are off: profile 'linux-docker' grants dind"
            ),
            "{warning}"
        );
        assert!(warning.contains("Administration: write"), "{warning}");
        assert!(warning.contains("even once every profile says dind = false"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_marked_engine_keeps_them_off_after_dind_is_denied() {
        let dir = scratch("marked");
        let marker = dir.join(MARKER_FILE);
        write_marker(&marker, 5).unwrap();
        let cfg = config("dind = false", "git_mirror = true", "");
        assert!(cfg.validate().is_empty(), "the file itself is fine");

        let verdict = at_boot(&cfg, Some(&marker), 9);
        assert_eq!(verdict.cause, Some(Cause::Marked));
        assert!(!verdict.mark, "the marker is written once");
        let written: Marker = serde_json::from_slice(&std::fs::read(&marker).unwrap()).unwrap();
        assert_eq!(written.since_ms, 5, "and never moved");
        let mut profiles = cfg.clone().into_profiles("2.330.0").unwrap();
        verdict.apply(&mut profiles, &mut None);
        assert!(profiles.iter().all(|p| match &p.backend {
            Backend::Docker(d) => !d.git_mirror,
            _ => true,
        }));
        let warning = verdict.warning(Some(&marker)).unwrap();
        assert!(warning
            .starts_with("git_mirror is off: this host's Docker engine has granted dind before"));
        assert!(warning.contains(&marker.display().to_string()));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_engine_that_never_granted_dind_runs_them_as_written() {
        let dir = scratch("clean");
        let marker = dir.join(MARKER_FILE);
        let cfg = config("dind = false", "git_mirror = true\nmirror = true", BOTH);
        let verdict = at_boot(&cfg, Some(&marker), 1);
        assert!(!verdict.refused() && !verdict.mark);
        assert!(!marker.exists());
        let mut profiles = cfg.clone().into_profiles("2.330.0").unwrap();
        let mut mirror = cfg.mirror_policy();
        verdict.apply(&mut profiles, &mut mirror);
        assert!(profiles.iter().any(|p| match &p.backend {
            Backend::Docker(d) => d.git_mirror,
            _ => false,
        }));
        assert!(mirror.unwrap().lfs.is_some());
        assert_eq!(verdict.warning(Some(&marker)), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_host_that_asks_for_neither_is_marked_but_not_warned() {
        let dir = scratch("quiet");
        let marker = dir.join(MARKER_FILE);
        let verdict = at_boot(&config("", "", ""), Some(&marker), 1);
        assert!(verdict.refused() && verdict.mark && marker.exists());
        assert_eq!(verdict.warning(Some(&marker)), None);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_marker_path_counts_as_marked() {
        let dir = scratch("unreadable");
        // A path through a file fails with "not a directory", not "not found".
        let file = dir.join("file");
        std::fs::write(&file, "").unwrap();
        assert!(marked(&file.join(MARKER_FILE)));
        assert!(!marked(&dir.join(MARKER_FILE)));
        std::fs::write(dir.join(MARKER_FILE), "not json").unwrap();
        assert!(
            marked(&dir.join(MARKER_FILE)),
            "content is never needed to count"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // --- what the token touched goes -------------------------------------------

    /// Docker as the cleanup sees it: volumes by name, each with the kind label
    /// faber stamped, or none; some still mounted.
    #[derive(Default)]
    struct FakeVolumes {
        volumes: Mutex<BTreeMap<String, Option<&'static str>>>,
        in_use: Mutex<Vec<String>>,
        listed: Mutex<Vec<String>>,
        removed: Mutex<Vec<String>>,
        down: Mutex<bool>,
    }

    impl Volumes for FakeVolumes {
        async fn labelled(&self, kind: &str) -> anyhow::Result<Vec<String>> {
            anyhow::ensure!(
                !*self.down.lock().unwrap(),
                "Cannot connect to the Docker daemon"
            );
            self.listed.lock().unwrap().push(kind.to_string());
            Ok(self
                .volumes
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, k)| **k == Some(kind))
                .map(|(name, _)| name.clone())
                .collect())
        }

        async fn remove(&self, name: &str) -> anyhow::Result<bool> {
            if self.in_use.lock().unwrap().iter().any(|n| n == name) {
                anyhow::bail!("Error response from daemon: remove {name}: volume is in use");
            }
            self.removed.lock().unwrap().push(name.to_string());
            Ok(self.volumes.lock().unwrap().remove(name).is_some())
        }
    }

    #[tokio::test]
    async fn the_cleanup_deletes_the_labelled_volumes_and_nothing_else() {
        let docker = FakeVolumes::default();
        {
            let mut v = docker.volumes.lock().unwrap();
            v.insert("faber-git-owner-demo".into(), Some(KIND_GIT_MIRROR));
            v.insert("faber-gitauth-refresh-1-0".into(), Some(KIND_GIT_AUTH));
            v.insert("faber-mirror-auth".into(), Some(KIND_MIRROR_AUTH));
            // The names, without the labels: not faber's to delete (AGENTS.md).
            v.insert("faber-git-owner-other".into(), None);
            v.insert("faber-mirror-auth-copy".into(), None);
            // faber's, but nothing the token touched.
            v.insert("faber-cache-owner-demo".into(), Some(docker::KIND_CACHE));
            v.insert("faber-mirror-cache".into(), Some(docker::KIND_MIRROR));
        }
        docker
            .in_use
            .lock()
            .unwrap()
            .push("faber-mirror-auth".into());

        assert_eq!(clean_once(&docker).await, 1, "the mounted one is left");
        assert_eq!(*docker.listed.lock().unwrap(), KINDS.to_vec());
        docker.in_use.lock().unwrap().clear();
        assert_eq!(clean_once(&docker).await, 0);

        let mut removed = docker.removed.lock().unwrap().clone();
        removed.sort();
        assert_eq!(
            removed,
            vec![
                "faber-git-owner-demo",
                "faber-gitauth-refresh-1-0",
                "faber-mirror-auth"
            ]
        );
        let kept: Vec<String> = docker.volumes.lock().unwrap().keys().cloned().collect();
        assert_eq!(
            kept,
            vec![
                "faber-cache-owner-demo",
                "faber-git-owner-other",
                "faber-mirror-auth-copy",
                "faber-mirror-cache"
            ]
        );
    }

    #[tokio::test]
    async fn a_docker_that_does_not_answer_yet_is_tried_again() {
        let docker = FakeVolumes::default();
        *docker.down.lock().unwrap() = true;
        assert_eq!(clean_once(&docker).await, KINDS.len());
        *docker.down.lock().unwrap() = false;
        assert_eq!(
            clean_once(&docker).await,
            0,
            "nothing there: one pass, done"
        );
    }

    #[test]
    fn the_listing_reads_both_labels_and_never_a_name() {
        for kind in KINDS {
            let args = ls_args(kind);
            assert!(args.contains(&format!("label={LABEL_OWNED}=true")));
            assert!(args.contains(&format!("label={LABEL_KIND}={kind}")));
            assert!(!args.iter().any(|a| a.starts_with("name=")), "{args:?}");
        }
    }
}
