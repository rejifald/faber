//! On-demand runner-image variants, cached while hot (ADR-0008).
//!
//! A runner image bakes a version-locked toolchain — Playwright's browsers at
//! `/opt/ms-playwright` — and can bake exactly one version. Repos do not have one
//! version at a time: a PR that bumps `@playwright/test` needs the new browsers
//! while every other open PR still needs the old ones, and that overlap lasts as
//! long as the PR is open. No single image is correct for that window, which is
//! why [`crate::drift`] could only ever *report* it.
//!
//! So the image stops being a property of the profile and becomes a function of
//! the job: faber derives a thin per-version variant of the profile's base image,
//! builds it the first time a job asks for it, and keeps it while it stays hot.
//!
//! Three properties are load-bearing:
//!
//! * **The recipe is faber's, never the repo's.** A variant build runs on the
//!   *host* daemon, outside the per-job dind sandbox that contains everything else
//!   a repo makes faber run. A repo may opt out of being healed; it can never say
//!   what is built or how.
//! * **The tag is keyed on the base's image id**, not its tag — so rebuilding the
//!   base orphans its variants instead of leaving one silently in service against
//!   a base it no longer matches.
//! * **Eviction is licensed by reproducibility.** AGENTS.md forbids faber ever
//!   reaping a runner image, because the operator's base exists on this builder
//!   and in no registry: one `docker rmi` takes CI down with nothing to re-pull.
//!   That rule is about *irreproducible* images. A variant faber built from a
//!   recipe faber owns can be rebuilt from nothing but its tag, which is what
//!   makes it evictable — and the only thing that does. The filter is
//!   `org.faber.kind=image`, which the base can never match.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::config::{needs_allow_in, NeedsVerdict, RunnerImagePolicy, ToolNeeds};
use crate::docker::{self, VariantBuild};
use crate::host;
use crate::setup::exe::{chain_of, PathFacts};
use crate::wall::trust::root_only;

/// faber's app-support directory: `~/Library/Application Support/faber`.
fn default_root() -> Option<PathBuf> {
    crate::platform::state_dir()
}

/// The one tool ADR-0008 bakes and ADR-0010 gates. Named rather than inlined so
/// the `[needs.<tool>]` key and the label namespace can never drift apart.
const PLAYWRIGHT_TOOL: &str = "playwright";

/// The last-used ledger's filename under the app-support root.
const LEDGER_FILE: &str = "image-cache.json";

/// The directory under the app-support root holding faber's own recipe. Its
/// contents are the build context, so nothing else may live there.
const RECIPE_DIR: &str = "runner-variant";

/// The local image repository every variant is tagged under. Local-only by
/// construction: nothing here is ever pushed anywhere (AGENTS.md).
pub const VARIANT_REPO: &str = "faber-runner";

/// How long a just-used variant is untouchable, whatever the pressure.
///
/// A job resolves its image *before* admission and may then park in the budget
/// queue, so between `ensure` returning a tag and a container existing for it
/// there is a window in which the variant is neither idle nor in use by anything
/// the daemon can see. Evicting inside that window would fail a job faber had
/// just built an image for, which is the one outcome this whole mechanism exists
/// to prevent. Five minutes comfortably covers a ramp, and costs a disk-pressure
/// pass nothing it will not get back on the next sweep.
const RESOLVE_GRACE_MS: u64 = 300_000;

/// What a variant build is charged against free space before it may start, in
/// GiB.
///
/// ADR-0008 measured a variant at about 2 GiB over its base (base layers are
/// shared). The charge sits above that on purpose, as ADR-0024's per-job
/// charges do: BuildKit keeps the browser archives it downloaded in its build
/// cache until a prune, and a newer Playwright's browsers are not smaller. A
/// constant until faber measures its own builds.
pub const BUILD_CHARGE_GIB: u64 = 4;

/// The least free space, in GiB, a variant build may leave behind it, whatever
/// the floors are set to.
///
/// Ten is ADR-0032's charge for a dind job: after the build there is still room
/// for the heaviest job admission would let in, and the disk stays well clear of
/// ADR-0032's 3 GiB critical floor, where faber starts freezing jobs. A stated
/// admission floor above it raises it.
pub const BUILD_MARGIN_MIN_GIB: u64 = 10;

/// The two disk thresholds variants act on (ADR-0008, amendment of 2026-10-30).
///
/// - `margin_gib`: the free space a build must leave behind it. Below it the
///   janitor's disk-pressure pass evicts every variant no job holds, as it once
///   did anywhere under the reclamation floor.
/// - `build_gib`: what a build needs free to start — its
///   [`BUILD_CHARGE_GIB`] on top of the margin.
///
/// Both are capped at `disk_floor_gib`, so neither is ever stricter than the one
/// floor both used before, and `0` still turns both off. The gap between them is
/// one build's charge, which is what keeps the two from feeding each other: a
/// pass that evicts below the margin leaves less than a rebuild needs, and a
/// build that starts at the threshold ends above the margin.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VariantDisk {
    pub margin_gib: u64,
    pub build_gib: u64,
}

impl VariantDisk {
    /// The thresholds for a host whose reclamation floor is `disk_floor_gib` and
    /// whose stated admission floor (ADR-0024 §1) is `admission_floor_gib`.
    pub fn new(disk_floor_gib: u64, admission_floor_gib: Option<u64>) -> Self {
        let margin_gib = admission_floor_gib
            .unwrap_or(0)
            .max(BUILD_MARGIN_MIN_GIB)
            .min(disk_floor_gib);
        let build_gib = margin_gib
            .saturating_add(BUILD_CHARGE_GIB)
            .min(disk_floor_gib);
        Self {
            margin_gib,
            build_gib,
        }
    }

    /// Why a build may not start with `free_mib` free, or `None` when it may.
    /// An unreadable reading never refuses (see [`host::below_disk_floor`]).
    pub fn build_refusal(&self, free_mib: Option<u64>) -> Option<String> {
        if !host::below_disk_floor(self.build_gib, free_mib) {
            return None;
        }
        Some(format!(
            "{} MiB free is below the {} GiB a variant build needs — its {} GiB charge on \
             top of the {} GiB it must leave free (ADR-0008) — refusing to start a \
             multi-gigabyte runner-image build",
            free_mib.unwrap_or(0),
            self.build_gib,
            self.build_gib.saturating_sub(self.margin_gib),
            self.margin_gib
        ))
    }

    /// Which eviction tier a disk-pressure pass that measured `free_mib` uses.
    /// Only below the margin does a warm variant go: above it a rebuild would
    /// be allowed, so evicting one trades its space now for the same space again
    /// on the next job that needs it.
    pub fn pressure_tier(&self, free_mib: Option<u64>) -> EvictTier {
        if host::below_disk_floor(self.margin_gib, free_mib) {
            EvictTier::Squeezed
        } else {
            EvictTier::Cold
        }
    }
}

/// How hard an eviction pass presses on the variant cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvictTier {
    /// The ordinary rule: beyond `keep`, or idle past the TTL.
    Cold,
    /// Every variant no job holds: free space is under the margin a build must
    /// leave, so a rebuild could not start anyway and the disk comes first.
    Squeezed,
}

/// How much of the base's `sha256:…` id goes into a variant tag. Twelve hex
/// characters is Docker's own short-id length and the collision odds over the
/// handful of base images one host ever holds are not worth a longer tag.
const BASE_ID_CHARS: usize = 12;

/// faber's own derivation recipe, materialized next to the config on first use.
///
/// Deliberately minimal. It says only how to *install* the toolchain; what the
/// resulting image is — owner, kind, base, baked version — is stamped by faber at
/// build time (see `build_variant_args`), so a recipe cannot mislabel an image
/// and an operator recipe does not have to remember the contract.
///
/// `BASE_USER` is passed in and restored: the install needs root for system
/// dependencies, and a runner image that silently starts running as root is a
/// different image in the way that matters most.
///
/// `PLAYWRIGHT_SKIP_BROWSER_GC` is not a micro-optimisation. Left to itself
/// `playwright install` deletes the browser revisions the new version doesn't
/// need — sound on a normal filesystem, actively wrong in a layered image, where
/// the deleted files stay in the parent layer regardless. The variant pays for the
/// base's browsers either way; pruning them only means it cannot *use* what it is
/// already carrying. Measured on a real runner image: the same size either way,
/// serving one version with the GC on and two with it off. Accumulation is bounded
/// at two sets because a variant is always derived `FROM` the base, never from
/// another variant.
///
/// There is a second, stronger reason not to "tidy" this flag away. JIT runners
/// are not pinned to the job they were minted for, so a runner started from this
/// variant is routinely handed a *different* repo job pinning the version the base
/// bakes. Prune the base's browsers and that job fails on a missing browser — faber
/// would have manufactured the exact failure class it exists to remove, for a job
/// that was fine before. Keeping both sets is what makes the swap survivable.
pub const VARIANT_DOCKERFILE: &str = r#"# Generated by faber (ADR-0008) — rewritten on every build.
# To use your own recipe instead, set `dockerfile` under [runner_image] in
# faber's config.toml; faber will then leave that file entirely alone.
ARG BASE
FROM ${BASE}
ARG PW_VERSION
ARG BASE_USER=root
USER root
RUN set -eux; \
    export PLAYWRIGHT_BROWSERS_PATH="${PLAYWRIGHT_BROWSERS_PATH:-/opt/ms-playwright}"; \
    export PLAYWRIGHT_SKIP_BROWSER_GC=1; \
    npx --yes "playwright@${PW_VERSION}" install --with-deps chromium firefox webkit; \
    mkdir -p "$PLAYWRIGHT_BROWSERS_PATH"; \
    printf '%s' "${PW_VERSION}" > "$PLAYWRIGHT_BROWSERS_PATH/.playwright-version"
USER ${BASE_USER}
"#;

/// A variant that is ready to run a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Variant {
    pub tag: String,
    /// Whether this call built it, as opposed to finding it warm. Only for the
    /// log and the audit note — the job cannot tell the difference.
    pub built: bool,
}

/// The variant tag for one (base, version) pair.
///
/// Keyed on the base's content id so a base rebuild produces different tags for
/// the same versions: the old variants fall out of use and are evicted normally,
/// rather than serving jobs against a base that no longer exists.
pub fn variant_tag(base_id: &str, version: &str) -> String {
    format!(
        "{VARIANT_REPO}:{}-pw-{}",
        short_id(base_id),
        tag_safe(version)
    )
}

/// The short form of a `sha256:…` image id used in a tag.
fn short_id(id: &str) -> String {
    let hex = id.strip_prefix("sha256:").unwrap_or(id);
    hex.chars().take(BASE_ID_CHARS).collect()
}

/// Fold a version string to what a Docker tag accepts. A resolved npm version is
/// already tag-safe; this exists so a prerelease or build-metadata pin
/// (`1.61.0-alpha+1`) produces a usable tag instead of a build that fails on its
/// own name.
fn tag_safe(version: &str) -> String {
    version
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

// --- whose recipe it is (ADR-0028, #238) -----------------------------------

/// How many entries the recipe's directory, the build context, may hold before
/// faber stops judging them and refuses: a recipe belongs in a directory of its
/// own, and walking a large tree on every settings save is not worth it.
const RECIPE_CONTEXT_MAX: usize = 4096;

const S_IFMT: u32 = 0o170_000;
const S_IFREG: u32 = 0o100_000;
const S_IFLNK: u32 = 0o120_000;
const DIR_TYPE: u32 = 0o040_000;
const GROUP_WRITE: u32 = 0o020;
const WORLD_WRITE: u32 = 0o002;

/// Whether root alone could have written an operator recipe and everything a
/// build from it reads (ADR-0028, #238): `chain` is the recipe and every
/// directory above it up to `/`, as [`crate::setup::exe::chain_of`] gathers it;
/// `context` is every entry under the recipe's directory, which is sent as the
/// build context so a recipe's `COPY` still works. Whoever can write any of
/// them changes every variant without touching the config.
///
/// The rule is [`crate::wall::trust::root_only`], the one the wall's stored
/// files are held to: any owner but root, or a group- or world-writable entry
/// (a sticky directory excepted), is refused. The sticky exception holds only
/// above the context: the recipe's own directory and every directory inside
/// it are refused when group- or world-writable, sticky or not, because any
/// account could add an entry there before `docker build` reads the context,
/// and a `COPY .` would carry it into every variant. A symlink in the context is
/// judged by its owner only: its mode bits mean nothing, and on Linux they are
/// always `0777`.
pub fn recipe_verdict(chain: &[PathFacts], context: &[PathFacts]) -> Result<(), String> {
    let Some(file) = chain.first() else {
        return Err("no path facts to judge".into());
    };
    if file.mode & S_IFMT != S_IFREG {
        return Err(format!("{} is not a regular file", file.path.display()));
    }
    root_only(chain)?;
    // The sticky exception covers only the directories ABOVE the context. In
    // the context itself, a sticky world-writable directory still lets any
    // account add an entry between the check and `docker build` taring it,
    // and a `COPY .` would carry that entry into every variant.
    for dir in chain
        .get(1)
        .into_iter()
        .chain(context.iter().filter(|e| e.mode & S_IFMT == DIR_TYPE))
    {
        if dir.mode & (GROUP_WRITE | WORLD_WRITE) != 0 {
            return Err(format!(
                "{} is group- or world-writable (mode {:o}), sticky or not: no directory \
                 in the recipe's build context may be",
                dir.path.display(),
                dir.mode & 0o7777
            ));
        }
    }
    for entry in context {
        let judged = if entry.mode & S_IFMT == S_IFLNK {
            PathFacts {
                mode: S_IFLNK | 0o755,
                ..entry.clone()
            }
        } else {
            entry.clone()
        };
        root_only(std::slice::from_ref(&judged))
            .map_err(|e| format!("{e} (in the recipe's build context)"))?;
    }
    Ok(())
}

/// The facts [`recipe_verdict`] judges, read from the filesystem: the recipe
/// resolved through every symlink with each ancestor, and every entry under
/// its directory, symlinks not followed.
fn recipe_facts(path: &Path) -> Result<(Vec<PathFacts>, Vec<PathFacts>), String> {
    use std::os::unix::fs::MetadataExt;
    let chain = chain_of(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let dir = chain
        .get(1)
        .map(|f| f.path.clone())
        .ok_or_else(|| format!("{} has no directory", path.display()))?;
    let mut context = Vec::new();
    let mut pending = vec![dir.clone()];
    while let Some(at) = pending.pop() {
        let entries =
            std::fs::read_dir(&at).map_err(|e| format!("cannot list {}: {e}", at.display()))?;
        for entry in entries {
            let entry = entry.map_err(|e| format!("cannot list {}: {e}", at.display()))?;
            let p = entry.path();
            let meta = std::fs::symlink_metadata(&p)
                .map_err(|e| format!("cannot read {}: {e}", p.display()))?;
            if context.len() >= RECIPE_CONTEXT_MAX {
                return Err(format!(
                    "{} holds more than {RECIPE_CONTEXT_MAX} entries: give the recipe a \
                     directory of its own",
                    dir.display()
                ));
            }
            if meta.is_dir() {
                pending.push(p.clone());
            }
            context.push(PathFacts {
                path: p,
                uid: meta.uid(),
                gid: meta.gid(),
                mode: meta.mode(),
            });
        }
    }
    Ok((chain, context))
}

/// The recipe at `path`, judged: on `Ok`, the resolved file to read and the
/// directory to send as the build context.
pub fn judge_recipe(path: &Path) -> Result<(PathBuf, PathBuf), String> {
    let (chain, context) = recipe_facts(path)?;
    recipe_verdict(&chain, &context)?;
    let dir = chain[1].path.clone();
    if let Some(secret) = context_holds_secrets(&dir, &secret_dirs()) {
        return Err(format!(
            "its build context {} holds {}, where faber keeps its GitHub credentials: a build \
             sends its whole context to Docker (the legacy builder sends every file in it)",
            dir.display(),
            secret.display()
        ));
    }
    Ok((chain[0].path.clone(), dir))
}

/// faber's root-owned directory, as written and as resolved (`/etc` is a link
/// to `/private/etc` on macOS): what no build context may be or contain.
fn secret_dirs() -> Vec<PathBuf> {
    let etc = PathBuf::from(crate::platform::layout::ETC_DIR);
    let mut dirs = vec![etc.clone()];
    if let Ok(real) = std::fs::canonicalize(&etc) {
        if real != etc {
            dirs.push(real);
        }
    }
    dirs
}

/// The first of `secrets` that `context` is or contains, if any. Pure.
fn context_holds_secrets<'a>(context: &Path, secrets: &'a [PathBuf]) -> Option<&'a PathBuf> {
    secrets.iter().find(|s| s.starts_with(context))
}

/// Why the operator recipe `policy` would build from cannot be trusted, if it
/// cannot: `None` without one, or with building off. `faber check-config` and
/// a settings save refuse a config with a problem here; the daemon warns at
/// boot and refuses each variant build from it, but never refuses to boot.
pub fn recipe_problem(policy: &RunnerImagePolicy) -> Option<String> {
    let path = policy
        .dockerfile
        .as_deref()
        .map(str::trim)
        .filter(|p| !p.is_empty() && policy.build.enabled())?;
    judge_recipe(Path::new(path)).err().map(|e| {
        format!(
            "runner_image.dockerfile {path}: only root may write the recipe, its build context \
             and every directory above it (ADR-0028): {e}. Move it under root, e.g. `sudo \
             install -o root -m 0644 {path} /etc/faber-recipe/` (a directory of its own: a build sends \
             its whole context to Docker)"
        )
    })
}

/// Last-used times for the variants faber built, keyed by tag.
///
/// Docker records no last-use time for an image, so "hot" is a fact only faber
/// can know: it is the one thing that starts the containers. Persisted so a
/// restart doesn't reset every variant's age to now and defeat the TTL.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Ledger {
    #[serde(default)]
    used: BTreeMap<String, u64>,
}

/// One variant weighed for eviction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub tag: String,
    pub last_used_ms: u64,
}

/// The variant cache: resolution, building, and the eviction policy.
pub struct VariantCache {
    policy: RunnerImagePolicy,
    /// Serializes builds. Two jobs from the same PR would otherwise each build
    /// the same gigabyte-plus of browsers, and the loser would discard its own
    /// work on the tag race. Held across the whole build, so a second job for a
    /// *different* version waits rather than competing for the daemon.
    build_lock: Mutex<()>,
    ledger: Mutex<Ledger>,
    /// faber's app-support directory, where the ledger and faber's own recipe
    /// live. `None` when there is no `HOME` to put them in, in which case
    /// last-used stays in memory for this process's lifetime and only an operator
    /// recipe can be built from.
    root: Option<PathBuf>,
    /// The disk thresholds builds and the janitor's variant tier act on, from
    /// `cfg.host` at construction ([`VariantDisk`]). `ensure` refuses to *start*
    /// a build below `build_gib`; the janitor reads `margin_gib` through
    /// [`Self::pressure_tier`]. All zero leaves both off.
    disk: VariantDisk,
    /// This host's `[needs.<tool>]` allowlist (ADR-0010 §4) — the operator's
    /// ceiling on what CI may cause to be built here. **Empty means no ceiling
    /// configured, not "nothing permitted"**: an operator upgrading into this must
    /// keep the healing they already had. Once any entry exists the map is
    /// authoritative and an unlisted tool fails closed. Applied in [`Self::ensure`]
    /// beside the disk floor, because both are pre-build refusals and a build is
    /// the only thing either of them is about.
    needs: HashMap<String, ToolNeeds>,
    /// Builds that failed, by tag: when, and why. Every queued job pinning the
    /// version calls [`Self::ensure`], and each would otherwise wait its turn on
    /// `build_lock` to fail the same build again, one after another.
    failed: std::sync::Mutex<HashMap<String, (std::time::Instant, String)>>,
}

/// How long a failed build of a tag is not tried again. Its jobs run on the
/// base image meanwhile, as on any failure ([`VariantCache::ensure`]).
pub const BUILD_FAILURE_HOLD: Duration = Duration::from_secs(600);

/// Why `tag`'s build is held back after an earlier `failure` (when, and its
/// cause), or `None` with none or once the hold has passed. Pure.
fn held_back(
    tag: &str,
    failure: Option<&(std::time::Instant, String)>,
    now: std::time::Instant,
    hold: Duration,
) -> Option<String> {
    let (at, cause) = failure?;
    let ago = now.saturating_duration_since(*at);
    (ago < hold).then(|| {
        format!(
            "building {tag} failed {}s ago, so it is not tried again for {}s: {cause}",
            ago.as_secs(),
            hold.saturating_sub(ago).as_secs()
        )
    })
}

impl VariantCache {
    /// Attach this host's `[needs]` allowlist (ADR-0010 §4).
    pub fn with_needs(mut self, needs: HashMap<String, ToolNeeds>) -> Self {
        self.needs = needs;
        self
    }

    /// Open the cache, reading whatever last-used ledger survives from before.
    ///
    /// `disk` comes from `cfg.host` ([`VariantDisk::new`]), passed in rather than
    /// read from a stored `HostPolicy` because the cache has no other use for one.
    pub fn new(policy: RunnerImagePolicy, disk: VariantDisk) -> Self {
        Self {
            disk,
            ..Self::rooted(policy, default_root())
        }
    }

    /// Which tier a disk-pressure pass that measured `free_mib` evicts at
    /// ([`VariantDisk::pressure_tier`]). Asked of the cache so the janitor and
    /// the build gate read one set of thresholds.
    pub fn pressure_tier(&self, free_mib: Option<u64>) -> EvictTier {
        self.disk.pressure_tier(free_mib)
    }

    /// Open the cache against an explicit app-support root.
    ///
    /// The seam a live test needs: exercising a real build against the real daemon
    /// must not write into the running daemon's ledger or overwrite the recipe it
    /// would use. Production always goes through [`VariantCache::new`], which is
    /// also the only path that sets real disk thresholds — a cache opened here
    /// starts with the gate off (`0`), since a live exercise of the build path
    /// has no `HostPolicy` to mirror and no business inventing one.
    pub fn rooted(policy: RunnerImagePolicy, root: Option<PathBuf>) -> Self {
        let ledger = root
            .as_ref()
            .map(|r| r.join(LEDGER_FILE))
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        Self {
            policy,
            build_lock: Mutex::new(()),
            ledger: Mutex::new(ledger),
            root,
            disk: VariantDisk::new(0, None),
            needs: HashMap::new(),
            failed: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Where the last-used ledger is written.
    fn ledger_path(&self) -> Option<PathBuf> {
        self.root.as_ref().map(|r| r.join(LEDGER_FILE))
    }

    /// Whether per-job image resolution runs at all. Off is the default, and with
    /// it off nothing in this module is ever reached.
    pub fn enabled(&self) -> bool {
        self.policy.build.enabled()
    }

    /// The image a job pinning `version` should run on, building it if this host
    /// doesn't have it yet.
    ///
    /// Errors are the caller's cue to run the base image and let ADR-0001 surface
    /// the mismatch: a variant is an optimisation, and an optimisation must never
    /// turn a job that would have run into one that cannot.
    pub async fn ensure(&self, base: &str, version: &str) -> anyhow::Result<Variant> {
        // #113: `version` is what a repo's lockfile pins, and it becomes a tag, a
        // label and `${PW_VERSION}` in the recipe's `npx` call. Refused before any
        // of those, on the same check the drift read applies, and reported by
        // length: the value is the repo's choice.
        anyhow::ensure!(
            crate::untrusted::release_version(version),
            "the pinned version is not a release version ({} bytes) — refusing to \
             build a variant for it",
            version.len()
        );
        let base_id = docker::image_id(base)
            .await
            .with_context(|| format!("resolving the id of base image {base}"))?;
        let tag = variant_tag(&base_id, version);

        if docker::image_exists(&tag).await {
            self.touch(&tag).await;
            return Ok(Variant { tag, built: false });
        }

        let _lock = self.build_lock.lock().await;
        // Re-check under the lock: whoever held it may well have been building
        // exactly this tag, and rebuilding it would be a wasted gigabyte.
        if docker::image_exists(&tag).await {
            self.touch(&tag).await;
            return Ok(Variant { tag, built: false });
        }
        if let Some(why) = held_back(
            &tag,
            self.failed.lock().unwrap().get(&tag),
            std::time::Instant::now(),
            BUILD_FAILURE_HOLD,
        ) {
            anyhow::bail!(why);
        }

        // ADR-0007: faber is this host's Docker manager, and a variant build is
        // the one disk consumer *faber itself* starts that can put gigabytes down
        // in a single shot. It is gated on what it costs, not on the reclamation
        // floor (ADR-0008, amendment of 2026-10-30): a host can sit under that
        // floor for days with ample room for a 2 GiB build, and refusing there
        // while the janitor evicts the variant is how jobs needing one were
        // refused forever. `VariantDisk` charges the build and keeps a margin
        // behind it; an unreadable `df` is not evidence of pressure, and where
        // Docker runs in faber's Linux VM the tighter disk decides (#410).
        let free = host::docker_disk_free().await.map(|r| r.free_mib);
        if let Some(why) = self.disk.build_refusal(free) {
            anyhow::bail!(why);
        }

        // ADR-0010 §4: the operator's ceiling on what CI may cause to be built
        // here. This gates the *build*, not the label, because the build is
        // commissioned by the repo's lockfile — which a pull request may
        // legitimately edit — so validating a declared need against the lockfile
        // could never have bounded it. Placed on `ensure` rather than at a call
        // site so every path that could start a build passes it.
        //
        // An empty `[needs]` map is "no ceiling configured", NOT "nothing
        // permitted": an operator upgrading into this keeps the healing they
        // already had, which is ADR-0010 §5's promise that adopting nothing
        // changes nothing. Once any entry exists the map is authoritative and an
        // unlisted tool fails closed.
        if !self.needs.is_empty() {
            match needs_allow_in(&self.needs, PLAYWRIGHT_TOOL, version) {
                NeedsVerdict::Allowed => {}
                NeedsVerdict::UnknownTool => anyhow::bail!(
                    "`{PLAYWRIGHT_TOOL}` is not in this host's [needs] allowlist — \
                     refusing to build {version} (ADR-0010 §4)"
                ),
                NeedsVerdict::OutOfRange(configured) => anyhow::bail!(
                    "{PLAYWRIGHT_TOOL} {version} is outside this host's allowed \
                     versions (`{configured}`) — refusing to build it (ADR-0010 §4)"
                ),
            }
        }

        // Read once: the build is fed these bytes, never the path again.
        let (recipe, context) = self.recipe()?;
        // An error, never a silent `root`: see `docker::image_user` (#105).
        let base_user = docker::image_user(base)
            .await
            .context("reading the user the base image runs as")?;
        info!("image: building {tag} — {base} + playwright {version}");
        let started = std::time::Instant::now();
        let built = docker::build_variant(
            &VariantBuild {
                recipe: &recipe,
                context: &context,
                base,
                base_id: &base_id,
                base_user: &base_user,
                version,
                tag: &tag,
            },
            Duration::from_secs(self.policy.build_timeout_secs),
        )
        .await
        .with_context(|| format!("building runner-image variant {tag}"));
        if let Err(e) = &built {
            self.failed
                .lock()
                .unwrap()
                .insert(tag.clone(), (std::time::Instant::now(), format!("{e:#}")));
        }
        built?;
        self.failed.lock().unwrap().remove(&tag);
        info!("image: built {tag} in {:?}", started.elapsed());
        self.touch(&tag).await;
        Ok(Variant { tag, built: true })
    }

    /// The recipe to build with, as bytes, and the build context: the
    /// operator's, or faber's own template written out fresh.
    ///
    /// faber's template is rewritten on every build rather than materialized once,
    /// so an upgrade that improves it takes effect instead of leaving a host on
    /// whatever shipped the day it first built something. An operator `dockerfile`
    /// is never touched — that is the whole point of pointing at one — and is
    /// built from only when root alone could have written it and its build
    /// context ([`judge_recipe`], ADR-0028): otherwise the build is refused.
    /// Its bytes are read once, after that check, and the build is fed those.
    fn recipe(&self) -> anyhow::Result<(Vec<u8>, PathBuf)> {
        if let Some(path) = self.policy.dockerfile.as_deref().map(str::trim) {
            if !path.is_empty() {
                let (file, context) = judge_recipe(Path::new(path)).map_err(|e| {
                    anyhow::anyhow!(
                        "refusing to build from the recipe at {path}: {e} (ADR-0028: move it \
                         under root, in a directory of its own, e.g. `sudo install -o root -m 0644 {path} \
                         /etc/faber-recipe/`)"
                    )
                })?;
                let bytes =
                    std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
                return Ok((bytes, context));
            }
        }
        let dir = self
            .root
            .as_ref()
            .context("no HOME to write faber's variant recipe into")?
            .join(RECIPE_DIR);
        std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("Dockerfile");
        std::fs::write(&path, VARIANT_DOCKERFILE)
            .with_context(|| format!("writing {}", path.display()))?;
        Ok((VARIANT_DOCKERFILE.as_bytes().to_vec(), dir))
    }

    /// Mark a variant as used now, and persist that. Best-effort: failing to
    /// record it costs the TTL its accuracy, never a job.
    async fn touch(&self, tag: &str) {
        let snapshot = {
            let mut ledger = self.ledger.lock().await;
            ledger.used.insert(tag.to_string(), now_ms());
            ledger.clone()
        };
        self.save(&snapshot);
    }

    fn save(&self, ledger: &Ledger) {
        let Some(path) = self.ledger_path() else {
            return;
        };
        let write = || -> anyhow::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&path, serde_json::to_string_pretty(ledger)?)?;
            Ok(())
        };
        if let Err(e) = write() {
            warn!("image: recording variant use in {}: {e}", path.display());
        }
    }

    /// Reconcile the ledger against the daemon's actual image list and return the
    /// variants that should go.
    ///
    /// Split from the deletion so the janitor can find out whether it has any work
    /// *before* deciding to shut admission for it: on a host with a stable version
    /// set this is two metadata calls and nothing else. [`EvictTier::Squeezed`]
    /// takes everything no job holds, since a variant can be rebuilt and the
    /// wedged daemon a full disk produces cannot be undone.
    ///
    /// `claimed` is the images live claims have been cleared to run on: a parked
    /// claim holding one has no container yet, so the daemon's in-use answer
    /// cannot see it, and evicting it would fail a job faber already resolved.
    /// Shielded in every tier, like the in-use set.
    pub async fn evictable(&self, tier: EvictTier, claimed: &HashSet<String>) -> Vec<String> {
        let tags = match docker::variant_tags().await {
            Ok(t) => t,
            Err(e) => {
                warn!("image: listing variants: {e}");
                return Vec::new();
            }
        };
        self.evictable_from(tags, docker::images_in_use().await, claimed, tier)
            .await
    }

    /// [`Self::evictable`] given the variants the daemon listed, its answer on
    /// which images running containers use, and the images live claims hold.
    ///
    /// No in-use set, no eviction (#105): a failed listing shields nothing, so it
    /// must not read as "no container uses any variant".
    async fn evictable_from(
        &self,
        tags: Vec<String>,
        in_use: anyhow::Result<HashSet<String>>,
        claimed: &HashSet<String>,
        tier: EvictTier,
    ) -> Vec<String> {
        let in_use: HashSet<String> = match in_use {
            Ok(in_use) => in_use.union(claimed).cloned().collect(),
            Err(e) => {
                warn!("image: listing the images running containers use: {e} — evicting nothing");
                return Vec::new();
            }
        };
        let present: HashSet<&str> = tags.iter().map(String::as_str).collect();

        let now = now_ms();
        let candidates: Vec<Candidate> = {
            let mut ledger = self.ledger.lock().await;
            // Drop what the daemon no longer has (removed by hand, or by a
            // previous pass), and adopt what it has that faber never recorded —
            // dated now, so a variant built out-of-band gets a full TTL rather
            // than being evicted the moment it appears.
            ledger.used.retain(|tag, _| present.contains(tag.as_str()));
            for tag in &tags {
                ledger.used.entry(tag.clone()).or_insert(now);
            }
            let snapshot = ledger.clone();
            drop(ledger);
            self.save(&snapshot);
            snapshot
                .used
                .into_iter()
                .map(|(tag, last_used_ms)| Candidate { tag, last_used_ms })
                .collect()
        };

        // Under the floor, `keep = 0` makes every rank evictable on its own, so no
        // TTL is needed to reach the rest: what survives is only what the grace and
        // the in-use set shield.
        let aggressive = tier == EvictTier::Squeezed;
        let keep = if aggressive { 0 } else { self.policy.keep };
        let ttl = (!aggressive && self.policy.idle_ttl_hours > 0)
            .then(|| self.policy.idle_ttl_hours.saturating_mul(3_600_000));
        evict_list(&candidates, keep, ttl, RESOLVE_GRACE_MS, now, &in_use)
    }

    /// Remove the variants `evictable` picked. Returns what actually went, so the
    /// caller can log a reclamation that happened rather than one it intended.
    pub async fn evict(&self, tags: &[String]) -> Vec<String> {
        let mut gone = Vec::new();
        for tag in tags {
            match docker::remove_image(tag).await {
                Ok(true) => gone.push(tag.clone()),
                // Refused because something is using it after all, or already
                // removed: either way not this pass's business.
                Ok(false) => {}
                Err(e) => warn!("image: removing variant {tag}: {e}"),
            }
        }
        if !gone.is_empty() {
            let snapshot = {
                let mut ledger = self.ledger.lock().await;
                ledger.used.retain(|tag, _| !gone.contains(tag));
                ledger.clone()
            };
            self.save(&snapshot);
        }
        gone
    }
}

/// Which variants to evict, as a pure decision over the weighed candidates.
///
/// Two independent *reasons* to evict — ranked below `keep`, or idle past the TTL
/// — and two *shields* that override both however hard the host is squeezed:
///
/// * a variant a running container is using (`docker rmi` would refuse it anyway;
///   knowing first is what keeps that from being logged as a failure every pass);
/// * a variant handed to a job within `grace_ms`, which has no container yet and
///   so is invisible to the first shield (see [`RESOLVE_GRACE_MS`]).
///
/// The TTL is deliberately *not* subordinate to `keep`: a version nobody has
/// needed in a week is not hot, and holding it merely because it is one of few is
/// how a cache becomes a leak.
pub fn evict_list(
    candidates: &[Candidate],
    keep: usize,
    idle_ttl_ms: Option<u64>,
    grace_ms: u64,
    now: u64,
    in_use: &HashSet<String>,
) -> Vec<String> {
    let mut ranked: Vec<&Candidate> = candidates.iter().collect();
    // Most recently used first; tag as the tiebreak so the order is total and a
    // pass is reproducible.
    ranked.sort_by(|a, b| {
        b.last_used_ms
            .cmp(&a.last_used_ms)
            .then_with(|| a.tag.cmp(&b.tag))
    });
    ranked
        .into_iter()
        .enumerate()
        .filter(|(rank, c)| {
            let idle_for = now.saturating_sub(c.last_used_ms);
            if in_use.contains(&c.tag) || idle_for < grace_ms {
                return false;
            }
            let beyond_keep = *rank >= keep;
            let stale = idle_ttl_ms.is_some_and(|ttl| idle_for >= ttl);
            beyond_keep || stale
        })
        .map(|(_, c)| c.tag.clone())
        .collect()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(tag: &str, last_used_ms: u64) -> Candidate {
        Candidate {
            tag: tag.to_string(),
            last_used_ms,
        }
    }

    const HOUR: u64 = 3_600_000;
    /// The real grace. Every candidate in these tests is hours old, so it only
    /// matters where a test is about the grace itself.
    const GRACE: u64 = RESOLVE_GRACE_MS;

    // --- whose recipe it is (ADR-0028, #238) ------------------------------

    const DIR: u32 = 0o040_000;

    fn pf(path: &str, uid: u32, gid: u32, mode: u32) -> PathFacts {
        PathFacts {
            path: PathBuf::from(path),
            uid,
            gid,
            mode,
        }
    }

    const RECIPE: &str = "/etc/faber/variant.Dockerfile";

    /// The recipe and every directory above it, all root's.
    fn recipe_chain() -> Vec<PathFacts> {
        vec![
            pf(RECIPE, 0, 0, S_IFREG | 0o644),
            pf("/etc/faber", 0, 0, DIR | 0o755),
            pf("/etc", 0, 0, DIR | 0o755),
            pf("/", 0, 0, DIR | 0o755),
        ]
    }

    #[test]
    fn a_root_only_recipe_is_trusted() {
        assert_eq!(recipe_verdict(&recipe_chain(), &[]), Ok(()));
        // With root's own files beside it in the build context.
        let context = [
            pf("/etc/faber/files", 0, 0, DIR | 0o755),
            pf("/etc/faber/files/ca.pem", 0, 0, S_IFREG | 0o644),
            // A symlink's mode means nothing; on Linux it is always 0777.
            pf("/etc/faber/link", 0, 0, S_IFLNK | 0o777),
        ];
        assert_eq!(recipe_verdict(&recipe_chain(), &context), Ok(()));
    }

    #[test]
    fn a_recipe_the_operator_owns_is_refused() {
        let mut chain = recipe_chain();
        chain[0].uid = 501;
        let e = recipe_verdict(&chain, &[]).unwrap_err();
        assert!(e.contains("uid 501") && e.contains(RECIPE), "{e}");
        // Or a directory above it.
        let mut chain = recipe_chain();
        chain[1].uid = 501;
        let e = recipe_verdict(&chain, &[]).unwrap_err();
        assert!(e.starts_with("/etc/faber ") && e.contains("uid 501"), "{e}");
    }

    #[test]
    fn a_group_or_world_writable_ancestor_is_refused() {
        let mut chain = recipe_chain();
        chain[2].mode = DIR | 0o775;
        let e = recipe_verdict(&chain, &[]).unwrap_err();
        assert!(e.starts_with("/etc ") && e.contains("writable"), "{e}");
        let mut chain = recipe_chain();
        chain[1].mode = DIR | 0o757;
        assert!(recipe_verdict(&chain, &[]).is_err());
        // The recipe itself, group-writable.
        let mut chain = recipe_chain();
        chain[0].mode = S_IFREG | 0o664;
        assert!(recipe_verdict(&chain, &[]).is_err());
    }

    #[test]
    fn a_recipe_in_a_sticky_world_writable_directory_is_refused() {
        // The recipe's directory is the build context: anyone could add an
        // entry there between the check and the build, sticky bit or not.
        let chain = vec![
            pf("/private/tmp/variant.Dockerfile", 0, 0, S_IFREG | 0o644),
            pf("/private/tmp", 0, 0, DIR | 0o1777),
            pf("/private", 0, 0, DIR | 0o755),
            pf("/", 0, 0, DIR | 0o755),
        ];
        let e = recipe_verdict(&chain, &[]).unwrap_err();
        assert!(
            e.starts_with("/private/tmp ") && e.contains("sticky or not"),
            "{e}"
        );
        // Above the context, the sticky exception still holds.
        let chain = vec![
            pf(
                "/private/tmp/faber/variant.Dockerfile",
                0,
                0,
                S_IFREG | 0o644,
            ),
            pf("/private/tmp/faber", 0, 0, DIR | 0o755),
            pf("/private/tmp", 0, 0, DIR | 0o1777),
            pf("/private", 0, 0, DIR | 0o755),
            pf("/", 0, 0, DIR | 0o755),
        ];
        assert_eq!(recipe_verdict(&chain, &[]), Ok(()));
    }

    #[test]
    fn a_sticky_directory_inside_the_build_context_is_refused() {
        let context = [
            pf("/etc/faber/drop", 0, 0, DIR | 0o1777),
            pf("/etc/faber/drop/a.txt", 0, 0, S_IFREG | 0o644),
        ];
        let e = recipe_verdict(&recipe_chain(), &context).unwrap_err();
        assert!(
            e.starts_with("/etc/faber/drop ") && e.contains("sticky or not"),
            "{e}"
        );
    }

    #[test]
    fn the_build_context_is_held_to_the_same_rule() {
        for entry in [
            pf("/etc/faber/files/ca.pem", 501, 20, S_IFREG | 0o644),
            pf("/etc/faber/files", 0, 20, DIR | 0o775),
            pf("/etc/faber/files/ca.pem", 0, 0, S_IFREG | 0o666),
            pf("/etc/faber/link", 501, 20, S_IFLNK | 0o755),
        ] {
            let e = recipe_verdict(&recipe_chain(), std::slice::from_ref(&entry)).unwrap_err();
            assert!(e.contains("build context"), "{entry:?}: {e}");
        }
    }

    #[test]
    fn a_recipe_that_is_no_regular_file_is_refused() {
        let mut chain = recipe_chain();
        chain[0].mode = DIR | 0o755;
        let e = recipe_verdict(&chain, &[]).unwrap_err();
        assert!(e.contains("not a regular file"), "{e}");
        assert!(recipe_verdict(&[], &[]).is_err());
    }

    /// Without an operator recipe, or with building off, there is nothing to
    /// judge; a recipe that is not there is a problem.
    #[test]
    fn only_a_recipe_in_force_is_judged() {
        use crate::config::ImageBuildMode;
        let policy = |build: ImageBuildMode, recipe: Option<&str>| RunnerImagePolicy {
            build,
            dockerfile: recipe.map(String::from),
            ..RunnerImagePolicy::default()
        };
        let missing = "/nonexistent/faber-test/variant.Dockerfile";
        assert_eq!(
            recipe_problem(&policy(ImageBuildMode::OnDemand, None)),
            None
        );
        assert_eq!(
            recipe_problem(&policy(ImageBuildMode::OnDemand, Some("  "))),
            None
        );
        assert_eq!(
            recipe_problem(&policy(ImageBuildMode::Off, Some(missing))),
            None
        );
        let e = recipe_problem(&policy(ImageBuildMode::OnDemand, Some(missing))).unwrap();
        assert!(
            e.starts_with("runner_image.dockerfile ") && e.contains("sudo install"),
            "{e}"
        );
    }

    #[test]
    fn a_variant_tag_names_its_base_and_its_version() {
        let tag = variant_tag("sha256:0123456789abcdefdeadbeef", "1.61.1");
        assert_eq!(tag, "faber-runner:0123456789ab-pw-1.61.1");
    }

    #[test]
    fn rebuilding_the_base_changes_every_variant_tag() {
        // The whole reason the tag is keyed on the id: a variant built against a
        // superseded base must fall out of use, not keep serving jobs.
        let before = variant_tag("sha256:aaaaaaaaaaaabbbb", "1.61.1");
        let after = variant_tag("sha256:ccccccccccccdddd", "1.61.1");
        assert_ne!(before, after);
    }

    #[test]
    fn an_exotic_version_still_produces_a_usable_tag() {
        // A build-metadata pin must not produce a tag docker rejects — the build
        // would fail for a reason that has nothing to do with the build.
        let tag = variant_tag("sha256:aaaaaaaaaaaa", "1.61.0-alpha+build.1");
        assert_eq!(tag, "faber-runner:aaaaaaaaaaaa-pw-1.61.0-alpha-build.1");
    }

    #[test]
    fn the_hottest_variants_are_kept() {
        let now = 100 * HOUR;
        let candidates = [
            c("a", now - HOUR),
            c("b", now - 2 * HOUR),
            c("c", now - 3 * HOUR),
            c("d", now - 4 * HOUR),
        ];
        let gone = evict_list(&candidates, 2, None, GRACE, now, &HashSet::new());
        assert_eq!(gone, vec!["c".to_string(), "d".to_string()]);
    }

    #[test]
    fn a_cold_variant_goes_even_when_it_is_one_of_few() {
        // `keep` is not a floor. A version nobody has needed in a week is not hot,
        // and holding it because it is one of two is how a cache becomes a leak.
        let now = 1000 * HOUR;
        let candidates = [c("hot", now - HOUR), c("cold", now - 200 * HOUR)];
        let gone = evict_list(
            &candidates,
            5,
            Some(168 * HOUR),
            GRACE,
            now,
            &HashSet::new(),
        );
        assert_eq!(gone, vec!["cold".to_string()]);
    }

    #[test]
    fn a_running_job_s_image_is_never_evicted() {
        let now = 1000 * HOUR;
        let candidates = [c("busy", now - 500 * HOUR), c("idle", now - 500 * HOUR)];
        let in_use: HashSet<String> = ["busy".to_string()].into_iter().collect();
        let gone = evict_list(&candidates, 0, Some(HOUR), GRACE, now, &in_use);
        assert_eq!(gone, vec!["idle".to_string()]);
    }

    #[test]
    fn a_variant_just_handed_to_a_job_is_shielded() {
        // The window this exists for: `ensure` returned the tag and the job then
        // parked in the memory-budget queue, so no container references it yet and
        // nothing the daemon can see says it is in use. Evicting here would fail a
        // job faber had *just* built an image for.
        let now = 1000 * HOUR;
        let candidates = [c("fresh", now - 60_000), c("old", now - 50 * HOUR)];
        let gone = evict_list(&candidates, 0, None, GRACE, now, &HashSet::new());
        assert_eq!(gone, vec!["old".to_string()]);
    }

    #[test]
    fn a_zero_ttl_leaves_only_the_rank_rule() {
        // `idle_ttl_hours = 0` disables the TTL; it must not read as "everything
        // is stale", which would evict the whole cache on the next sweep.
        let now = 1000 * HOUR;
        let candidates = [c("a", now - 900 * HOUR), c("b", now - 800 * HOUR)];
        assert!(evict_list(&candidates, 3, None, GRACE, now, &HashSet::new()).is_empty());
    }

    #[test]
    fn the_disk_floor_tier_takes_everything_but_the_live_and_the_just_built() {
        let now = 1000 * HOUR;
        let candidates = [
            c("running", now - HOUR),
            // Resolved a minute ago: its job is parked in admission and has no
            // container yet, so nothing the daemon can see says it is in use.
            c("just-resolved", now - 60_000),
            c("idle", now - HOUR),
        ];
        let in_use: HashSet<String> = ["running".to_string()].into_iter().collect();
        // What `EvictTier::Squeezed` passes: no rank survives, so only the two shields
        // decide.
        let gone = evict_list(&candidates, 0, None, RESOLVE_GRACE_MS, now, &in_use);
        assert_eq!(gone, vec!["idle".to_string()]);
    }

    /// #105: with no answer on which images running containers use, nothing is
    /// evicted, even a variant every other rule would let go. A failed listing
    /// shields nothing, so it must never read as "no container uses any variant".
    #[tokio::test]
    async fn no_answer_on_the_images_in_use_evicts_nothing() {
        let cache = VariantCache::rooted(RunnerImagePolicy::default(), None);
        let tag = "faber-runner:0123456789ab-pw-1.61.1".to_string();
        // Idle since the epoch: past the grace and any TTL.
        cache.ledger.lock().await.used.insert(tag.clone(), 0);
        let listed = || vec![tag.clone()];
        let unanswered = Err(anyhow::anyhow!("docker did not answer"));
        assert!(cache
            .evictable_from(listed(), unanswered, &HashSet::new(), EvictTier::Squeezed)
            .await
            .is_empty());
        assert_eq!(
            cache
                .evictable_from(
                    listed(),
                    Ok(HashSet::new()),
                    &HashSet::new(),
                    EvictTier::Squeezed
                )
                .await,
            vec![tag.clone()],
            "the control: answered, it goes"
        );
        assert!(
            cache
                .evictable_from(
                    listed(),
                    Ok(HashSet::from([tag.clone()])),
                    &HashSet::new(),
                    EvictTier::Squeezed
                )
                .await
                .is_empty(),
            "and one a running container uses stays"
        );
    }

    const GIB: u64 = 1024;

    #[test]
    fn a_build_is_charged_its_cost_on_top_of_the_margin() {
        // Reclamation floor 25, no admission floor stated: the margin is the
        // 10 GiB minimum and a build needs 14 free.
        let disk = VariantDisk::new(25, None);
        assert_eq!(
            disk,
            VariantDisk {
                margin_gib: BUILD_MARGIN_MIN_GIB,
                build_gib: BUILD_MARGIN_MIN_GIB + BUILD_CHARGE_GIB,
            }
        );
        assert!(disk.build_refusal(Some(14 * GIB - 1)).is_some());
        assert!(disk.build_refusal(Some(14 * GIB)).is_none());
        assert!(disk.build_refusal(Some(40 * GIB)).is_none());
    }

    #[test]
    fn a_stated_admission_floor_raises_the_margin() {
        // A build must not take the disk below where admission stops.
        let disk = VariantDisk::new(25, Some(15));
        assert_eq!(disk.margin_gib, 15);
        assert_eq!(disk.build_gib, 15 + BUILD_CHARGE_GIB);
        assert!(disk.build_refusal(Some(18 * GIB)).is_some());
    }

    #[test]
    fn the_build_gate_is_never_stricter_than_the_reclamation_floor() {
        // The floor the gate used before this rule is its ceiling: a host that
        // could build before still can.
        let disk = VariantDisk::new(25, Some(24));
        assert_eq!(disk.build_gib, 25);
        assert!(disk.build_refusal(Some(25 * GIB)).is_none());
        let low = VariantDisk::new(5, None);
        assert_eq!(
            low,
            VariantDisk {
                margin_gib: 5,
                build_gib: 5
            }
        );
        assert!(low.build_refusal(Some(5 * GIB)).is_none());
    }

    #[test]
    fn a_zero_floor_never_blocks_a_build() {
        // `0` is the operator's off switch for the whole disk-floor mechanism
        // (see `host::below_disk_floor`); it must read as "no policy", not as
        // "always below zero GiB free".
        let off = VariantDisk::new(0, Some(10));
        assert!(off.build_refusal(Some(0)).is_none());
        assert_eq!(off.pressure_tier(Some(0)), EvictTier::Cold);
    }

    #[test]
    fn an_unreadable_free_space_reading_never_blocks_a_build() {
        // A failed `df` is not evidence of pressure. Guessing pressure from a
        // read failure would trade a diagnostic gap for exactly the outcome
        // ADR-0007 exists to prevent — refusing a job a healthy host could have
        // run, on no evidence at all.
        assert!(VariantDisk::new(25, None).build_refusal(None).is_none());
    }

    #[test]
    fn eviction_squeezes_only_below_the_margin() {
        let disk = VariantDisk::new(25, None);
        assert_eq!(disk.pressure_tier(Some(10 * GIB - 1)), EvictTier::Squeezed);
        assert_eq!(disk.pressure_tier(Some(10 * GIB)), EvictTier::Cold);
        assert_eq!(disk.pressure_tier(Some(22 * GIB)), EvictTier::Cold);
    }

    #[test]
    fn a_squeezed_eviction_cannot_pay_for_its_own_rebuild() {
        // The band between the margin and the build threshold is one build's
        // charge, so the two rules cannot chase each other: a pass that evicts
        // just under the margin, freeing a variant of about 2 GiB, leaves less
        // than a rebuild needs; a build that starts at the threshold and costs its
        // full charge ends at the margin, which does not squeeze.
        let disk = VariantDisk::new(25, None);
        let just_under = disk.margin_gib * GIB - 1;
        let variant_mib = 2048;
        assert_eq!(disk.pressure_tier(Some(just_under)), EvictTier::Squeezed);
        assert!(disk.build_refusal(Some(just_under + variant_mib)).is_some());
        let after_build = disk.build_gib * GIB - BUILD_CHARGE_GIB * GIB;
        assert_eq!(disk.pressure_tier(Some(after_build)), EvictTier::Cold);
    }

    /// The deadlock behind ADR-0008's amendment of 2026-10-30, on illustrative
    /// numbers. Say the Linux VM's disk sat at 15–21 GiB free under the default
    /// 25 GiB reclamation floor. Every disk-pressure pass evicted the one variant
    /// a watched repo's jobs needed (freeing a couple of GiB), and every rebuild
    /// was refused for being below that same floor, so jobs declaring
    /// `faber.playwright-<v>` were refused a runner until free space happened to
    /// clear 25 GiB.
    #[tokio::test]
    async fn the_variant_jobs_need_survives_a_floor_it_cannot_lift() {
        let disk = VariantDisk::new(25, None);
        let cache = VariantCache::rooted(RunnerImagePolicy::default(), None);
        let tag = "faber-runner:0123456789ab-pw-1.64.1".to_string();
        // Last used an hour ago: past the resolve grace, well inside the TTL.
        let hour_ago = now_ms() - 3_600_000;
        cache.ledger.lock().await.used.insert(tag.clone(), hour_ago);

        for free_mib in [15 * GIB, 18 * GIB, 21 * GIB] {
            let tier = disk.pressure_tier(Some(free_mib));
            assert!(
                cache
                    .evictable_from(vec![tag.clone()], Ok(HashSet::new()), &HashSet::new(), tier)
                    .await
                    .is_empty(),
                "a pass at {free_mib} MiB free evicted the variant its jobs need"
            );
            assert!(
                disk.build_refusal(Some(free_mib)).is_none(),
                "a rebuild at {free_mib} MiB free was refused"
            );
        }
    }

    /// A claim parked in admission after resolving a variant has no container,
    /// so nothing the daemon reports shields its image — even long after the
    /// resolve grace. The images live claims hold are shielded in every tier.
    #[tokio::test]
    async fn a_variant_a_parked_claim_holds_is_never_evicted() {
        let cache = VariantCache::rooted(RunnerImagePolicy::default(), None);
        let tag = "faber-runner:0123456789ab-pw-1.64.1".to_string();
        cache.ledger.lock().await.used.insert(tag.clone(), 0);
        let claimed = HashSet::from([tag.clone()]);
        for tier in [EvictTier::Cold, EvictTier::Squeezed] {
            assert!(cache
                .evictable_from(vec![tag.clone()], Ok(HashSet::new()), &claimed, tier)
                .await
                .is_empty());
        }
        assert_eq!(
            cache
                .evictable_from(
                    vec![tag.clone()],
                    Ok(HashSet::new()),
                    &HashSet::new(),
                    EvictTier::Squeezed
                )
                .await,
            vec![tag],
            "the control: unclaimed and squeezed, it goes"
        );
    }

    #[test]
    fn faber_s_recipe_restores_the_base_user() {
        // A runner image that silently starts running as root is a different
        // image in the way that matters most.
        assert!(VARIANT_DOCKERFILE.contains("USER root"));
        assert!(VARIANT_DOCKERFILE.trim().ends_with("USER ${BASE_USER}"));
    }

    #[test]
    fn faber_s_recipe_keeps_the_base_s_browsers() {
        // `playwright install` prunes revisions the new version doesn't need. In a
        // layered image the pruned files stay in the parent layer, so the variant
        // pays for the base's browsers either way — pruning them only makes it pay
        // and not be able to use them. Measured on a real runner image: the same
        // size either way, serving one version with the GC on and two with it off.
        //
        // And JIT runners are not job-pinned, so this variant is routinely handed a
        // job pinning the base's version instead of its own. Pruning would fail that
        // job on a missing browser — a failure faber itself would have created.
        assert!(VARIANT_DOCKERFILE.contains("PLAYWRIGHT_SKIP_BROWSER_GC=1"));
    }

    #[test]
    fn faber_s_recipe_installs_system_dependencies() {
        // `--with-deps` is the whole reason this is a build and not a mounted
        // browser directory: a browser build needing a new system library must
        // not fail with a missing-library error.
        assert!(VARIANT_DOCKERFILE.contains("--with-deps"));
    }

    #[test]
    fn a_failed_build_is_held_back_until_the_hold_passes() {
        let at = std::time::Instant::now();
        let failure = (at, "docker build exited 125".to_string());
        let hold = BUILD_FAILURE_HOLD;
        let why = held_back(
            "faber-runner:x",
            Some(&failure),
            at + Duration::from_secs(5),
            hold,
        )
        .expect("held right after the failure");
        assert!(
            why.contains("faber-runner:x") && why.contains("exited 125"),
            "{why}"
        );
        assert!(held_back(
            "faber-runner:x",
            Some(&failure),
            at + hold - Duration::from_secs(1),
            hold
        )
        .is_some());
        assert_eq!(
            held_back("faber-runner:x", Some(&failure), at + hold, hold),
            None
        );
        assert_eq!(held_back("faber-runner:x", None, at, hold), None);
    }

    #[test]
    fn a_build_context_never_holds_faber_s_credentials() {
        // A build sends its context to Docker, the daemon CI containers run on;
        // the legacy builder sends every file in it, COPYed or not.
        let secrets = [
            PathBuf::from("/etc/faber"),
            PathBuf::from("/private/etc/faber"),
        ];
        for context in ["/etc/faber", "/etc", "/", "/private/etc/faber", "/private"] {
            assert!(
                context_holds_secrets(Path::new(context), &secrets).is_some(),
                "{context}"
            );
        }
        for context in [
            "/etc/faber-recipe",
            "/usr/local/share/recipe",
            "/etc/faberx",
        ] {
            assert_eq!(
                context_holds_secrets(Path::new(context), &secrets),
                None,
                "{context}"
            );
        }
    }
}
