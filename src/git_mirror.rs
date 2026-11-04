//! Jobs borrow git objects from a mirror only faber writes — ADR-0035.
//!
//! Every job's `actions/checkout` fetches objects the host fetched minutes
//! earlier for another job, and deletes them with its workspace. With
//! `[[profile]] git_mirror = true`, faber keeps one bare repository per watched
//! repo in a named Docker volume, fetches it from GitHub with its own
//! credentials, and mounts it **read-only** into that repo's runner containers at
//! [`MOUNT`], exporting [`ENV`]. A workflow step writes the path into the
//! workspace's `objects/info/alternates` before checkout, and the fetch then
//! transfers only what the mirror lacks (docs/git-mirror.md).
//!
//! What keeps it safe, in the order the ADR decides it:
//!
//! 1. **faber alone writes** (§1). A refresh is a short-lived container of the
//!    profile's runner image, the volume read-write, the credential a read-only
//!    `0600` file in a volume of its own ([`GitCredential`]), never an argument,
//!    a variable or a host file. Jobs get
//!    `:ro` ([`runner_args`]), so nothing a job does reaches the next job's
//!    checkout. Nor does anything planted in the volume steer a fetch that holds
//!    the credential: each script rewrites the repository's `config` and deletes
//!    its `hooks/` before git runs, git reads no other config and speaks only
//!    https, and the credential helper answers only `https://github.com`
//!    ([`credential_helper`]).
//! 2. **No fork code, ever** (§1, §6). The refspec is explicit ([`refspecs`]):
//!    branches and tags, and the heads of open PRs whose head repo *is* the base
//!    repo ([`same_repo_prs`]), read from the REST API because a ref name cannot
//!    tell them apart. Never `--mirror`, never `refs/pull/*/merge`.
//! 3. **Malformed objects are refused at the door** (§1): `fetch.fsckObjects`.
//! 4. **One writer at a time per repo** (§1, §4). Refresh, repack, prune and
//!    delete share one lock; requests that arrive during a refresh coalesce into
//!    one follow-up ([`Coalesce`]).
//! 5. **Nothing a running job borrowed is deleted** (§4). Routine repacks keep
//!    unreachable objects; `gc --prune=now` and deleting the volume wait until no
//!    job holds a [`Lease`] and no container mounts the volume.
//! 6. **Best-effort** (§2). A mirror that is not there, not refreshed yet, or
//!    being pruned is simply not mounted, and the job checks out as before.
//!
//! Absent or `false` on every profile, nothing here runs except the janitor's
//! one volume listing, which finds nothing on a host that never turned it on.
//! It is off, too, wherever a job could be root on the Docker engine the
//! credential would sit on: on a host that grants dind, or whose engine ever did
//! ([`crate::mirror_beside_dind`]). Boot then never calls [`Mirrors::configure`].

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{OwnedRwLockReadGuard, RwLock};
use tracing::{debug, info, warn};

use crate::docker::{
    self, CredentialFill, CredentialVolume, KIND_GIT_AUTH, KIND_GIT_MIRROR, KIND_GIT_MIRROR_WORK,
    LABEL_KIND, LABEL_OWNED, LABEL_REPO,
};
use crate::github::{Github, OpenPull};
use crate::untrusted;

/// Where a runner container sees its repo's mirror: a sibling of
/// [`docker::CACHE_MOUNT`], outside `_work` (ADR-0035 §2).
pub const MOUNT: &str = "/runner/_git";

/// The variable that says the mirror is mounted, set only when it is. It is the
/// contract the workflow step keys off, as `FABER_CACHE_DIR` is for the cache.
pub const ENV: &str = "FABER_GIT_MIRROR";

/// How often every mirror is refreshed regardless of events (ADR-0035 §1).
pub const REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);

/// How long after the last prune (or the first refresh this process saw) the
/// next `gc --prune=now` may run: at most daily (ADR-0035 §4).
pub const PRUNE_INTERVAL_MS: u64 = 24 * 60 * 60 * 1000;

/// Consecutive successful watched-repo listings a repo must be missing from
/// before the janitor deletes its mirror, and [`UNSERVED_GRACE_MS`] is the least
/// time those misses must span. Both, because each alone is cheap to fool: a run
/// of pressure passes can fit many listings into a minute, and one listing an
/// hour ago says nothing about now.
///
/// The discovery listing is GitHub search, which is intermittently short: in
/// October 2026 one short answer deleted most of a host's mirrors, each a cold
/// clone to rebuild. [`crate::github::Github::repos_with_topic`] now refuses a
/// page that admits it is partial, but a short page that claims to be whole
/// (`total_count: 0`) cannot be told from every repo leaving the topic. This is
/// the same idea as the reconcile loop's prune miss-grace, which stops a
/// discovery blip resetting the sizing caps: a mirror is rebuildable, so
/// deleting it an hour late costs disk, while deleting it on a blip costs a
/// clone per repo. A restart forgets the misses, which only delays a deletion.
const UNSERVED_PASSES: u32 = 3;

/// See [`UNSERVED_PASSES`].
const UNSERVED_GRACE_MS: u64 = 60 * 60 * 1000;

/// Loose objects at which a refresh is followed by a repack. Fetches of fewer
/// than `fetch.unpackLimit` (100) objects land loose, so they build up between
/// pushes.
pub const REPACK_LOOSE: u64 = 2000;

/// Packs at which a refresh is followed by a repack. Every larger fetch adds one.
pub const REPACK_PACKS: u64 = 20;

/// Where the refresh and maintenance containers see the volume.
const MIRROR_DIR: &str = "/mirror";

/// Where the refresh container sees faber's credential: a named volume
/// ([`GitCredential`]) mounted read-only, holding one file in git-credential
/// format.
const CREDENTIAL_DIR: &str = "/faber/git";
const CREDENTIAL_FILE: &str = "credentials";

/// Cap on one refresh. The first clone of a large repo moves real data, so it
/// gets far more than a metadata call; a bound regardless, because a hung fetch
/// must not hold the repo's writer lock forever.
const REFRESH_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// Cap on one repack or prune.
const MAINTENANCE_TIMEOUT: Duration = Duration::from_secs(30 * 60);

/// How much of a refresh container's output faber reads (#105). The report is a
/// handful of short lines.
const OUTPUT_CAP: usize = 64 * 1024;

/// How much of a failed fetch's message is kept for the dashboard.
const ERROR_CHARS: usize = 300;

// --- the decisions (pure) ------------------------------------------------------

/// The name of `owner_repo`'s mirror volume. Only ever used to *create* and
/// *mount* it; faber finds mirrors by their labels ([`volume_ls_args`]).
pub fn volume_name(owner_repo: &str) -> String {
    format!("faber-git-{}", docker::volume_slug(owner_repo))
}

/// The refspecs a refresh fetches (ADR-0035 §1): every branch and tag, and the
/// head of each listed same-repo PR. Nothing else, ever: no `refs/pull/*`
/// pattern, which would take fork PRs' heads, and no merge ref, which GitHub
/// recomputes and whose parents are enough.
///
/// `same_repo_prs` must come from [`same_repo_prs`]; a number this is handed is
/// trusted to name a same-repo head.
pub fn refspecs(same_repo_prs: &[u64]) -> Vec<String> {
    let mut prs = same_repo_prs.to_vec();
    prs.sort_unstable();
    prs.dedup();
    let mut out = vec![
        "+refs/heads/*:refs/heads/*".to_string(),
        "+refs/tags/*:refs/tags/*".to_string(),
    ];
    out.extend(
        prs.iter()
            .map(|n| format!("+refs/pull/{n}/head:refs/pull/{n}/head")),
    );
    out
}

/// The numbers of `pulls` whose head lives in `base` itself. A fork's head, and a
/// head whose fork was deleted (`head.repo` null), are never included: their
/// code is from outside the trust boundary (ADR-0035 §6).
///
/// Two checks, both required. The authoritative one: the head repo's numeric id
/// is the base repo's id, which no fork can share and no rename changes. And the
/// head repo's name is `base` (case-insensitively, as GitHub names are), so a
/// listing that somehow came from another repo cannot pass on ids alone.
pub fn same_repo_prs(base: &str, pulls: &[OpenPull]) -> Vec<u64> {
    let mut out: Vec<u64> = pulls
        .iter()
        .filter(|p| match (&p.head.repo, &p.base.repo) {
            (Some(head), Some(target)) => {
                head.id == target.id && head.full_name.eq_ignore_ascii_case(base)
            }
            _ => false,
        })
        .map(|p| p.number)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// The `docker run` flags that hand a runner its repo's mirror, or nothing.
///
/// `:ro` is the trust model (ADR-0035 §2, §6): no uid, sudo or `chmod` inside the
/// container can write through a read-only mount. The variable is set only when
/// the volume is, so it always describes the real mount.
pub fn runner_args(volume: Option<&str>) -> Vec<String> {
    match volume {
        None => Vec::new(),
        Some(vol) => vec![
            "-v".into(),
            format!("{vol}:{MOUNT}:ro"),
            "-e".into(),
            format!("{ENV}={MOUNT}"),
        ],
    }
}

/// Whether a run is one whose sighting refreshes its repo's mirror: faber has no
/// webhook, so a new `push` or `pull_request` run in the poll is the event.
pub fn is_trigger_event(event: &str) -> bool {
    matches!(event, "push" | "pull_request")
}

/// Fold one poll's runs (`(run_id, event)`) into `seen`, and say whether any
/// trigger run is new since the last poll.
///
/// `seen` is replaced by this poll's trigger runs, so it stays the size of the
/// active set rather than growing with the repo's history. A run that drops out
/// of the active statuses and comes back (a re-run) counts as new again, which
/// is one extra refresh at worst.
pub fn note_runs<'a>(
    seen: &mut HashSet<u64>,
    runs: impl IntoIterator<Item = (u64, &'a str)>,
) -> bool {
    let now: HashSet<u64> = runs
        .into_iter()
        .filter(|(_, event)| is_trigger_event(event))
        .map(|(id, _)| id)
        .collect();
    let fresh = now.iter().any(|id| !seen.contains(id));
    *seen = now;
    fresh
}

/// Whether a mirror's daily prune is due at `now_ms`. `None` is never due: the
/// clock starts at the first refresh this process sees, so a restart cannot make
/// pruning more frequent than daily.
pub fn prune_due(last_ms: Option<u64>, now_ms: u64) -> bool {
    last_ms.is_some_and(|last| now_ms.saturating_sub(last) >= PRUNE_INTERVAL_MS)
}

/// The floor a first clone is weighed against (ADR-0035 §5, amended by #348):
/// the higher of the janitor's reclaim floor (`[host] disk_floor_gib`, `0` off)
/// and the admission floor. A clone that leaves less than the reclaim floor
/// free starts the next reclamation, which deletes every mirror nothing mounts,
/// so every repo clones again: the loop the builder host ran hundreds of times
/// over about ten days. `None` when neither floor is set.
pub fn clone_floor_gib(disk_floor_gib: u64, admission_floor_gib: Option<u64>) -> Option<u64> {
    match disk_floor_gib.max(admission_floor_gib.unwrap_or(0)) {
        0 => None,
        f => Some(f),
    }
}

/// What the first clone of a mirror may do, against [`clone_floor_gib`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FirstClone {
    /// Build it.
    Build,
    /// Not now, and why; the next timer tries again.
    Wait(String),
}

/// Whether the first clone of a repo of `repo_kib` may start with `free_mib` free
/// on Docker's volume, under `floor_gib` ([`clone_floor_gib`]).
///
/// - No floor, or `0`: it builds, as the ADR says.
/// - Free space unreadable: it builds. The reading the job gate depends on is the
///   same one, so a clone that cannot be weighed is no worse than a job that
///   cannot, and Gate 0 still stops jobs at the floor.
/// - The repo's size unknown: it waits. A GitHub that cannot say how large the
///   repo is will not serve the clone either.
/// - Otherwise it builds only if free space less the repo's size stays at or
///   above the floor.
pub fn first_clone(
    floor_gib: Option<u64>,
    free_mib: Option<u64>,
    repo_kib: Option<u64>,
) -> FirstClone {
    let floor_gib = match floor_gib {
        None | Some(0) => return FirstClone::Build,
        Some(f) => f,
    };
    let Some(free_mib) = free_mib else {
        return FirstClone::Build;
    };
    let Some(repo_kib) = repo_kib else {
        return FirstClone::Wait(
            "the first clone waits: GitHub did not report the repo's size".to_string(),
        );
    };
    let repo_mib = repo_kib.div_ceil(1024);
    let after = free_mib.saturating_sub(repo_mib);
    if after < floor_gib.saturating_mul(1024) {
        FirstClone::Wait(format!(
            "the first clone waits: {free_mib} MiB free less the repo's {repo_mib} MiB is below \
             the {floor_gib} GiB floor"
        ))
    } else {
        FirstClone::Build
    }
}

/// What a refresh container reported ([`REFRESH_SCRIPT`]).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// The fetch succeeded.
    pub fetched: bool,
    /// What a failed fetch said, cut short and on one line.
    pub error: Option<String>,
    /// Loose objects in the mirror (`git count-objects -v`'s `count`).
    pub loose: u64,
    /// Packs in the mirror.
    pub packs: u64,
    /// The mirror's size on disk (KiB, `du -sk`).
    pub kib: u64,
    /// The mirror holds at least one branch: a first clone finished at some
    /// point, so it may be mounted, however stale this fetch left it.
    pub complete: bool,
}

/// Parse a refresh container's report. `None` unless it ends with `end`, so
/// output cut short is never read as a mirror with nothing in it.
pub fn parse_report(text: &str) -> Option<Report> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if lines.last() == Some(&"") {
        lines.pop();
    }
    if lines.last() != Some(&"end") {
        return None;
    }
    let mut r = Report::default();
    let mut fetch_seen = false;
    for line in &lines[..lines.len() - 1] {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        match key {
            "fetch" => {
                fetch_seen = true;
                r.fetched = value == "ok";
            }
            "error" => r.error = Some(value.chars().take(ERROR_CHARS).collect()),
            "loose" => r.loose = value.trim().parse().ok()?,
            "packs" => r.packs = value.trim().parse().ok()?,
            "kib" => r.kib = value.trim().parse().ok()?,
            "complete" => r.complete = value == "yes",
            _ => {}
        }
    }
    fetch_seen.then_some(r)
}

/// Whether a refresh should be followed by a repack (ADR-0035 §4: only when
/// enough loose objects or packs have built up).
pub fn needs_repack(r: &Report) -> bool {
    r.loose >= REPACK_LOOSE || r.packs >= REPACK_PACKS
}

/// Refreshes requested for one repo: at most one runs, and any number that
/// arrive meanwhile collapse into a single follow-up (ADR-0035 §1).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Coalesce {
    running: bool,
    pending: bool,
}

impl Coalesce {
    /// A refresh is wanted. `true` means none is running and the caller must
    /// start one; `false` means the running one will run once more.
    pub fn request(&mut self) -> bool {
        if self.running {
            self.pending = true;
            false
        } else {
            self.running = true;
            true
        }
    }

    /// The running refresh finished. `true` means run again, for the requests
    /// that arrived while it ran.
    pub fn finish(&mut self) -> bool {
        if self.pending {
            self.pending = false;
            true
        } else {
            self.running = false;
            false
        }
    }

    /// Whether a refresh is running now.
    pub fn running(&self) -> bool {
        self.running
    }
}

// --- the scripts ---------------------------------------------------------------

/// Shell all three scripts share, with the mirror's directory in `$1`. git obeys
/// a repository's own `config` and `hooks/`, so anything planted there would
/// steer a fetch that holds faber's token: a `url.<base>.insteadOf` sends it
/// elsewhere, `http.proxy` with `http.sslVerify = false` reads it in transit, and
/// a `reference-transaction` hook runs beside it. So git here reads no config
/// but this script's: no system or global file, `GIT_DIR` set so no planted
/// `.git` file is discovered, no hooks, and only https. `reset_repo` then deletes
/// what else in a repository points git outside it (`hooks/`, `commondir`,
/// `objects/info/alternates`) and rewrites `config` from a template. A new
/// mirror is pinned to SHA-1 and files-backed refs, the format the template
/// describes and the one GitHub serves.
macro_rules! hermetic_git_sh {
    () => {
        r#"set -u
umask 022
dir=$1
export LC_ALL=C GIT_DIR=. GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null GIT_TERMINAL_PROMPT=0
export GIT_DEFAULT_HASH=sha1 GIT_DEFAULT_REF_FORMAT=files
g() { git -c core.hooksPath=/dev/null -c protocol.allow=never -c protocol.https.allow=always "$@"; }
reset_repo() {
  rm -rf hooks commondir objects/info/alternates config.faber || return 1
  printf '%s\n' '[core]' 'repositoryformatversion = 0' 'bare = true' 'sharedRepository = world' \
    'logAllRefUpdates = false' 'hooksPath = /dev/null' '[gc]' 'auto = 0' '[fetch]' \
    'fsckObjects = true' > config.faber || return 1
  mv -f config.faber config
}
"#
    };
}

/// The refresh, run as `sh -c REFRESH_SCRIPT sh <dir> <url> <helper|-> <refspec>…`.
///
/// It always exits 0 and reports instead ([`parse_report`]), so a failed fetch
/// still says how large the mirror is and whether it is usable. The credential
/// reaches git through [`credential_helper`], which `cat`s the mounted file only
/// for `https://github.com`; git never sees it in an argument, and the
/// `-c credential.helper=` first clears any helper the image configured.
///
/// The repository is reset before every fetch (`hermetic_git_sh!`), so what a
/// fetch obeys is only ever what this script wrote.
///
/// Every write is git's own: objects through `index-pack` under a temporary
/// name, each ref through its own lockfile rename, so a job borrowing from the
/// mirror while this runs sees either the old ref with its objects or the new
/// one with its objects (ADR-0035 §4). PR heads no longer on the list are
/// deleted; their objects stay until the daily prune.
pub const REFRESH_SCRIPT: &str = concat!(
    hermetic_git_sh!(),
    r#"url=$2; helper=$3; shift 3
if [ "$helper" = - ]; then helper=; fi
report_fail() { echo "fetch failed"; echo "error $1"; echo end; exit 0; }
cd "$dir" 2>/dev/null || report_fail "cannot enter the mirror"
if [ ! -f HEAD ]; then
  g init -q --bare . >/dev/null 2>&1 || report_fail "git init failed"
fi
reset_repo || report_fail "cannot reset the mirror's config and hooks"
if err=$(g -c credential.helper= -c "credential.helper=$helper" -c protocol.version=2 \
    fetch --quiet --prune --no-tags "$url" "$@" 2>&1); then
  echo "fetch ok"
else
  echo "fetch failed"
  printf 'error %s\n' "$(printf '%s' "$err" | tr '\n\t' '  ' | cut -c1-300)"
fi
g for-each-ref --format='%(refname)' refs/pull/ | while read -r ref; do
  keep=no
  for spec in "$@"; do
    [ "$spec" = "+$ref:$ref" ] && keep=yes
  done
  [ "$keep" = yes ] || g update-ref -d "$ref"
done
g count-objects -v | while IFS=': ' read -r k v; do
  case "$k" in
    count) echo "loose $v" ;;
    packs) echo "packs $v" ;;
  esac
done
echo "kib $(du -sk . | cut -f1)"
if [ -n "$(g for-each-ref --count=1 refs/heads/)" ]; then echo "complete yes"; else echo "complete no"; fi
echo end
"#
);

/// Routine maintenance after a refresh (ADR-0035 §4): one pack, and unreachable
/// objects kept in it, because a running job may borrow any of them.
pub const REPACK_SCRIPT: &str = concat!(
    hermetic_git_sh!(),
    r#"cd "$dir" || exit 1
reset_repo || exit 1
g repack -a -d -q --keep-unreachable || exit 1
echo "fetch ok"
echo "kib $(du -sk . | cut -f1)"
echo end
"#
);

/// The daily prune (ADR-0035 §4), run only while nothing mounts the mirror.
/// `gc` runs the `reference-transaction` hook as it packs refs, so it is reset
/// first like a refresh.
pub const PRUNE_SCRIPT: &str = concat!(
    hermetic_git_sh!(),
    r#"cd "$dir" || exit 1
reset_repo || exit 1
g gc --prune=now -q || exit 1
echo "fetch ok"
echo "kib $(du -sk . | cut -f1)"
echo end
"#
);

/// The credential helper a refresh hands git ([`REFRESH_SCRIPT`]'s `<helper>`):
/// on `get`, `file` printed, and only for `https://github.com`. git asks a
/// helper for whatever host it is about to send a request to, and one that
/// answered any host would hand the token to wherever a rewritten URL pointed.
/// The host is compared whole, so `github.com:8443` and `github.com.<anything>`
/// get nothing.
pub(crate) fn credential_helper(file: &str) -> String {
    format!(
        "!f() {{ test \"$1\" = get || return 0; p=; h=; while IFS= read -r l; do \
         case \"$l\" in protocol=*) p=${{l#protocol=}} ;; host=*) h=${{l#host=}} ;; '') break ;; \
         esac; done; test \"$p\" = https && test \"$h\" = github.com && cat {}; }}; f",
        crate::control::shell_quote(file)
    )
}

// --- argv ------------------------------------------------------------------------

/// The labels on everything here: faber's, of `kind`, for `owner_repo`. Read
/// back by label, never by name (AGENTS.md).
fn labels(kind: &str, owner_repo: &str) -> Vec<String> {
    [
        format!("{LABEL_OWNED}=true"),
        format!("{LABEL_KIND}={kind}"),
        format!("{LABEL_REPO}={owner_repo}"),
    ]
    .into_iter()
    .flat_map(|kv| ["--label".to_string(), kv])
    .collect()
}

/// `docker volume create` for `owner_repo`'s mirror. Idempotent: an existing
/// volume is a no-op.
fn volume_create_args(owner_repo: &str) -> Vec<String> {
    let mut args = vec!["volume".to_string(), "create".to_string()];
    args.extend(labels(KIND_GIT_MIRROR, owner_repo));
    args.push(volume_name(owner_repo));
    args
}

/// `docker volume inspect` of the mirror's labels, to tell a first clone from a
/// refresh, and faber's mirror from a volume that only carries its name.
fn volume_inspect_args(owner_repo: &str) -> Vec<String> {
    vec![
        "volume".into(),
        "inspect".into(),
        "--format".into(),
        "{{json .Labels}}".into(),
        volume_name(owner_repo),
    ]
}

/// What sits under a mirror's volume name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeState {
    /// Nothing, or the daemon could not say.
    Missing,
    /// faber's mirror of this repo: labelled `org.faber.kind=git-mirror` and
    /// `org.faber.repo=<this repo>`.
    Ours,
    /// A volume with the name but not the labels. The likeliest is one Docker
    /// created empty at a job's `-v` after the mirror was deleted behind faber's
    /// back; `volume create` would then be a no-op and the labels would never
    /// arrive, so the janitor and reclamation, which read labels, would never
    /// see it. faber did not label it, so it may not delete it (AGENTS.md: what
    /// faber may delete is what its labels say it built): it is never mounted,
    /// never written, never removed, and the mirror is blocked until the operator
    /// removes or renames it ([`plan_refresh`]).
    Foreign,
}

/// What a refresh does about what sits under the mirror's name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefreshPlan {
    /// faber's mirror is there: fetch into it.
    Refresh,
    /// Nothing is there: a first clone, weighed against the disk floor.
    FirstClone,
    /// Something faber did not label is there: touch nothing, and say what the
    /// operator has to do.
    Blocked(String),
}

/// Decide a refresh from the volume's state. A [`VolumeState::Foreign`] volume
/// is refused, never deleted, mounted or fetched into.
pub fn plan_refresh(state: VolumeState, owner_repo: &str) -> RefreshPlan {
    match state {
        VolumeState::Ours => RefreshPlan::Refresh,
        VolumeState::Missing => RefreshPlan::FirstClone,
        VolumeState::Foreign => {
            let name = volume_name(owner_repo);
            RefreshPlan::Blocked(format!(
                "blocked: a volume named {name} exists without faber's git-mirror labels, so \
                 faber will not use or delete it — remove volume {name} by hand or rename it; \
                 faber will then build its mirror"
            ))
        }
    }
}

/// Classify a [`volume_inspect_args`] answer: `None` when the inspect failed.
/// Labels are read, never the name (AGENTS.md).
pub fn classify_volume(inspected: Option<&str>, owner_repo: &str) -> VolumeState {
    let Some(text) = inspected else {
        return VolumeState::Missing;
    };
    let labels: Option<HashMap<String, String>> = serde_json::from_str(text.trim()).ok().flatten();
    let ours = labels.is_some_and(|l| {
        l.get(LABEL_OWNED).map(String::as_str) == Some("true")
            && l.get(LABEL_KIND).map(String::as_str) == Some(KIND_GIT_MIRROR)
            && l.get(LABEL_REPO).map(String::as_str) == Some(owner_repo)
    });
    if ours {
        VolumeState::Ours
    } else {
        VolumeState::Foreign
    }
}

/// Every mirror volume on the host, by its label, as one JSON object per volume
/// carrying its name and repo.
fn volume_ls_args() -> Vec<String> {
    vec![
        "volume".into(),
        "ls".into(),
        "--filter".into(),
        format!("label={LABEL_KIND}={KIND_GIT_MIRROR}"),
        "--format".into(),
        format!(r#"{{"Name":{{{{json .Name}}}},"Repo":{{{{json (.Label "{LABEL_REPO}")}}}}}}"#),
    ]
}

/// Every container, running or not, that mounts `volume`: a stopped runner
/// awaiting teardown still holds its mount.
fn mounted_by_args(volume: &str) -> Vec<String> {
    vec![
        "ps".into(),
        "-a".into(),
        "--filter".into(),
        format!("volume={volume}"),
        "--format".into(),
        "{{.Names}}".into(),
    ]
}

/// The refresh container (ADR-0035 §1): the profile's runner image (it has git,
/// because `actions/checkout` needs git), as root so it can write the volume
/// whatever uid the image runs jobs as, on Docker's default network rather than
/// any job's, with no dind. The refspecs follow the script as arguments, so
/// they are only ever data to the shell.
#[allow(clippy::too_many_arguments)]
fn refresh_args(
    owner_repo: &str,
    image: &str,
    container: &str,
    credential_vol: Option<&str>,
    url: &str,
    refspecs: &[String],
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "--rm".into(),
        "--name".into(),
        container.into(),
        "--pull=never".into(),
        "--user".into(),
        "0:0".into(),
        "--entrypoint".into(),
        "sh".into(),
    ];
    args.extend(labels(KIND_GIT_MIRROR_WORK, owner_repo));
    args.push("-v".into());
    args.push(format!("{}:{MIRROR_DIR}", volume_name(owner_repo)));
    let helper = match credential_vol {
        Some(vol) => {
            args.push("-v".into());
            args.push(format!("{vol}:{CREDENTIAL_DIR}:ro"));
            credential_helper(&format!("{CREDENTIAL_DIR}/{CREDENTIAL_FILE}"))
        }
        None => "-".to_string(),
    };
    args.push(image.into());
    args.extend([
        "-c".to_string(),
        REFRESH_SCRIPT.to_string(),
        "sh".to_string(),
        MIRROR_DIR.to_string(),
        url.to_string(),
        helper,
    ]);
    args.extend(refspecs.iter().cloned());
    args
}

/// A repack or prune container: the volume read-write, no network at all.
fn maintenance_args(owner_repo: &str, image: &str, container: &str, script: &str) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "--rm".into(),
        "--name".into(),
        container.into(),
        "--pull=never".into(),
        "--user".into(),
        "0:0".into(),
        "--network".into(),
        "none".into(),
        "--entrypoint".into(),
        "sh".into(),
    ];
    args.extend(labels(KIND_GIT_MIRROR_WORK, owner_repo));
    args.push("-v".into());
    args.push(format!("{}:{MIRROR_DIR}", volume_name(owner_repo)));
    args.push(image.into());
    args.extend([
        "-c".to_string(),
        script.to_string(),
        "sh".to_string(),
        MIRROR_DIR.to_string(),
    ]);
    args
}

/// The URL a refresh fetches: exactly the one `actions/checkout` computes.
fn fetch_url(owner_repo: &str) -> String {
    format!("https://github.com/{owner_repo}")
}

/// A name for one refresh or maintenance container, unique within this process
/// and across restarts. Not `faber-git-…`, which is the volumes' namespace.
fn container_name(what: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "faber-gitwork-{what}-{}-{}",
        crate::audit::now_ms(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

// --- the credential ------------------------------------------------------------

/// faber's token as a git-credential file, alive for one refresh.
///
/// The same delivery as the registry credential ([`docker::REGISTRY_CONFIG_MOUNT`]):
/// a `0600` file in a `0700` directory, in a named volume of its own
/// ([`CredentialVolume`]) that a short-lived container of the refresh image fills
/// from its stdin, mounted read-only, never an argument, a variable or a host
/// file (ADR-0026: faber's Docker shares no host folder). The volume's name, never
/// the token, is all that is passed around. [`refresh_io`] removes it as soon as
/// the refresh ends; a leftover is reaped by [`Mirrors::janitor`] by its
/// `git-auth` label once no refresh of its repo holds the writer lock.
struct GitCredential {
    volume: CredentialVolume,
}

impl GitCredential {
    /// Fill a credential volume for `container`'s refresh of `owner_repo` with
    /// `token`, or `None` when that fails; the refresh then fetches anonymously,
    /// which a private repo refuses. The refresh runs as root, so the file stays
    /// root's.
    async fn prepare(image: &str, owner_repo: &str, token: &str, container: &str) -> Option<Self> {
        let volume = credential_volume_name(container);
        let labels = labels(KIND_GIT_AUTH, owner_repo);
        let fill_container = container_name("credential");
        let fill = CredentialFill {
            volume: &volume,
            labels: &labels,
            container: &fill_container,
            image,
            dir: CREDENTIAL_DIR,
            file: CREDENTIAL_FILE,
            owner: None,
        };
        match CredentialVolume::fill(&fill, credential_file(token).as_bytes()).await {
            Ok(volume) => Some(Self { volume }),
            Err(e) => {
                warn!("git mirror: could not write the fetch credential: {e}");
                None
            }
        }
    }
}

/// The git-credential file a refresh reads.
pub(crate) fn credential_file(token: &str) -> String {
    format!("username=x-access-token\npassword={token}\n")
}

/// The credential volume for the refresh `container`. Only ever used to create
/// and mount it; a leftover is found by its label ([`credential_ls_args`]).
fn credential_volume_name(container: &str) -> String {
    let what = container
        .strip_prefix("faber-gitwork-")
        .unwrap_or(container);
    format!("faber-gitauth-{what}")
}

/// Every credential-fill container that has stopped, by its label, as one JSON
/// object per container carrying its name and repo. The fill removes its own
/// container after it exits (#348); one whose removal failed holds its
/// credential volume until this listing finds it.
fn credential_fill_ps_args() -> Vec<String> {
    vec![
        "ps".into(),
        "-a".into(),
        "--filter".into(),
        format!("label={LABEL_KIND}={KIND_GIT_AUTH}"),
        "--filter".into(),
        "status=exited".into(),
        "--filter".into(),
        "status=created".into(),
        "--filter".into(),
        "status=dead".into(),
        "--format".into(),
        format!(r#"{{"Name":{{{{json .Names}}}},"Repo":{{{{json (.Label "{LABEL_REPO}")}}}}}}"#),
    ]
}

/// Every refresh-credential volume on the host, by its label, as one JSON object
/// per volume carrying its name and repo.
fn credential_ls_args() -> Vec<String> {
    vec![
        "volume".into(),
        "ls".into(),
        "--filter".into(),
        format!("label={LABEL_KIND}={KIND_GIT_AUTH}"),
        "--format".into(),
        format!(r#"{{"Name":{{{{json .Name}}}},"Repo":{{{{json (.Label "{LABEL_REPO}")}}}}}}"#),
    ]
}

// --- the state -----------------------------------------------------------------

/// What a refresh needs that only boot knows.
pub struct Ctx {
    /// faber's own client: its token fetches, its REST calls list PRs.
    pub gh: Arc<Github>,
    /// The runner image of the first `git_mirror` profile, which runs the
    /// refresh and maintenance containers.
    pub image: String,
    /// The floor the first clone is weighed by ([`clone_floor_gib`]).
    pub floor_gib: Option<u64>,
}

/// One repo's mirror, as this process knows it.
#[derive(Debug, Default)]
struct RepoState {
    coalesce: Coalesce,
    /// A refresh reported the mirror complete: jobs may mount it.
    ready: bool,
    /// The trigger runs the last poll saw ([`note_runs`]).
    seen_runs: HashSet<u64>,
    last_ok_ms: Option<u64>,
    last_attempt_ms: Option<u64>,
    last_error: Option<String>,
    /// A volume faber did not label holds the mirror's name, and what the
    /// operator has to do about it ([`RefreshPlan::Blocked`]).
    blocked: Option<String>,
    kib: Option<u64>,
    last_prune_ms: Option<u64>,
    /// Jobs hold a read guard for as long as they mount the mirror; prune and
    /// delete take the write side, and only by `try`, so they wait for an idle
    /// mirror rather than stalling a launch.
    mounts: Arc<RwLock<()>>,
    /// The one writer: refresh, repack, prune and delete.
    writer: Arc<tokio::sync::Mutex<()>>,
}

/// What deleting a mirror volume came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Removal {
    /// This call removed it.
    Removed,
    /// It was not there by the time the delete ran.
    AlreadyGone,
    /// A refresh, a job's lease or a mounting container kept it.
    InUse,
}

/// A job's hold on its repo's mirror, from launch to teardown. While any lease
/// is out, the mirror is neither pruned nor deleted (ADR-0035 §4).
#[derive(Debug)]
pub struct Lease {
    volume: String,
    _guard: OwnedRwLockReadGuard<()>,
}

impl Lease {
    /// The volume to mount.
    pub fn volume(&self) -> &str {
        &self.volume
    }
}

/// One mirror on the dashboard (ADR-0035 §5): its size, its last refresh and its
/// last refresh error. Additive to `/api/state`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MirrorView {
    pub repo: String,
    /// Jobs are being given it.
    pub ready: bool,
    /// A refresh is running now.
    pub refreshing: bool,
    pub size_kib: Option<u64>,
    /// When a fetch last succeeded (ms).
    pub last_refresh_ms: Option<u64>,
    /// When a refresh was last attempted (ms).
    pub last_attempt_ms: Option<u64>,
    /// Why the last refresh did not succeed, if it did not.
    pub last_error: Option<String>,
    /// Set while a volume faber did not label holds the mirror's name: a fault
    /// only the operator can clear, naming the volume and the fix. Additive.
    #[serde(default)]
    pub blocked: Option<String>,
}

/// Every repo's mirror. One per process ([`global`]), because the docker
/// backend's launch path and the reconcile loop both reach it.
#[derive(Default)]
pub struct Mirrors {
    ctx: OnceLock<Ctx>,
    repos: Mutex<HashMap<String, RepoState>>,
    /// Repos with a mirror volume that the watched-repo listing has left out,
    /// since when and how many listings in a row ([`UNSERVED_PASSES`]).
    unserved: Mutex<HashMap<String, Miss>>,
    /// Held through a first clone, so they run one at a time and each weighs
    /// the disk the previous one left. Unheld, every repo read the same free
    /// space at the same tick and all of them cloned into it (#348).
    first_clones: tokio::sync::Mutex<()>,
}

/// A run of watched-repo listings one mirrored repo was missing from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Miss {
    since_ms: u64,
    passes: u32,
}

/// The mirror volumes the janitor deletes this pass, from `volumes` as
/// `(name, repo)`, the watched repos `served`, and the misses so far, which it
/// updates. With mirrors off every volume goes at once: that is the operator
/// turning the key off, not discovery. With them on, a volume goes only once its
/// repo has been missing for [`UNSERVED_PASSES`] listings over at least
/// [`UNSERVED_GRACE_MS`]; a listing that names the repo again ends its run.
fn unserved_due(
    misses: &mut HashMap<String, Miss>,
    volumes: &[(String, String)],
    served: &[String],
    enabled: bool,
    now_ms: u64,
) -> Vec<(String, String)> {
    if !enabled {
        misses.clear();
        return volumes.to_vec();
    }
    let missing: HashSet<&str> = volumes
        .iter()
        .map(|(_, repo)| repo.as_str())
        .filter(|repo| !served.iter().any(|s| s == repo))
        .collect();
    misses.retain(|repo, _| missing.contains(repo.as_str()));
    for repo in &missing {
        let miss = misses.entry((*repo).to_string()).or_insert(Miss {
            since_ms: now_ms,
            passes: 0,
        });
        miss.passes = miss.passes.saturating_add(1);
    }
    volumes
        .iter()
        .filter(|(_, repo)| {
            misses.get(repo).is_some_and(|m| {
                m.passes >= UNSERVED_PASSES
                    && now_ms.saturating_sub(m.since_ms) >= UNSERVED_GRACE_MS
            })
        })
        .cloned()
        .collect()
}

/// The process's mirrors.
pub fn global() -> &'static Mirrors {
    static MIRRORS: OnceLock<Mirrors> = OnceLock::new();
    MIRRORS.get_or_init(Mirrors::default)
}

impl Mirrors {
    /// Turn mirrors on for this process. Called once at boot, and only when a
    /// profile has `git_mirror = true`.
    pub fn configure(&self, ctx: Ctx) {
        if self.ctx.set(ctx).is_err() {
            warn!("git mirror: configured twice; keeping the first");
        }
    }

    /// Whether any profile turned mirrors on.
    pub fn enabled(&self) -> bool {
        self.ctx.get().is_some()
    }

    /// A lease on `owner_repo`'s mirror for one job, or `None`: not refreshed
    /// yet, or being pruned or deleted right now. `None` is a plain checkout.
    pub fn lease(&self, owner_repo: &str) -> Option<Lease> {
        let repos = self.repos.lock().unwrap();
        let state = repos.get(owner_repo)?;
        if !state.ready {
            return None;
        }
        let guard = state.mounts.clone().try_read_owned().ok()?;
        Some(Lease {
            volume: volume_name(owner_repo),
            _guard: guard,
        })
    }

    /// [`Self::lease`], then a check that the volume under the name is faber's
    /// mirror of this repo ([`VolumeState::Ours`]). Anything else is not mounted:
    /// a `-v` of a missing volume would have Docker create an empty, unlabelled
    /// one. The mirror stops being ready, and a refresh is asked for, which
    /// rebuilds it (ADR-0035 §2, best-effort).
    pub async fn lease_checked(&'static self, owner_repo: &str) -> Option<Lease> {
        let lease = self.lease(owner_repo)?;
        let state = volume_state(owner_repo).await;
        if state == VolumeState::Ours {
            return Some(lease);
        }
        drop(lease);
        let why = match plan_refresh(state, owner_repo) {
            RefreshPlan::Blocked(fault) => fault,
            _ => "not mounted: its volume is gone".to_string(),
        };
        warn!("git mirror: {owner_repo}: not mounting it — {why}");
        self.with_state(owner_repo, |s| {
            s.ready = false;
            if matches!(state, VolumeState::Foreign) {
                s.blocked = Some(why.clone());
            }
            s.last_error = Some(why);
        });
        self.request_refresh(owner_repo);
        None
    }

    /// A runner launch with the mirror mounted was refused, and the job is being
    /// launched without it (ADR-0035 §2). The mirror stops being given to jobs
    /// until a refresh has looked at it again, and the dashboard says why.
    pub fn note_mount_failure(&'static self, owner_repo: &str, why: &str) {
        self.with_state(owner_repo, |s| {
            s.ready = false;
            s.last_error = Some(format!(
                "a job could not mount it: {}",
                why.chars().take(ERROR_CHARS).collect::<String>()
            ));
        });
        self.request_refresh(owner_repo);
    }

    /// What one poll of `owner_repo` saw. A new `push` or `pull_request` run
    /// requests a refresh. A no-op unless mirrors are on.
    pub fn observe_runs<'a>(
        &'static self,
        owner_repo: &str,
        runs: impl IntoIterator<Item = (u64, &'a str)>,
    ) {
        if !self.enabled() {
            return;
        }
        let fresh = {
            let mut repos = self.repos.lock().unwrap();
            let state = repos.entry(owner_repo.to_string()).or_default();
            note_runs(&mut state.seen_runs, runs)
        };
        if fresh {
            debug!("git mirror: {owner_repo}: a new push or pull_request run — refreshing");
            self.request_refresh(owner_repo);
        }
    }

    /// Ask for a refresh of `owner_repo`'s mirror: started now if none is
    /// running, folded into one follow-up if one is.
    pub fn request_refresh(&'static self, owner_repo: &str) {
        if !self.enabled() {
            return;
        }
        let start = {
            let mut repos = self.repos.lock().unwrap();
            repos
                .entry(owner_repo.to_string())
                .or_default()
                .coalesce
                .request()
        };
        if start {
            let repo = owner_repo.to_string();
            tokio::spawn(async move { self.refresh_worker(repo).await });
        }
    }

    /// Run refreshes of one repo until no request is pending.
    async fn refresh_worker(&'static self, repo: String) {
        loop {
            self.refresh_once(&repo).await;
            let again = {
                let mut repos = self.repos.lock().unwrap();
                repos
                    .get_mut(&repo)
                    .map(|s| s.coalesce.finish())
                    .unwrap_or(false)
            };
            if !again {
                break;
            }
        }
    }

    /// The lock handles of `owner_repo`, created if this process has not met it.
    fn handles(&self, owner_repo: &str) -> (Arc<RwLock<()>>, Arc<tokio::sync::Mutex<()>>) {
        let mut repos = self.repos.lock().unwrap();
        let state = repos.entry(owner_repo.to_string()).or_default();
        (state.mounts.clone(), state.writer.clone())
    }

    fn with_state<T>(&self, owner_repo: &str, f: impl FnOnce(&mut RepoState) -> T) -> T {
        let mut repos = self.repos.lock().unwrap();
        f(repos.entry(owner_repo.to_string()).or_default())
    }

    /// One refresh, under the repo's writer lock, and the repack that may follow.
    async fn refresh_once(&self, repo: &str) {
        let Some(ctx) = self.ctx.get() else {
            return;
        };
        let (_, writer) = self.handles(repo);
        let _writing = writer.lock().await;
        let started = crate::audit::now_ms();
        self.with_state(repo, |s| s.last_attempt_ms = Some(started));
        let unusable = |blocked: Option<String>| {
            self.with_state(repo, |s| {
                s.ready = false;
                s.kib = None;
                s.blocked = blocked;
            })
        };
        match refresh_io(ctx, repo, &self.first_clones, unusable).await {
            Ok(report) => {
                let usable = self.with_state(repo, |s| {
                    s.blocked = None;
                    s.ready = s.ready || report.complete;
                    s.kib = Some(report.kib);
                    if report.fetched {
                        s.last_ok_ms = Some(started);
                        s.last_error = None;
                    } else {
                        s.last_error = Some(
                            report
                                .error
                                .clone()
                                .unwrap_or_else(|| "the fetch failed".to_string()),
                        );
                    }
                    if s.ready && s.last_prune_ms.is_none() {
                        s.last_prune_ms = Some(started);
                    }
                    s.ready
                });
                if report.fetched {
                    debug!(
                        "git mirror: {repo}: refreshed ({} KiB, {} loose, {} packs)",
                        report.kib, report.loose, report.packs
                    );
                } else {
                    warn!(
                        "git mirror: {repo}: refresh failed ({}) — {}",
                        report.error.as_deref().unwrap_or("no message"),
                        if usable {
                            "jobs keep the previous mirror"
                        } else {
                            "jobs check out without one"
                        }
                    );
                }
                if needs_repack(&report) {
                    self.maintain(ctx, repo, "repack", REPACK_SCRIPT).await;
                }
            }
            Err(e) => {
                warn!("git mirror: {repo}: {e}");
                self.with_state(repo, |s| s.last_error = Some(e));
            }
        }
    }

    /// Run a repack or prune container and record the size it reports. Called
    /// with the writer lock held.
    async fn maintain(&self, ctx: &Ctx, repo: &str, what: &str, script: &str) -> bool {
        let container = container_name(what);
        let args = maintenance_args(repo, &ctx.image, &container, script);
        match run_container(&container, &args, MAINTENANCE_TIMEOUT).await {
            Ok(text) => match parse_report(&text) {
                Some(r) => {
                    self.with_state(repo, |s| s.kib = Some(r.kib));
                    info!("git mirror: {repo}: {what} done ({} KiB)", r.kib);
                    true
                }
                None => {
                    warn!("git mirror: {repo}: the {what} reported nothing usable");
                    false
                }
            },
            Err(e) => {
                warn!("git mirror: {repo}: {what} failed: {e}");
                false
            }
        }
    }

    /// The janitor's pass (ADR-0035 §4, §5): delete the mirrors of repos no
    /// `git_mirror` profile serves any more, and run each served mirror's daily
    /// prune while nothing mounts it.
    ///
    /// `served` is `None` when the watched repos could not be listed: nothing is
    /// deleted then, because a failed listing is not a repo leaving the topic.
    /// Nor is one listing that leaves a repo out: see [`UNSERVED_PASSES`].
    pub async fn janitor(&self, served: Option<&[String]>) {
        self.reap_credentials().await;
        let volumes = match list_volumes().await {
            Ok(v) => v,
            Err(e) => {
                warn!("git mirror: listing mirror volumes: {e}");
                return;
            }
        };
        // A failed listing (`None`) is no evidence either way, so it neither
        // deletes nor touches the run of misses.
        if let Some(served) = served {
            let due = unserved_due(
                &mut self.unserved.lock().unwrap(),
                &volumes,
                served,
                self.enabled(),
                crate::audit::now_ms(),
            );
            for (name, repo) in &due {
                match self.delete(repo, name).await {
                    Ok(Removal::Removed) => info!(
                        "git mirror: deleted {name} — no git_mirror profile serves {repo} any more"
                    ),
                    Ok(Removal::AlreadyGone) => debug!("git mirror: {name} was already gone"),
                    Ok(Removal::InUse) => {
                        debug!("git mirror: {name} is in use — deleting it later")
                    }
                    Err(e) => warn!("git mirror: deleting {name}: {e}"),
                }
            }
        }
        let Some(ctx) = self.ctx.get() else {
            return;
        };
        let now = crate::audit::now_ms();
        let due: Vec<String> = {
            let repos = self.repos.lock().unwrap();
            repos
                .iter()
                .filter(|(_, s)| s.ready && prune_due(s.last_prune_ms, now))
                .map(|(r, _)| r.clone())
                .collect()
        };
        for repo in due {
            self.prune(ctx, &repo).await;
        }
    }

    /// Remove refresh-credential volumes a refresh left behind (ADR-0035 §1), by
    /// their `git-auth` label (AGENTS.md: read labels, never names). A refresh
    /// holds its repo's writer lock from before its credential volume exists until
    /// after it is removed, so one found while that lock is free has no owner in
    /// this process. One a previous process's refresh container still mounts is
    /// refused `in use` by the daemon and left for a later pass.
    async fn reap_credentials(&self) {
        // Stopped fill containers first: each mounts its volume until it goes.
        match docker::docker_stdout(
            &credential_fill_ps_args(),
            docker::CLI_TIMEOUT,
            untrusted::OUTPUT_CAP,
        )
        .await
        {
            Ok(text) => {
                for (name, repo) in parse_volumes(&text) {
                    let (_, writer) = self.handles(&repo);
                    let Ok(_writing) = writer.try_lock_owned() else {
                        continue;
                    };
                    match docker::docker_rm(&["rm", "-f", &name]).await {
                        Ok(true) => debug!("git mirror: removed a leftover credential fill {name}"),
                        Ok(false) => {}
                        Err(e) => warn!("git mirror: removing credential fill {name}: {e}"),
                    }
                }
            }
            Err(e) => warn!("git mirror: listing credential fills: {e}"),
        }
        let text = match docker::docker_stdout(
            &credential_ls_args(),
            docker::CLI_TIMEOUT,
            untrusted::OUTPUT_CAP,
        )
        .await
        {
            Ok(text) => text,
            Err(e) => {
                warn!("git mirror: listing credential volumes: {e}");
                return;
            }
        };
        for (name, repo) in parse_volumes(&text) {
            let (_, writer) = self.handles(&repo);
            let Ok(_writing) = writer.try_lock_owned() else {
                continue;
            };
            match docker::docker_rm(&["volume", "rm", &name]).await {
                Ok(true) => debug!("git mirror: removed a leftover fetch credential {name}"),
                Ok(false) => {}
                Err(e) if e.to_string().to_lowercase().contains("in use") => {
                    debug!("git mirror: credential {name} is in use — removing it later")
                }
                Err(e) => warn!("git mirror: removing credential {name}: {e}"),
            }
        }
    }

    /// `gc --prune=now` on one mirror, only while no job holds a lease, no
    /// container mounts the volume, and no refresh is running.
    async fn prune(&self, ctx: &Ctx, repo: &str) {
        let (mounts, writer) = self.handles(repo);
        let Ok(_writing) = writer.try_lock_owned() else {
            return;
        };
        let Ok(_exclusive) = mounts.try_write_owned() else {
            debug!("git mirror: {repo}: a job mounts it — pruning later");
            return;
        };
        match mounted(&volume_name(repo)).await {
            Ok(false) => {}
            Ok(true) => {
                debug!("git mirror: {repo}: a container mounts it — pruning later");
                return;
            }
            Err(e) => {
                warn!(
                    "git mirror: {repo}: could not tell whether it is mounted ({e}) — not pruning"
                );
                return;
            }
        }
        if self.maintain(ctx, repo, "prune", PRUNE_SCRIPT).await {
            self.with_state(repo, |s| s.last_prune_ms = Some(crate::audit::now_ms()));
        }
    }

    /// Under disk pressure (ADR-0035 §5): delete every mirror nothing mounts.
    /// The next jobs check out cold until a refresh rebuilds it. Returns the
    /// volumes removed.
    pub async fn reclaim(&self) -> Vec<String> {
        let volumes = match list_volumes().await {
            Ok(v) => v,
            Err(e) => {
                warn!("git mirror: listing mirror volumes: {e}");
                return Vec::new();
            }
        };
        let mut gone = Vec::new();
        for (name, repo) in volumes {
            match self.delete(&repo, &name).await {
                Ok(Removal::Removed) => gone.push(name),
                Ok(Removal::AlreadyGone) => debug!("git mirror: {name} was already gone"),
                Ok(Removal::InUse) => debug!("git mirror: {name} is in use — kept"),
                Err(e) => warn!("git mirror: deleting {name}: {e}"),
            }
        }
        gone
    }

    /// Delete one mirror volume. [`Removal::InUse`] when a refresh is running, a
    /// job holds a lease, or the daemon refuses because a container mounts it.
    /// Never `-f`.
    async fn delete(&self, repo: &str, name: &str) -> anyhow::Result<Removal> {
        anyhow::ensure!(
            untrusted::object_name(name),
            "not a volume name Docker allows, nothing deleted"
        );
        let (mounts, writer) = self.handles(repo);
        let Ok(_writing) = writer.try_lock_owned() else {
            return Ok(Removal::InUse);
        };
        let Ok(_exclusive) = mounts.try_write_owned() else {
            return Ok(Removal::InUse);
        };
        match docker::docker_rm(&["volume", "rm", name]).await {
            Ok(removed) => {
                let mut repos = self.repos.lock().unwrap();
                if let Some(s) = repos.get_mut(repo) {
                    s.ready = false;
                    s.kib = None;
                    s.last_ok_ms = None;
                    s.last_prune_ms = None;
                    // Forgotten unless a refresh is queued: a served repo's next
                    // refresh starts it again as a first clone.
                    if !s.coalesce.running() {
                        repos.remove(repo);
                    }
                }
                Ok(if removed {
                    Removal::Removed
                } else {
                    Removal::AlreadyGone
                })
            }
            Err(e) if e.to_string().to_lowercase().contains("in use") => Ok(Removal::InUse),
            Err(e) => Err(e),
        }
    }

    /// Every mirror this process knows, for `/api/state`, by repo.
    pub fn view(&self) -> Vec<MirrorView> {
        let repos = self.repos.lock().unwrap();
        let mut out: Vec<MirrorView> = repos
            .iter()
            .filter(|(_, s)| s.last_attempt_ms.is_some() || s.ready)
            .map(|(repo, s)| MirrorView {
                repo: repo.clone(),
                ready: s.ready,
                refreshing: s.coalesce.running(),
                size_kib: s.kib,
                last_refresh_ms: s.last_ok_ms,
                last_attempt_ms: s.last_attempt_ms,
                last_error: s.last_error.clone(),
                blocked: s.blocked.clone(),
            })
            .collect();
        out.sort_by(|a, b| a.repo.cmp(&b.repo));
        out
    }
}

// --- the I/O ---------------------------------------------------------------------

/// One refresh of `repo`, from the disk check to the report. `unusable` is told
/// when there is no mirror of faber's under the name — `None` when nothing is
/// there, `Some(fault)` when something faber did not label is — so the caller
/// stops calling it ready and shows the fault.
async fn refresh_io(
    ctx: &Ctx,
    repo: &str,
    first_clones: &tokio::sync::Mutex<()>,
    unusable: impl FnOnce(Option<String>),
) -> Result<Report, String> {
    // Held to the end of a first clone: the clone, not the check, is what fills
    // the disk the next repo's check reads.
    let mut _first_clone = None;
    match plan_refresh(volume_state(repo).await, repo) {
        RefreshPlan::Refresh => {}
        RefreshPlan::Blocked(fault) => {
            unusable(Some(fault.clone()));
            return Err(fault);
        }
        RefreshPlan::FirstClone => {
            unusable(None);
            _first_clone = Some(first_clones.lock().await);
            let free = crate::host::docker_disk_free().await.map(|r| r.free_mib);
            let size = ctx.gh.repo_size_kib(repo).await.ok();
            if let FirstClone::Wait(why) = first_clone(ctx.floor_gib, free, size) {
                return Err(why);
            }
            info!("git mirror: {repo}: building the first clone");
        }
    }
    docker::docker_stdout(&volume_create_args(repo), docker::CLI_TIMEOUT, OUTPUT_CAP)
        .await
        .map_err(|e| format!("creating the mirror volume: {e}"))?;
    let pulls = ctx
        .gh
        .open_pulls(repo)
        .await
        .map_err(|e| format!("listing open PRs: {e} — the previous mirror is kept"))?;
    let specs = refspecs(&same_repo_prs(repo, &pulls));
    let container = container_name("refresh");
    let credential = GitCredential::prepare(
        &ctx.image,
        repo,
        ctx.gh.token_for_git().await.expose(),
        &container,
    )
    .await;
    let args = refresh_args(
        repo,
        &ctx.image,
        &container,
        credential.as_ref().map(|c| c.volume.name()),
        &fetch_url(repo),
        &specs,
    );
    let ran = run_container(&container, &args, REFRESH_TIMEOUT).await;
    // Right after the refresh, on success and failure alike: the container that
    // mounted it is gone (`--rm`, or removed by `run_container`).
    if let Some(credential) = credential {
        credential.volume.remove().await;
    }
    let text = ran.map_err(|e| format!("the refresh container failed: {e}"))?;
    parse_report(&text).ok_or_else(|| "the refresh reported nothing usable".to_string())
}

/// What sits under `repo`'s mirror volume name.
async fn volume_state(repo: &str) -> VolumeState {
    let inspected =
        docker::docker_stdout(&volume_inspect_args(repo), docker::CLI_TIMEOUT, OUTPUT_CAP)
            .await
            .ok();
    classify_volume(inspected.as_deref(), repo)
}

/// Whether any container, running or not, mounts `volume`.
async fn mounted(volume: &str) -> anyhow::Result<bool> {
    let out =
        docker::docker_stdout(&mounted_by_args(volume), docker::CLI_TIMEOUT, OUTPUT_CAP).await?;
    Ok(out.split('\n').any(|l| !l.trim().is_empty()))
}

/// One row of [`volume_ls_args`].
#[derive(Debug, Deserialize)]
struct ListedVolume {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Repo", default)]
    repo: String,
}

/// Every mirror volume as `(name, repo)`, from [`volume_ls_args`].
async fn list_volumes() -> anyhow::Result<Vec<(String, String)>> {
    let text = docker::docker_stdout(
        &volume_ls_args(),
        docker::CLI_TIMEOUT,
        untrusted::OUTPUT_CAP,
    )
    .await?;
    Ok(parse_volumes(&text))
}

/// Parse the mirror-volume listing. A row without a repo label, or whose name is
/// outside Docker's alphabet, is skipped: it is not a mirror faber made.
fn parse_volumes(text: &str) -> Vec<(String, String)> {
    let rows = untrusted::json_lines::<ListedVolume>(text, "git mirror: volume listing");
    untrusted::accept(rows, "git mirror: volume listing", |row| {
        if row.repo.is_empty() {
            Ok(None)
        } else if untrusted::object_name(&row.name) {
            Ok(Some((row.name, row.repo)))
        } else {
            Err("a volume name is outside Docker's own alphabet")
        }
    })
}

/// Run a throwaway container to completion and return its output, removing it
/// by name if the run did not finish.
async fn run_container(
    container: &str,
    args: &[String],
    limit: Duration,
) -> anyhow::Result<String> {
    match docker::docker_stdout(args, limit, OUTPUT_CAP).await {
        Ok(out) => Ok(out),
        Err(e) => {
            if let Err(rm) = docker::docker_rm(&["rm", "-f", container]).await {
                warn!("git mirror: removing container {container}: {rm}");
            }
            Err(e)
        }
    }
}

/// Refresh every served repo's mirror now and then every [`REFRESH_INTERVAL`]
/// (ADR-0035 §1). Spawned at boot only when a profile has `git_mirror = true`.
pub async fn run_loop(orch: Arc<crate::reconcile::Orchestrator>) {
    let mirrors = global();
    info!(
        "git mirror: on — refreshing each watched repo's mirror every {REFRESH_INTERVAL:?} and \
         on every new push or pull_request run (ADR-0035)"
    );
    loop {
        match orch.served_repos().await {
            Ok(repos) => {
                for repo in repos {
                    mirrors.request_refresh(&repo);
                }
            }
            Err(e) => warn!("git mirror: could not list the watched repos ({e}) — retrying later"),
        }
        tokio::time::sleep(REFRESH_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::github::{PullHead, PullHeadRepo};
    use std::path::Path;

    fn mirror_volumes() -> Vec<(String, String)> {
        ["owner/demo", "owner/other"]
            .iter()
            .map(|r| (volume_name(r), r.to_string()))
            .collect()
    }

    /// One listing that leaves every repo out — what a short GitHub search that
    /// claims to be whole looks like — deletes no mirror, and nor do more of
    /// them until they span the grace.
    #[test]
    fn a_repo_missing_from_one_listing_keeps_its_mirror() {
        let volumes = mirror_volumes();
        let mut misses = HashMap::new();
        let t0 = 1_000_000;
        assert!(unserved_due(&mut misses, &volumes, &[], true, t0).is_empty());
        // Many passes in quick succession are still within the grace.
        for i in 1..10 {
            assert!(unserved_due(&mut misses, &volumes, &[], true, t0 + i).is_empty());
        }
        // An hour on, missing every time: both go.
        let due = unserved_due(&mut misses, &volumes, &[], true, t0 + UNSERVED_GRACE_MS);
        assert_eq!(due, volumes);
    }

    /// Time alone is not enough either: the misses must be consecutive passes.
    #[test]
    fn a_mirror_goes_only_after_enough_consecutive_misses() {
        let volumes = mirror_volumes();
        let served = vec!["owner/demo".to_string()];
        let mut misses = HashMap::new();
        let t0 = 1_000_000;
        for pass in 0..UNSERVED_PASSES - 1 {
            let at = t0 + u64::from(pass) * UNSERVED_GRACE_MS;
            assert!(unserved_due(&mut misses, &volumes, &served, true, at).is_empty());
        }
        let at = t0 + u64::from(UNSERVED_PASSES) * UNSERVED_GRACE_MS;
        assert_eq!(
            unserved_due(&mut misses, &volumes, &served, true, at),
            vec![(volume_name("owner/other"), "owner/other".to_string())],
            "only the repo that is missing goes; the served one stays"
        );
    }

    /// A listing that names the repo again ends its run of misses.
    #[test]
    fn a_repo_seen_again_starts_its_grace_over() {
        let volumes = mirror_volumes();
        let all = vec!["owner/demo".to_string(), "owner/other".to_string()];
        let mut misses = HashMap::new();
        let t0 = 1_000_000;
        for pass in 0..UNSERVED_PASSES - 1 {
            unserved_due(&mut misses, &volumes, &[], true, t0 + u64::from(pass));
        }
        assert!(unserved_due(&mut misses, &volumes, &all, true, t0 + 10).is_empty());
        assert!(misses.is_empty(), "a served repo carries no miss");
        assert!(
            unserved_due(&mut misses, &volumes, &[], true, t0 + UNSERVED_GRACE_MS * 2).is_empty(),
            "the first miss after being seen starts a new run"
        );
    }

    /// With mirrors off nothing is discovery: every mirror goes at once.
    #[test]
    fn turning_mirrors_off_deletes_every_mirror_at_once() {
        let volumes = mirror_volumes();
        let mut misses = HashMap::new();
        unserved_due(&mut misses, &volumes, &[], true, 1);
        assert_eq!(unserved_due(&mut misses, &volumes, &[], false, 2), volumes);
        assert!(misses.is_empty());
    }

    /// The base repo every PR below targets: `owner/demo`, id 100.
    const BASE_ID: u64 = 100;

    fn pull(number: u64, head: Option<(u64, &str)>) -> OpenPull {
        OpenPull {
            number,
            head: PullHead {
                repo: head.map(|(id, name)| PullHeadRepo {
                    id,
                    full_name: name.to_string(),
                }),
            },
            base: PullHead {
                repo: Some(PullHeadRepo {
                    id: BASE_ID,
                    full_name: "owner/demo".to_string(),
                }),
            },
        }
    }

    // --- the refspec: no fork refs, ever ------------------------------------

    #[test]
    fn the_refspec_is_branches_tags_and_listed_heads_only() {
        assert_eq!(
            refspecs(&[]),
            vec!["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"]
        );
        assert_eq!(
            refspecs(&[7, 3, 7]),
            vec![
                "+refs/heads/*:refs/heads/*",
                "+refs/tags/*:refs/tags/*",
                "+refs/pull/3/head:refs/pull/3/head",
                "+refs/pull/7/head:refs/pull/7/head",
            ]
        );
    }

    #[test]
    fn the_refspec_never_names_a_pattern_over_pull_refs_or_a_merge_ref() {
        for spec in refspecs(&[1, 2, 3]) {
            assert!(!spec.contains("refs/pull/*"), "{spec}");
            assert!(!spec.contains("/merge"), "{spec}");
            assert!(spec != "+refs/*:refs/*", "{spec}");
        }
    }

    #[test]
    fn fork_heads_never_reach_the_refspec() {
        let pulls = vec![
            pull(1, Some((BASE_ID, "owner/demo"))),
            pull(2, Some((7, "stranger/demo"))),
            pull(3, None), // the fork was deleted
            pull(4, Some((BASE_ID, "Owner/Demo"))),
            pull(5, Some((8, "owner/demo-fork"))),
            pull(6, Some((9, "owner/demo/extra"))),
            // The base's name on another repo's id: the id is authoritative.
            pull(7, Some((10, "owner/demo"))),
            // The base's id under another name: refused too, both must hold.
            pull(8, Some((BASE_ID, "stranger/demo"))),
        ];
        let same = same_repo_prs("owner/demo", &pulls);
        assert_eq!(same, vec![1, 4]);
        let mut no_base = pull(9, Some((BASE_ID, "owner/demo")));
        no_base.base.repo = None;
        assert!(same_repo_prs("owner/demo", &[no_base]).is_empty());
        let specs = refspecs(&same);
        for fork in [2, 3, 5, 6, 7, 8] {
            assert!(
                !specs
                    .iter()
                    .any(|s| s.contains(&format!("refs/pull/{fork}/"))),
                "PR {fork} is not same-repo and must never be fetched"
            );
        }
    }

    #[test]
    fn only_a_labelled_volume_of_this_repo_is_mounted() {
        let ours = r#"{"org.faber.owned":"true","org.faber.kind":"git-mirror","org.faber.repo":"owner/demo"}"#;
        assert_eq!(classify_volume(Some(ours), "owner/demo"), VolumeState::Ours);
        assert_eq!(
            classify_volume(Some(ours), "owner/other"),
            VolumeState::Foreign
        );
        // What Docker conjures at a `-v` of a missing volume: no labels at all.
        assert_eq!(
            classify_volume(Some("null\n"), "owner/demo"),
            VolumeState::Foreign
        );
        assert_eq!(
            classify_volume(Some("{}"), "owner/demo"),
            VolumeState::Foreign
        );
        let cache =
            r#"{"org.faber.owned":"true","org.faber.kind":"cache","org.faber.repo":"owner/demo"}"#;
        assert_eq!(
            classify_volume(Some(cache), "owner/demo"),
            VolumeState::Foreign
        );
        assert_eq!(classify_volume(None, "owner/demo"), VolumeState::Missing);
        assert_eq!(
            classify_volume(Some("garbage"), "owner/demo"),
            VolumeState::Foreign
        );
    }

    #[test]
    fn a_volume_faber_did_not_label_blocks_the_mirror_and_is_never_removed() {
        assert_eq!(
            plan_refresh(VolumeState::Ours, "owner/demo"),
            RefreshPlan::Refresh
        );
        assert_eq!(
            plan_refresh(VolumeState::Missing, "owner/demo"),
            RefreshPlan::FirstClone
        );
        let RefreshPlan::Blocked(fault) = plan_refresh(VolumeState::Foreign, "owner/demo") else {
            panic!("a foreign volume must block the mirror");
        };
        assert!(fault.contains("remove volume faber-git-owner-demo by hand or rename it"));
        assert!(fault.contains("faber will then build its mirror"));

        // Two `volume rm` calls exist, both behind a `org.faber.kind` filter an
        // unlabelled volume can never appear in: `delete` (listings filtered on
        // `git-mirror`) and `reap_credentials` (`credential_ls_args`, filtered on
        // `git-auth`, the refresh's own short-lived credential volume).
        let src = include_str!("git_mirror.rs");
        let code = &src[..src.find("#[cfg(test)]").unwrap()];
        let rm = format!("{:?}, {:?}", "volume", "rm");
        assert_eq!(
            code.matches(&rm).count(),
            2,
            "a new volume removal appeared; it must read a faber label"
        );
        let reap_at = code.find("async fn reap_credentials(").unwrap();
        let delete_at = code.find("async fn delete(").unwrap();
        let first = code.find(&rm).unwrap();
        assert!(first > reap_at.min(delete_at));
        let (reap_end, delete_end) = (
            code[reap_at..].find("\n    }\n").unwrap() + reap_at,
            code[delete_at..].find("\n    }\n").unwrap() + delete_at,
        );
        for at in code.match_indices(&rm).map(|(i, _)| i) {
            assert!(
                (reap_at..reap_end).contains(&at) || (delete_at..delete_end).contains(&at),
                "a volume removal outside delete / reap_credentials"
            );
        }
        assert!(code[reap_at..reap_end].contains("credential_ls_args()"));
    }

    // --- the runner's mount ---------------------------------------------------

    #[test]
    fn no_mirror_adds_no_flag_and_no_variable() {
        assert!(runner_args(None).is_empty());
    }

    #[test]
    fn a_mirror_is_mounted_read_only_outside_work_with_its_variable() {
        let a = runner_args(Some("faber-git-owner-demo"));
        assert_eq!(
            a,
            vec![
                "-v",
                "faber-git-owner-demo:/runner/_git:ro",
                "-e",
                "FABER_GIT_MIRROR=/runner/_git"
            ]
        );
        assert!(!a.iter().any(|s| s.contains("_work")));
    }

    #[test]
    fn the_volume_name_is_per_repo_and_docker_legal() {
        assert_eq!(volume_name("owner/demo"), "faber-git-owner-demo");
        assert_ne!(volume_name("owner/a"), volume_name("owner/b"));
        assert!(untrusted::object_name(&volume_name("Weird/re po!")));
        // Not the cache's namespace, and not a per-job kind's prefix.
        assert_ne!(
            volume_name("owner/demo"),
            docker::cache_volume_name("owner/demo")
        );
    }

    // --- refresh scheduling ---------------------------------------------------

    #[test]
    fn requests_during_a_refresh_collapse_into_one_follow_up() {
        let mut c = Coalesce::default();
        assert!(c.request(), "the first request starts a refresh");
        assert!(!c.request(), "a second waits");
        assert!(!c.request(), "and a third folds into the same follow-up");
        assert!(c.finish(), "one follow-up runs");
        assert!(c.running());
        assert!(!c.finish(), "and nothing after it");
        assert!(!c.running());
        assert!(c.request(), "an idle repo starts again");
    }

    #[test]
    fn only_new_push_and_pull_request_runs_trigger() {
        let mut seen = HashSet::new();
        assert!(note_runs(&mut seen, [(1, "push"), (2, "schedule")]));
        assert!(
            !note_runs(&mut seen, [(1, "push"), (2, "schedule")]),
            "seen already"
        );
        assert!(!note_runs(
            &mut seen,
            [(1, "push"), (3, "workflow_dispatch")]
        ));
        assert!(note_runs(&mut seen, [(1, "push"), (4, "pull_request")]));
        assert!(!note_runs(&mut seen, []), "an empty poll triggers nothing");
        assert!(seen.is_empty(), "and forgets what is no longer active");
    }

    #[test]
    fn pruning_is_at_most_daily_and_never_before_a_first_refresh() {
        assert!(!prune_due(None, u64::MAX));
        assert!(!prune_due(Some(1_000), 1_000 + PRUNE_INTERVAL_MS - 1));
        assert!(prune_due(Some(1_000), 1_000 + PRUNE_INTERVAL_MS));
    }

    // --- disk ---------------------------------------------------------------------

    #[test]
    fn the_first_clone_is_weighed_against_its_floor() {
        // No floor: it builds, whatever the disk says.
        assert_eq!(
            first_clone(None, Some(1), Some(10 << 20)),
            FirstClone::Build
        );
        assert_eq!(
            first_clone(Some(0), Some(1), Some(10 << 20)),
            FirstClone::Build
        );
        // 12 GiB free, 1 GiB repo, 10 GiB floor: 11 GiB left — builds.
        assert_eq!(
            first_clone(Some(10), Some(12 * 1024), Some(1024 * 1024)),
            FirstClone::Build
        );
        // Exactly at the floor afterwards builds; a MiB under waits.
        assert_eq!(
            first_clone(Some(10), Some(11 * 1024), Some(1024 * 1024)),
            FirstClone::Build
        );
        assert!(matches!(
            first_clone(Some(10), Some(11 * 1024 - 1), Some(1024 * 1024)),
            FirstClone::Wait(_)
        ));
        // A size GitHub did not report waits; a disk nobody could read builds.
        assert!(matches!(
            first_clone(Some(10), Some(100 << 10), None),
            FirstClone::Wait(_)
        ));
        assert_eq!(first_clone(Some(10), None, Some(1)), FirstClone::Build);
    }

    #[test]
    fn the_clone_floor_is_the_reclaim_floor_when_that_is_higher() {
        // #348: the default 25 GiB reclaim floor, with an admission floor of,
        // say, 10 GiB. A clone allowed down to the 10 GiB admission floor started
        // a reclamation at 25 GiB that deleted it again.
        assert_eq!(clone_floor_gib(25, Some(10)), Some(25));
        assert_eq!(clone_floor_gib(25, None), Some(25));
        // An admission floor above the reclaim floor still holds.
        assert_eq!(clone_floor_gib(10, Some(30)), Some(30));
        // Reclamation off: the admission floor alone, and with neither, none.
        assert_eq!(clone_floor_gib(0, Some(10)), Some(10));
        assert_eq!(clone_floor_gib(0, None), None);
        assert_eq!(clone_floor_gib(0, Some(0)), None);
    }

    #[test]
    fn first_clones_run_one_at_a_time() {
        // #348: many repos weighed the same free space at the same tick and
        // all cloned into it. The lock is taken in the first-clone arm only, and
        // before the disk is read.
        let code = include_str!("git_mirror.rs");
        let arm = code.find("RefreshPlan::FirstClone => {\n").unwrap();
        let lock = code[arm..].find("first_clones.lock().await").unwrap();
        let read = code[arm..].find("docker_disk_free()").unwrap();
        assert!(lock < read, "the lock is taken before the disk is weighed");
    }

    #[test]
    fn a_repack_follows_only_once_enough_has_built_up() {
        let r = |loose, packs| Report {
            loose,
            packs,
            ..Report::default()
        };
        assert!(!needs_repack(&r(10, 2)));
        assert!(needs_repack(&r(REPACK_LOOSE, 1)));
        assert!(needs_repack(&r(0, REPACK_PACKS)));
    }

    // --- the report -----------------------------------------------------------

    #[test]
    fn a_complete_report_parses() {
        let r = parse_report("fetch ok\nloose 12\npacks 3\nkib 4096\ncomplete yes\nend\n").unwrap();
        assert_eq!(
            r,
            Report {
                fetched: true,
                error: None,
                loose: 12,
                packs: 3,
                kib: 4096,
                complete: true
            }
        );
        let r = parse_report(
            "fetch failed\nerror fatal: could not read Username\nloose 0\npacks 1\nkib 9\ncomplete yes\nend\n",
        )
        .unwrap();
        assert!(!r.fetched);
        assert!(r.complete, "a stale mirror is still a usable one");
        assert_eq!(r.error.as_deref(), Some("fatal: could not read Username"));
    }

    #[test]
    fn a_report_cut_short_or_garbled_is_refused() {
        assert_eq!(parse_report("fetch ok\nloose 1\n"), None, "no end line");
        assert_eq!(parse_report("loose 1\nend\n"), None, "no fetch line");
        assert_eq!(parse_report("fetch ok\nkib lots\nend\n"), None);
        assert_eq!(parse_report(""), None);
    }

    // --- argv -----------------------------------------------------------------------

    #[test]
    fn the_refresh_container_carries_no_secret_and_writes_only_its_volume() {
        let specs = refspecs(&[5]);
        let a = refresh_args(
            "owner/demo",
            "ghcr.io/owner/my-runner:latest",
            "faber-gitwork-refresh-1-0",
            Some("faber-gitauth-refresh-1-0"),
            "https://github.com/owner/demo",
            &specs,
        );
        assert!(a
            .windows(2)
            .any(|w| w == ["-v", "faber-git-owner-demo:/mirror"]));
        assert!(a
            .windows(2)
            .any(|w| w == ["-v", "faber-gitauth-refresh-1-0:/faber/git:ro"]));
        assert!(!a.iter().any(|s| s == "-e"), "no variable carries anything");
        assert!(!a.iter().any(|s| s == "--privileged"));
        assert!(a.iter().any(|s| s == "--pull=never"));
        assert!(a.iter().any(|s| s == "--rm"));
        // The refspecs are arguments after the script, never script text.
        let script = a.iter().position(|s| s == REFRESH_SCRIPT).unwrap();
        let image = a
            .iter()
            .position(|s| s == "ghcr.io/owner/my-runner:latest")
            .unwrap();
        assert!(image < script);
        assert_eq!(&a[a.len() - specs.len()..], specs.as_slice());
        assert!(a.contains(&credential_helper("/faber/git/credentials")));
        // Labelled faber's, as a kind the sweep never reaps.
        assert!(a.contains(&format!("{LABEL_KIND}={KIND_GIT_MIRROR_WORK}")));
    }

    #[test]
    fn maintenance_runs_offline() {
        let a = maintenance_args("owner/demo", "img", "c", PRUNE_SCRIPT);
        assert!(a.windows(2).any(|w| w == ["--network", "none"]));
        assert!(a
            .windows(2)
            .any(|w| w == ["-v", "faber-git-owner-demo:/mirror"]));
    }

    #[test]
    fn the_volume_is_created_with_labels_and_listed_by_them() {
        let a = volume_create_args("owner/demo");
        assert!(a.contains(&format!("{LABEL_KIND}={KIND_GIT_MIRROR}")));
        assert!(a.contains(&format!("{LABEL_REPO}=owner/demo")));
        assert_eq!(a.last().unwrap(), "faber-git-owner-demo");
        let ls = volume_ls_args();
        assert!(ls.contains(&format!("label={LABEL_KIND}={KIND_GIT_MIRROR}")));
    }

    #[test]
    fn the_listing_keeps_only_labelled_well_named_mirrors() {
        let text = concat!(
            r#"{"Name":"faber-git-owner-demo","Repo":"owner/demo"}"#,
            "\n",
            r#"{"Name":"stray","Repo":""}"#,
            "\n",
            r#"{"Name":"/etc","Repo":"owner/x"}"#,
            "\n",
        );
        assert_eq!(
            parse_volumes(text),
            vec![("faber-git-owner-demo".to_string(), "owner/demo".to_string())]
        );
    }

    #[test]
    fn no_refresh_or_maintenance_argv_mounts_a_host_path() {
        // faber's Docker shares no host folder (ADR-0026): every `-v` source must
        // be a named volume, or the container gets an empty directory the daemon
        // made inside the VM.
        let container = container_name("refresh");
        let cred = credential_volume_name(&container);
        let refresh = refresh_args(
            "owner/demo",
            "img",
            &container,
            Some(&cred),
            "https://github.com/owner/demo",
            &refspecs(&[]),
        );
        let maintenance = maintenance_args("owner/demo", "img", "c", PRUNE_SCRIPT);
        for (what, argv, n) in [("refresh", &refresh, 2), ("maintenance", &maintenance, 1)] {
            let sources: Vec<&str> = argv
                .windows(2)
                .filter(|w| w[0] == "-v" || w[0] == "--volume" || w[0] == "--mount")
                .map(|w| w[1].split(':').next().unwrap_or_default())
                .collect();
            assert_eq!(sources.len(), n, "{what}: {sources:?}");
            for src in sources {
                assert!(
                    untrusted::object_name(src) && !src.starts_with(['/', '.', '~']),
                    "{what} mounts host path {src:?}"
                );
            }
        }
    }

    #[test]
    fn a_leftover_credential_fill_is_found_by_its_label_and_only_once_stopped() {
        // #348: the fill removes its container after it exits, not by `--rm`, so
        // a removal that failed is found here by its kind, never its name.
        let a = credential_fill_ps_args();
        assert!(a.contains(&format!("label={LABEL_KIND}={KIND_GIT_AUTH}")));
        for status in ["status=exited", "status=created", "status=dead"] {
            assert!(a.iter().any(|s| s == status), "{status}");
        }
        assert!(!a.iter().any(|s| s == "status=running"));
        let row = r#"{"Name":"faber-gitwork-credential-5-0","Repo":"owner/demo"}"#;
        assert_eq!(
            parse_volumes(row),
            vec![(
                "faber-gitwork-credential-5-0".to_string(),
                "owner/demo".to_string()
            )]
        );
    }

    #[test]
    fn the_credential_is_a_labelled_volume_found_by_its_label() {
        // The file a refresh reads, in git-credential format.
        assert_eq!(
            credential_file("tok"),
            "username=x-access-token\npassword=tok\n"
        );
        // Its own volume, distinct from the mirror volumes' `faber-git-` names,
        // and listed for the janitor by its kind, never by name (AGENTS.md).
        let name = credential_volume_name("faber-gitwork-refresh-5-0");
        assert_eq!(name, "faber-gitauth-refresh-5-0");
        assert!(!name.starts_with("faber-git-"));
        let ls = credential_ls_args();
        assert!(ls.contains(&format!("label={LABEL_KIND}={KIND_GIT_AUTH}")));
        assert!(!ls.iter().any(|a| a.starts_with("name=")));
        // A listed leftover parses to its name and repo.
        let text = r#"{"Name":"faber-gitauth-refresh-5-0","Repo":"owner/demo"}"#;
        assert_eq!(
            parse_volumes(text),
            vec![(
                "faber-gitauth-refresh-5-0".to_string(),
                "owner/demo".to_string()
            )]
        );
    }

    // --- leases ---------------------------------------------------------------------

    #[tokio::test]
    async fn a_mirror_is_leased_only_once_ready_and_never_while_held_exclusively() {
        let m = Mirrors::default();
        assert!(m.lease("owner/demo").is_none(), "unknown repo");
        m.with_state("owner/demo", |s| s.ready = false);
        assert!(m.lease("owner/demo").is_none(), "not refreshed yet");
        m.with_state("owner/demo", |s| s.ready = true);
        let lease = m.lease("owner/demo").expect("ready");
        assert_eq!(lease.volume(), "faber-git-owner-demo");
        let (mounts, _) = m.handles("owner/demo");
        assert!(
            mounts.clone().try_write_owned().is_err(),
            "a prune cannot start under a leased mirror"
        );
        drop(lease);
        let exclusive = mounts.clone().try_write_owned().expect("idle mirror");
        assert!(m.lease("owner/demo").is_none(), "no lease while pruning");
        drop(exclusive);
        assert!(m.lease("owner/demo").is_some());
    }

    #[test]
    fn the_view_lists_only_mirrors_something_happened_to() {
        let m = Mirrors::default();
        m.with_state("owner/a", |_| ());
        m.with_state("owner/b", |s| {
            s.last_attempt_ms = Some(5);
            s.kib = Some(64);
            s.last_error = Some("boom".into());
        });
        let v = m.view();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].repo, "owner/b");
        assert_eq!(v[0].size_kib, Some(64));
        assert_eq!(v[0].last_error.as_deref(), Some("boom"));
        assert!(!v[0].ready);
    }

    #[test]
    fn an_unconfigured_process_requests_nothing() {
        // `observe_runs` and `request_refresh` need `'static`; the global is
        // unconfigured in tests, so both must be no-ops that record nothing.
        let g = global();
        assert!(!g.enabled());
        g.observe_runs("owner/demo", [(1, "push")]);
        g.request_refresh("owner/demo");
        assert!(g.view().is_empty());
        assert!(g.lease("owner/demo").is_none());
    }

    // --- real git: a refresh, and a borrowing shallow fetch ---------------------

    /// Run `git` in `dir`, panicking with its stderr on failure.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("git runs");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// What lets a test refresh from a `file://` origin. The scripts speak only
    /// https, and `protocol.file.allow` outranks their `protocol.allow = never`
    /// for that one transport; faber never sets it.
    const FILE_PROTOCOL: [(&str, &str); 3] = [
        ("GIT_CONFIG_COUNT", "1"),
        ("GIT_CONFIG_KEY_0", "protocol.file.allow"),
        ("GIT_CONFIG_VALUE_0", "always"),
    ];

    fn run_script(script: &str, mirror: &Path, rest: &[&str], env: &[(&str, &str)]) -> String {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(script)
            .arg("sh")
            .arg(mirror)
            .args(rest)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .envs(env.iter().copied())
            .output()
            .expect("sh runs");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn refresh_with(mirror: &Path, url: &str, specs: &[String], env: &[(&str, &str)]) -> Report {
        let mut rest = vec![url, "-"];
        rest.extend(specs.iter().map(String::as_str));
        let text = run_script(REFRESH_SCRIPT, mirror, &rest, env);
        parse_report(&text).unwrap_or_else(|| panic!("unparsable report: {text}"))
    }

    fn refresh_locally(mirror: &Path, url: &str, specs: &[String]) -> Report {
        refresh_with(mirror, url, specs, &FILE_PROTOCOL)
    }

    /// A fresh directory for one test's repositories.
    fn scratch(what: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!(
            "faber-gitmirror-{what}-{}-{}",
            std::process::id(),
            crate::audit::now_ms()
        ));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// A bare `<name>.git` under `root` whose `main` holds one commit of
    /// `content`, and that commit.
    fn origin_with_main(root: &Path, name: &str, content: &str) -> (std::path::PathBuf, String) {
        let origin = root.join(format!("{name}.git"));
        let work = root.join(format!("{name}-work"));
        std::fs::create_dir_all(&work).unwrap();
        git(root, &["init", "-q", "--bare", origin.to_str().unwrap()]);
        git(&work, &["init", "-q", "-b", "main"]);
        std::fs::write(work.join("f.txt"), content).unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", content]);
        git(&work, &["push", "-q", origin.to_str().unwrap(), "main"]);
        let head = git(&work, &["rev-parse", "HEAD"]);
        (origin, head)
    }

    /// Plant in the bare repository `repo` what a writer of its volume could:
    /// a rewrite of `url` to `evil`, a hooks path of its own, and a
    /// `reference-transaction` hook in both places that creates `pwned`.
    fn plant(repo: &Path, url: &str, evil: &Path, pwned: &Path) {
        use std::io::Write as _;
        use std::os::unix::fs::PermissionsExt as _;
        let planted_hooks = repo.join("planted-hooks");
        let mut config = std::fs::OpenOptions::new()
            .append(true)
            .open(repo.join("config"))
            .unwrap();
        write!(
            config,
            "[url \"file://{}\"]\n\tinsteadOf = {url}\n[core]\n\thooksPath = {}\n",
            evil.display(),
            planted_hooks.display()
        )
        .unwrap();
        for dir in [repo.join("hooks"), planted_hooks] {
            std::fs::create_dir_all(&dir).unwrap();
            let hook = dir.join("reference-transaction");
            std::fs::write(&hook, format!("#!/bin/sh\ntouch '{}'\n", pwned.display())).unwrap();
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
    }

    /// The config every script leaves, as `git config --list` prints it.
    const TEMPLATE_CONFIG: [&str; 7] = [
        "core.repositoryformatversion=0",
        "core.bare=true",
        "core.sharedrepository=world",
        "core.logallrefupdates=false",
        "core.hookspath=/dev/null",
        "gc.auto=0",
        "fetch.fsckobjects=true",
    ];

    fn config_of(repo: &Path) -> Vec<String> {
        let file = repo.join("config");
        git(
            repo,
            &["config", "--file", file.to_str().unwrap(), "--list"],
        )
        .lines()
        .map(String::from)
        .collect()
    }

    /// The control first: the planted repository steers a plain `git fetch` to
    /// the planted origin and runs its hook. Then the same plant, in the mirror,
    /// steers nothing: the refresh fetches the real origin, runs no hook, and
    /// leaves the template config and no `hooks/`, `commondir` or alternates.
    #[test]
    fn a_refresh_resets_the_mirror_config_and_drops_its_hooks() {
        if !has_git() {
            eprintln!("git not installed — skipping");
            return;
        }
        let root = scratch("reset");
        let (origin, _) = origin_with_main(&root, "origin", "real\n");
        let (evil, evil_main) = origin_with_main(&root, "evil", "planted\n");
        let url = format!("file://{}", origin.display());
        let pwned = root.join("pwned");

        let control = root.join("control.git");
        git(&root, &["init", "-q", "--bare", control.to_str().unwrap()]);
        plant(&control, &url, &evil, &pwned);
        git(
            &control,
            &[
                "-c",
                "protocol.file.allow=always",
                "fetch",
                "-q",
                &url,
                "+refs/heads/*:refs/heads/*",
            ],
        );
        assert!(pwned.exists(), "the planted hook must run in the control");
        assert_eq!(git(&control, &["rev-parse", "refs/heads/main"]), evil_main);
        std::fs::remove_file(&pwned).unwrap();

        let mirror = root.join("mirror");
        std::fs::create_dir_all(&mirror).unwrap();
        assert!(refresh_locally(&mirror, &url, &refspecs(&[])).fetched);
        plant(&mirror, &url, &evil, &pwned);
        // A `commondir` would move config, hooks and refs to another repository,
        // and an alternates file would borrow its objects.
        let common = root.join("common.git");
        git(&root, &["init", "-q", "--bare", common.to_str().unwrap()]);
        plant(&common, &url, &evil, &pwned);
        std::fs::write(mirror.join("commondir"), format!("{}\n", common.display())).unwrap();
        std::fs::write(
            mirror.join("objects/info/alternates"),
            format!("{}/objects\n", evil.display()),
        )
        .unwrap();
        let work = root.join("origin-work");
        std::fs::write(work.join("f.txt"), "real, again\n").unwrap();
        git(&work, &["commit", "-q", "-am", "two"]);
        git(&work, &["push", "-q", origin.to_str().unwrap(), "main"]);
        let real_main = git(&work, &["rev-parse", "HEAD"]);

        let r = refresh_locally(&mirror, &url, &refspecs(&[]));
        assert!(r.fetched, "{r:?}");
        assert!(!pwned.exists(), "a planted hook ran during the refresh");
        assert_eq!(git(&mirror, &["rev-parse", "refs/heads/main"]), real_main);
        for gone in ["hooks", "commondir", "objects/info/alternates"] {
            assert!(!mirror.join(gone).exists(), "{gone} survived the refresh");
        }
        assert_eq!(config_of(&mirror), TEMPLATE_CONFIG);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// `gc` runs `reference-transaction` as it packs refs (the control), so the
    /// prune resets the repository first, and the repack does too.
    #[test]
    fn maintenance_resets_the_mirror_before_git_runs() {
        if !has_git() {
            eprintln!("git not installed — skipping");
            return;
        }
        let root = scratch("maintain");
        let (origin, _) = origin_with_main(&root, "origin", "real\n");
        let (evil, _) = origin_with_main(&root, "evil", "planted\n");
        let url = format!("file://{}", origin.display());
        let pwned = root.join("pwned");

        let control = root.join("control.git");
        git(&root, &["init", "-q", "--bare", control.to_str().unwrap()]);
        git(
            &control,
            &[
                "-c",
                "protocol.file.allow=always",
                "fetch",
                "-q",
                &url,
                "+refs/heads/*:refs/heads/*",
            ],
        );
        plant(&control, &url, &evil, &pwned);
        git(&control, &["gc", "--prune=now", "-q"]);
        assert!(
            pwned.exists(),
            "the planted hook must run in the control's gc"
        );
        std::fs::remove_file(&pwned).unwrap();

        let mirror = root.join("mirror");
        std::fs::create_dir_all(&mirror).unwrap();
        assert!(refresh_locally(&mirror, &url, &refspecs(&[])).fetched);
        for script in [PRUNE_SCRIPT, REPACK_SCRIPT] {
            plant(&mirror, &url, &evil, &pwned);
            let text = run_script(script, &mirror, &[], &[]);
            assert!(text.ends_with("end\n"), "{text}");
            assert!(!pwned.exists(), "a planted hook ran during maintenance");
            assert!(!mirror.join("hooks").exists());
            assert_eq!(config_of(&mirror), TEMPLATE_CONFIG);
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// Without the test's allowance the scripts speak only https: a URL that a
    /// planted rewrite (or anything else) turned into `file://`, `ext::` or
    /// `ssh://` is refused before any transport runs.
    #[test]
    fn a_refresh_fetches_over_https_only() {
        if !has_git() {
            eprintln!("git not installed — skipping");
            return;
        }
        let root = scratch("https");
        let (origin, _) = origin_with_main(&root, "origin", "real\n");
        let mirror = root.join("mirror");
        std::fs::create_dir_all(&mirror).unwrap();
        for url in [
            format!("file://{}", origin.display()),
            format!("ext::sh -c touch% {}", root.join("pwned").display()),
        ] {
            let r = refresh_with(&mirror, &url, &refspecs(&[]), &[]);
            assert!(!r.fetched, "{url}: {r:?}");
            assert!(
                r.error
                    .as_deref()
                    .is_some_and(|e| e.contains("not allowed")),
                "{url}: {r:?}"
            );
        }
        assert!(!root.join("pwned").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// git asks the helper for whatever host it is about to send a request to;
    /// only `https://github.com` gets the credential, and only on `get`.
    #[test]
    fn the_credential_helper_answers_only_github_com() {
        let root = scratch("helper");
        let file = root.join("credentials");
        std::fs::write(&file, credential_file("tok")).unwrap();
        let helper = credential_helper(file.to_str().unwrap());
        let body = helper.strip_prefix('!').expect("a shell helper");
        let ask = |action: &str, request: &str| -> String {
            use std::io::Write as _;
            let mut child = std::process::Command::new("sh")
                .arg("-c")
                .arg(format!("{body} {action}"))
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .spawn()
                .expect("sh runs");
            // A helper that does not answer exits without reading its request.
            if let Err(e) = child.stdin.take().unwrap().write_all(request.as_bytes()) {
                assert_eq!(e.kind(), std::io::ErrorKind::BrokenPipe, "{e}");
            }
            let out = child.wait_with_output().unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        };
        let github = "protocol=https\nhost=github.com\n\n";
        assert_eq!(ask("get", github), credential_file("tok"));
        // What git 2.46 and later also send is read past, not mistaken for a host.
        assert_eq!(
            ask(
                "get",
                "capability[]=authtype\nprotocol=https\nhost=github.com\nwwwauth[]=Basic realm=\"GitHub\"\n"
            ),
            credential_file("tok")
        );
        for refused in [
            "protocol=https\nhost=evil.example\n\n",
            "protocol=http\nhost=github.com\n\n",
            "protocol=https\nhost=github.com:8443\n\n",
            "protocol=https\nhost=github.com.evil.example\n\n",
            "protocol=https\nhost=api.github.com\n\n",
            "host=github.com\n\n",
        ] {
            assert_eq!(ask("get", refused), "", "{refused:?}");
        }
        for action in ["store", "erase"] {
            assert_eq!(ask(action, github), "", "{action}");
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    fn has_git() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    #[test]
    fn a_refresh_takes_no_fork_refs_and_a_borrowing_fetch_transfers_nothing() {
        if !has_git() {
            eprintln!("git not installed — skipping");
            return;
        }
        let root = std::env::temp_dir().join(format!(
            "faber-gitmirror-it-{}-{}",
            std::process::id(),
            crate::audit::now_ms()
        ));
        let work = root.join("work");
        let origin = root.join("origin.git");
        let mirror = root.join("mirror");
        let ws = root.join("ws");
        for d in [&work, &mirror, &ws] {
            std::fs::create_dir_all(d).unwrap();
        }
        git(&root, &["init", "-q", "--bare", origin.to_str().unwrap()]);
        git(&work, &["init", "-q", "-b", "main"]);
        for i in 0..20 {
            std::fs::write(
                work.join(format!("f{i}.txt")),
                format!("content {i}\n").repeat(50),
            )
            .unwrap();
        }
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "one"]);
        std::fs::write(work.join("f0.txt"), "changed\n").unwrap();
        git(&work, &["commit", "-q", "-am", "two"]);
        let main = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["tag", "v1"]);
        // A same-repo PR head, a fork PR head with code of its own, and a merge ref.
        git(&work, &["checkout", "-q", "-b", "feature"]);
        std::fs::write(work.join("feature.txt"), "same-repo\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "feature"]);
        git(&work, &["checkout", "-q", "main"]);
        git(&work, &["checkout", "-q", "-b", "fork"]);
        std::fs::write(work.join("fork.txt"), "from outside the boundary\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "fork"]);
        let fork = git(&work, &["rev-parse", "HEAD"]);
        git(&work, &["checkout", "-q", "main"]);
        let o = origin.to_str().unwrap();
        git(&work, &["push", "-q", o, "main", "v1", "feature"]);
        git(
            &work,
            &[
                "push",
                "-q",
                o,
                "feature:refs/pull/1/head",
                "fork:refs/pull/2/head",
            ],
        );
        git(&work, &["push", "-q", o, "feature:refs/pull/1/merge"]);
        // GitHub-shaped: the fork's commit exists only under refs/pull/2/head.
        git(&work, &["branch", "-q", "-D", "fork"]);

        let url = format!("file://{}", origin.display());
        let r = refresh_locally(&mirror, &url, &refspecs(&[1]));
        assert!(r.fetched, "{r:?}");
        assert!(r.complete);
        let refs = git(&mirror, &["for-each-ref", "--format=%(refname)"]);
        assert!(refs.contains("refs/heads/main"));
        assert!(refs.contains("refs/tags/v1"));
        assert!(refs.contains("refs/pull/1/head"));
        assert!(
            !refs.contains("refs/pull/2/"),
            "a fork head entered: {refs}"
        );
        assert!(!refs.contains("/merge"), "a merge ref entered: {refs}");
        let fork_present = std::process::Command::new("git")
            .arg("-C")
            .arg(&mirror)
            .args(["cat-file", "-e", &fork])
            .status()
            .unwrap()
            .success();
        assert!(!fork_present, "the fork's objects must not be stored");

        // A borrowing workspace: the pre-checkout step, then checkout's own fetch.
        git(&ws, &["init", "-q"]);
        git(&ws, &["remote", "add", "origin", &url]);
        std::fs::write(
            ws.join(".git/objects/info/alternates"),
            format!("{}/objects\n", mirror.display()),
        )
        .unwrap();
        git(
            &ws,
            &[
                "-c",
                "protocol.version=2",
                "fetch",
                "-q",
                "--no-tags",
                "--depth=1",
                "origin",
                "+refs/heads/main:refs/remotes/origin/main",
            ],
        );
        git(
            &ws,
            &[
                "checkout",
                "-q",
                "--force",
                "-B",
                "main",
                "refs/remotes/origin/main",
            ],
        );
        assert_eq!(git(&ws, &["rev-parse", "HEAD"]), main);
        let counts = git(&ws, &["count-objects", "-v"]);
        let field = |k: &str| -> u64 {
            counts
                .lines()
                .find_map(|l| l.strip_prefix(&format!("{k}: ")))
                .unwrap()
                .parse()
                .unwrap()
        };
        assert_eq!(field("count"), 0, "{counts}");
        assert_eq!(field("in-pack"), 0, "nothing was transferred: {counts}");
        git(&ws, &["fsck", "--connectivity-only"]);

        // The control: the same fetch without the alternates line transfers the
        // commit's whole tree, so the zero above is the mirror's doing.
        let cold = root.join("cold");
        std::fs::create_dir_all(&cold).unwrap();
        git(&cold, &["init", "-q"]);
        git(&cold, &["remote", "add", "origin", &url]);
        git(
            &cold,
            &[
                "fetch",
                "-q",
                "--no-tags",
                "--depth=1",
                "origin",
                "+refs/heads/main:refs/remotes/origin/main",
            ],
        );
        let cold_counts = git(&cold, &["count-objects", "-v"]);
        let cold_objects: u64 = cold_counts
            .lines()
            .filter_map(|l| {
                l.strip_prefix("count: ")
                    .or_else(|| l.strip_prefix("in-pack: "))
            })
            .map(|v| v.parse::<u64>().unwrap())
            .sum();
        assert!(cold_objects > 20, "{cold_counts}");

        // The PR closes: its head goes on the next refresh; a deleted branch is pruned.
        git(&work, &["push", "-q", o, "--delete", "feature"]);
        let r = refresh_locally(&mirror, &url, &refspecs(&[]));
        assert!(r.fetched, "{r:?}");
        let refs = git(&mirror, &["for-each-ref", "--format=%(refname)"]);
        assert!(!refs.contains("refs/pull/1/head"), "{refs}");
        assert!(!refs.contains("refs/heads/feature"), "{refs}");
        assert!(refs.contains("refs/heads/main"));

        let _ = std::fs::remove_dir_all(&root);
    }
}
