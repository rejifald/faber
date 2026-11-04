//! Bounding the per-repo build cache by age and size — ADR-0024 §4, ADR-0055.
//!
//! ADR-0005 gave each repo a persistent cache volume and made bounding it the
//! consumer's job, since faber has no idea what is inside. On the host ADR-0024
//! was written for, that held for the layout the consumer still used and failed
//! for the one it had abandoned: over a third of a cache volume of tens of GB was
//! a few top-level files nothing had read or written for nearly three weeks, one
//! of them the remains of an interrupted write. Nothing was going to read them
//! again, and nothing was going to delete them either.
//!
//! Two limits, `[host] cache_max_age_days` and `[host] cache_max_gib`, both per
//! volume and both optional; an entry goes if it breaks either one that is set.
//! faber deletes each **top-level** entry of a cache volume in which nothing has
//! been read, written or changed for that many days — a file, or a directory with
//! everything in it — and, while a volume holds more than the size limit, its least
//! recently used entries, oldest first. An age bound alone cannot bound a cache in
//! use (ADR-0055). If either key is set the operator chose the policy and only the
//! set limits apply (`0` = none on that axis); if neither is, faber decides: it
//! deletes nothing on a healthy disk, and under the janitor's disk pressure it
//! deletes the globally least recently used entries until the floor plus a margin
//! is in reach ([`relieve`]). Whole entries only: faber does not know the
//! consumer's layout, and half an entry is worse than a stale one, because a
//! consumer can mistake it for a hit.
//!
//! What keeps this from deleting something in use, in the order it happens:
//!
//! 1. **Off unless asked, or pressed.** With no limit in force [`trim`] returns before
//!    any Docker call, and an age of `1` is treated as off ([`max_age_days`]). Size
//!    and pressure deletes never touch a volume a running container mounts
//!    ([`volume_in_use`]), nor an entry stamped in the last [`SIZE_GRACE_SECS`].
//! 2. **Only faber's caches.** Volumes labelled `org.faber.kind=cache`, plus the
//!    exact name faber mounts for each repo it serves, for volumes created before
//!    labels existed — exact equality, never a prefix ([`target_volumes`]).
//! 3. **A scan that cannot disturb what it measures.** A throwaway container of the
//!    probe image, no network, the volume mounted **read-only**. The read-only
//!    mount is load-bearing: reading a directory refreshes its atime on a writable
//!    mount (relatime still does it once a day), so a scan through one would make
//!    every directory it measured look freshly used, and the next scan would find
//!    nothing old. The kernel skips atime updates on a read-only mount.
//! 4. **An entry's age is the newest atime, mtime or ctime anywhere in it.** One
//!    file read inside a directory keeps the whole directory ([`expired`]).
//! 5. **A listing that does not parse deletes nothing.** Output cut short is refused
//!    whole, and a line that is not exactly what the scan prints keeps its entry
//!    ([`parse_scan`]).
//! 6. **Re-checked, renamed aside, then deleted.** A second container re-measures
//!    each entry immediately before touching it — through a read-only mount of its
//!    own, for the reason in 3 — and skips anything used since the listing. It then
//!    renames the entry to [`EXPIRED_PREFIX`]`…` and deletes that, so a consumer
//!    looking it up sees a clean miss, never half an entry, and an entry left
//!    renamed by an interrupted pass is finished on the next one. Names reach the
//!    script as arguments, never as script text.
//!
//! Every failure is logged and none is fatal. Not ageing a cache is exactly what
//! ADR-0005 shipped, so doubt always resolves to keeping.
//!
//! ONE RACE REMAINS, and ADR-0024 accepts it: an entry unused for the whole window,
//! looked up in the instant between the re-check and the rename, is a miss.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::anyhow;
use tracing::{debug, info, warn};

use crate::docker::{self, CLI_TIMEOUT, KIND_CACHE, LABEL_KIND};
use crate::untrusted;

/// The smallest `cache_max_age_days` that means what it says.
///
/// The Docker VM mounts its ext4 `relatime`: a read refreshes a file's atime only
/// when the atime is older than its mtime or more than a day old. So an entry a
/// consumer reads every day can carry an atime almost a day stale, and a one-day
/// window would delete it.
pub const MIN_MAX_AGE_DAYS: u64 = 2;

/// What an expired entry is renamed to between the re-check and the delete.
///
/// Anything carrying it is faber's own half-finished delete and always goes, so
/// this prefix is faber's namespace inside a cache volume: a consumer that names
/// its own entries this way loses them on the next pass.
pub const EXPIRED_PREFIX: &str = ".faber-expired-";

const DAY_SECS: u64 = 86_400;

/// Cap on one volume's scan. It walks every file in the volume, so it gets far
/// more than a metadata call — a cache of extracted dependency trees can hold
/// hundreds of thousands of files — but a bound regardless, because a scan that
/// hangs on a wedged daemon must not hang the janitor with it.
const SCAN_TIMEOUT: Duration = Duration::from_secs(300);

/// Cap on one delete container: a re-walk of each doomed entry, the renames, and
/// the deletes, which free real blocks. A delete cut short loses nothing — what it
/// renamed aside is finished by the next pass.
const DELETE_TIMEOUT: Duration = Duration::from_secs(600);

/// Names handed to one delete container, so a volume with thousands of expired
/// entries cannot build an argv longer than `docker` or `execve` will take.
const DELETE_BATCH: usize = 200;

/// How much of a scan or delete container's output faber reads (#105). A scan
/// prints one line per top-level entry of a volume, so it gets more than a
/// listing's [`untrusted::OUTPUT_CAP`]. A scan that writes more is an error like
/// any other failed scan, so a cap reached deletes nothing.
const SCAN_OUTPUT_CAP: usize = 32 * 1024 * 1024;

/// How far the Docker VM's clock may disagree with this host's before faber stops
/// trusting either with an age.
///
/// Ages are measured on the VM's clock, which is the clock that stamped the
/// timestamps being judged. But a VM clock that has jumped ahead makes everything
/// look old at once, and that is the one error here that deletes. An hour is far
/// beyond ordinary drift and far below the two-day minimum window.
const CLOCK_TOLERANCE_SECS: u64 = 3600;

/// The age limit in force, in days, or `None` when ageing is off.
///
/// Absent and `0` are off by definition. A value under [`MIN_MAX_AGE_DAYS`] is
/// refused by `faber check-config` and the settings save; a daemon that boots on
/// one anyway treats it as off and says so ([`boot_warning`]), rather than delete
/// entries still in daily use — or refuse to boot, which would be worse
/// (AGENTS.md).
pub fn max_age_days(configured: Option<u64>) -> Option<u64> {
    configured.filter(|&days| days >= MIN_MAX_AGE_DAYS)
}

/// What to warn at boot about a configured age below the minimum, if anything.
pub fn boot_warning(configured: Option<u64>) -> Option<String> {
    match configured {
        Some(days) if days > 0 && days < MIN_MAX_AGE_DAYS => Some(format!(
            "host.cache_max_age_days = {days} is below the minimum of {MIN_MAX_AGE_DAYS}: the \
             Docker VM mounts its disk relatime, so an entry read every day can look a day \
             old. Treating it as off — no age limit applies. Run \
             `faber check-config` to fix the file (ADR-0024)."
        )),
        _ => None,
    }
}

// --- the decisions (pure) ----------------------------------------------------

/// One top-level entry of a cache volume, as the scan reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Its name inside the volume: one path component, never a path.
    pub name: String,
    /// The newest atime, mtime or ctime anywhere in it (Unix seconds, on the VM's
    /// clock), or `None` when the scan could not measure all of it — which keeps
    /// it.
    pub newest: Option<u64>,
    /// The space it holds (KiB), or `None` when unmeasured. Reported, and it drives
    /// the size and pressure picks ([`size_pick`], [`pressure_pick`]).
    pub kib: Option<u64>,
}

/// One volume's scan, parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scan {
    /// The VM's clock when the scan ran (Unix seconds). Ages are measured against
    /// this rather than the host's clock, because the same clock stamped the
    /// timestamps being judged.
    pub now: u64,
    pub entries: Vec<Entry>,
    /// Lines that did not parse. Their entries are unknown, so they are kept.
    pub rejected: usize,
}

/// Parse the scan container's output ([`SCAN_SCRIPT`]): `now\t<epoch>`, one
/// `<newest>\t<kib>\t<name>` line per top-level entry, then `end`.
///
/// `None` unless it is complete — the `now` line first and the `end` line last —
/// so output cut short is refused whole rather than read as a volume with fewer
/// entries in it. Within it, a line that is not exactly an entry line is counted
/// in `rejected` and its entry dropped: an entry the listing does not report is an
/// entry nothing deletes.
///
/// Split on `\n` alone, never [`str::lines`], which would also take a `\r` off the
/// end of a file name, and never trimmed: a trailing space is part of a name too.
/// The name is everything after the second tab, so a tab inside one survives.
pub fn parse_scan(text: &str) -> Option<Scan> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    // The last line's own newline leaves one empty element behind it.
    if lines.last() == Some(&"") {
        lines.pop();
    }
    let (first, rest) = lines.split_first()?;
    let (last, body) = rest.split_last()?;
    if *last != "end" {
        return None;
    }
    let now = digits(first.strip_prefix("now\t")?)?;
    let mut entries = Vec::with_capacity(body.len());
    let mut rejected = 0;
    for line in body {
        match parse_entry(line) {
            Some(entry) => entries.push(entry),
            None => rejected += 1,
        }
    }
    Some(Scan {
        now,
        entries,
        rejected,
    })
}

/// One `<newest>\t<kib>\t<name>` line. Each number is either all digits or `-`,
/// the scan's own word for "could not measure"; anything else is not a line the
/// scan prints, so it is refused.
fn parse_entry(line: &str) -> Option<Entry> {
    let (newest, rest) = line.split_once('\t')?;
    let (kib, name) = rest.split_once('\t')?;
    let entry = Entry {
        name: name.to_string(),
        newest: measured(newest)?,
        kib: measured(kib)?,
    };
    safe_name(&entry.name).then_some(entry)
}

/// `-` is a clean "unmeasured"; digits are a measurement; anything else is not
/// something the scan prints.
fn measured(field: &str) -> Option<Option<u64>> {
    if field == "-" {
        Some(None)
    } else {
        digits(field).map(Some)
    }
}

/// A non-negative decimal that fits a `u64`, and nothing looser: no sign, no
/// space, no empty string.
fn digits(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Whether `name` can only ever mean one top-level entry of the volume.
///
/// Not empty, not `.` or `..`, and no `/`: each of those would make "this entry"
/// the volume itself, its parent, or something deeper. No NUL or newline, which no
/// line-based listing carries faithfully. And no U+FFFD: the listing is decoded
/// lossily, so a name that was not valid UTF-8 comes back as one that does not
/// exist, and a deletion faber cannot name exactly is one it does not attempt.
pub fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\n', '\0', char::REPLACEMENT_CHARACTER])
}

/// Whether `entry` goes, judged at `now` against a limit of `days`.
///
/// It goes when its newest timestamp is at or before `now − days × 86400`: exactly
/// that old goes, a second younger stays. An entry the scan could not measure
/// stays. An entry already renamed to [`EXPIRED_PREFIX`] always goes, whatever its
/// timestamps say — the rename itself refreshed its ctime, and it is faber's own
/// half-finished delete.
///
/// Free space is not an input, deliberately: disk pressure never shortens the
/// window, so an entry used inside it survives however full the disk is.
pub fn expired(entry: &Entry, now: u64, days: u64) -> bool {
    if !safe_name(&entry.name) {
        return false;
    }
    if entry.name.starts_with(EXPIRED_PREFIX) {
        return true;
    }
    match (entry.newest, cutoff(now, days)) {
        (Some(newest), Some(cutoff)) => newest <= cutoff,
        _ => false,
    }
}

/// The newest timestamp an entry may carry and still go: `now − days × 86400`.
///
/// `None` below [`MIN_MAX_AGE_DAYS`] — [`max_age_days`] never passes such a value,
/// and this is the one function whose misuse deletes, so it refuses one on its own
/// account as well — and `None` when the window would reach back before the epoch.
fn cutoff(now: u64, days: u64) -> Option<u64> {
    if days < MIN_MAX_AGE_DAYS {
        return None;
    }
    now.checked_sub(days.checked_mul(DAY_SECS)?)
}

// --- the size and pressure picks (pure) ----------------------------------------

/// An entry whose timestamps are within this many seconds is never deleted for
/// size or for disk pressure (ADR-0055 §3): defence in depth, not proof of non-use.
/// The VM disk is relatime, so a read refreshes an atime at most once a day; what
/// keeps a running job's cache safe is that a volume any running container mounts
/// is not touched for size or pressure at all ([`volume_in_use`]).
pub const SIZE_GRACE_SECS: u64 = 2 * 3600;

const KIB_PER_GIB: u64 = 1024 * 1024;

/// The limits an operator set, as the pass applies them.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Entries unused this long go ([`max_age_days`] already applied).
    pub max_age_days: Option<u64>,
    /// A volume holding more than this (KiB) loses its least recently used
    /// entries ([`max_kib`] already applied).
    pub max_kib: Option<u64>,
}

impl Limits {
    /// The limits in force for the two configured keys; a key that is absent, `0`
    /// or (for the age) below the minimum imposes none.
    pub fn from_config(age_days: Option<u64>, max_gib: Option<u64>) -> Limits {
        Limits {
            max_age_days: max_age_days(age_days),
            max_kib: max_kib(max_gib),
        }
    }

    /// Whether any limit is in force.
    pub fn any(&self) -> bool {
        self.max_age_days.is_some() || self.max_kib.is_some()
    }
}

/// The size limit in force, in KiB, or `None` for absent or `0`.
pub fn max_kib(configured_gib: Option<u64>) -> Option<u64> {
    configured_gib
        .filter(|&g| g > 0)
        .map(|g| g.saturating_mul(KIB_PER_GIB))
}

/// Whether the operator chose the policy: either key is present, even as `0`
/// (ADR-0055 §2). When neither is, faber decides, under disk pressure only.
pub fn operator_set(age_days: Option<u64>, max_gib: Option<u64>) -> bool {
    age_days.is_some() || max_gib.is_some()
}

/// Entries chosen from one volume, and the cutoff that guards them.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Pick {
    /// Names to delete, least recently used first.
    pub names: Vec<String>,
    /// The newest timestamp among the chosen (0 when none): the delete script
    /// keeps anything newer, so an entry a job touched after the scan survives.
    pub cutoff: u64,
}

/// Whether `e` may be deleted for size or pressure: a safe name, measured, not
/// already faber's half-finished delete, sized, and not used within the grace.
fn lru_eligible(e: &Entry, now: u64) -> Option<u64> {
    if !safe_name(&e.name) || e.name.starts_with(EXPIRED_PREFIX) {
        return None;
    }
    // Deleting an entry nobody could size cannot be shown to help.
    e.kib.filter(|&k| k > 0)?;
    let newest = e.newest?;
    (newest <= now.checked_sub(SIZE_GRACE_SECS)?).then_some(newest)
}

/// The entries to delete so the volume holds no more than `cap_kib`, least
/// recently used first (ADR-0055 §1).
///
/// The volume's total is the sum of `kib` (unsized counts 0). Entries the age rule
/// already takes (`age_days`, and interrupted deletes) count as freed and are not
/// returned. Ties in `newest` go by name, so the pick is deterministic. May return
/// fewer than needed: everything left may be inside the grace.
pub fn size_pick(scan: &Scan, cap_kib: u64, age_days: Option<u64>) -> Pick {
    let total: u64 = scan.entries.iter().filter_map(|e| e.kib).sum();
    let by_age = |e: &Entry| {
        e.name.starts_with(EXPIRED_PREFIX) || age_days.is_some_and(|d| expired(e, scan.now, d))
    };
    let freed: u64 = scan
        .entries
        .iter()
        .filter(|e| by_age(e))
        .filter_map(|e| e.kib)
        .sum();
    let mut remaining = total.saturating_sub(freed);
    let mut pick = Pick::default();
    if remaining <= cap_kib {
        return pick;
    }
    let mut candidates: Vec<(u64, &Entry)> = scan
        .entries
        .iter()
        .filter(|e| !by_age(e))
        .filter_map(|e| lru_eligible(e, scan.now).map(|n| (n, e)))
        .collect();
    candidates.sort_by(|a, b| (a.0, &a.1.name).cmp(&(b.0, &b.1.name)));
    for (newest, e) in candidates {
        if remaining <= cap_kib {
            break;
        }
        remaining = remaining.saturating_sub(e.kib.unwrap_or(0));
        pick.cutoff = pick.cutoff.max(newest);
        pick.names.push(e.name.clone());
    }
    pick
}

/// The entries to delete across all `volumes` to free `needed_kib`, the globally
/// least recently used first (ADR-0055 §2). Stops as soon as the chosen sizes sum
/// to at least `needed_kib`. One result per volume that lost something, in the
/// order the volumes were given; each volume's grace is judged on its own scan's
/// clock.
pub fn pressure_pick(volumes: &[(String, Scan)], needed_kib: u64) -> Vec<(String, Pick)> {
    if needed_kib == 0 {
        return Vec::new();
    }
    let mut candidates: Vec<(u64, usize, &Entry)> = Vec::new();
    for (i, (_, scan)) in volumes.iter().enumerate() {
        for e in &scan.entries {
            if let Some(newest) = lru_eligible(e, scan.now) {
                candidates.push((newest, i, e));
            }
        }
    }
    candidates.sort_by(|a, b| (a.0, a.1, &a.2.name).cmp(&(b.0, b.1, &b.2.name)));
    let mut picks: Vec<Pick> = vec![Pick::default(); volumes.len()];
    let mut freed = 0u64;
    for (newest, i, e) in candidates {
        if freed >= needed_kib {
            break;
        }
        freed = freed.saturating_add(e.kib.unwrap_or(0));
        picks[i].cutoff = picks[i].cutoff.max(newest);
        picks[i].names.push(e.name.clone());
    }
    volumes
        .iter()
        .zip(picks)
        .filter(|(_, p)| !p.names.is_empty())
        .map(|((v, _), p)| (v.clone(), p))
        .collect()
}

/// Whether the VM's clock (`vm_now`) and this host's agree closely enough to judge
/// an age by. See [`CLOCK_TOLERANCE_SECS`].
fn clocks_agree(vm_now: u64, host_now: u64) -> bool {
    vm_now.abs_diff(host_now) <= CLOCK_TOLERANCE_SECS
}

/// One row of [`volume_ls_args`]'s listing: a volume's name and its
/// `org.faber.kind` label, empty when it has none.
#[derive(Debug, serde::Deserialize)]
struct ListedVolume {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Kind", default)]
    kind: String,
}

/// The cache volumes to age, from one [`volume_ls_args`] listing — one JSON
/// `{"Name":…,"Kind":…}` object per line — and the repos faber serves.
///
/// A volume labelled `org.faber.kind=cache` is one faber created for this and says
/// so, whatever it is called. An **unlabelled** volume qualifies only when its name
/// is exactly the one faber mounts for a repo it serves
/// ([`docker::cache_volume_name`]): cache volumes created before ADR-0007's labels
/// carry none, and the volume that prompted ADR-0024 is one of them. Exact
/// equality, never a prefix — AGENTS.md reads labels, not names, and this is the
/// one exception ADR-0024 sanctions, so it is held to the narrowest form there
/// is. A volume carrying some *other* faber kind is not a cache, whatever it is
/// named.
///
/// A name outside Docker's own volume alphabet is skipped with a warning (#105).
/// The name goes into a `-v name:/c` mount spec, where a `/` would mount a host
/// path in its place and a `:` would rewrite the mount.
pub fn target_volumes(listing: &str, served: &[String]) -> Vec<String> {
    let legacy: HashSet<String> = served
        .iter()
        .map(|repo| docker::cache_volume_name(repo))
        .collect();
    let rows = untrusted::json_lines::<ListedVolume>(listing, "cache: volume listing");
    let mut out: Vec<String> = Vec::new();
    for name in untrusted::accept(rows, "cache: volume listing", |row| {
        let cache = if row.kind.is_empty() {
            legacy.contains(&row.name)
        } else {
            row.kind == KIND_CACHE
        };
        if !cache {
            Ok(None)
        } else if untrusted::object_name(&row.name) {
            Ok(Some(row.name))
        } else {
            Err("a volume name is outside Docker's own alphabet")
        }
    }) {
        if !out.contains(&name) {
            out.push(name);
        }
    }
    out
}

/// What one delete container reported, entry by entry ([`DELETE_SCRIPT`]).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Deleted {
    /// Re-checked, still expired, and renamed aside.
    moved: Vec<String>,
    /// Used since the listing, or unmeasurable on the re-check: kept.
    kept: Vec<String>,
    /// No longer there when the re-check looked.
    gone: Vec<String>,
    /// Expired, but the rename failed: kept.
    failed: Vec<String>,
    /// Deleted from the renamed-aside area: this pass's renames, and any an
    /// interrupted pass left behind.
    removed: Vec<String>,
}

impl Deleted {
    fn absorb(&mut self, other: Deleted) {
        self.moved.extend(other.moved);
        self.kept.extend(other.kept);
        self.gone.extend(other.gone);
        self.failed.extend(other.failed);
        self.removed.extend(other.removed);
    }
}

/// Parse a delete container's `<what>\t<name>` report. Only ever used to say what
/// happened — the decisions were the container's, on its own re-check — so an
/// unrecognised line is skipped rather than refused.
fn parse_delete(text: &str) -> Deleted {
    let mut out = Deleted::default();
    for line in text.split('\n') {
        let Some((what, name)) = line.split_once('\t') else {
            continue;
        };
        let bucket = match what {
            "moved" => &mut out.moved,
            "kept" => &mut out.kept,
            "gone" => &mut out.gone,
            "failed" => &mut out.failed,
            "removed" => &mut out.removed,
            _ => continue,
        };
        bucket.push(name.to_string());
    }
    out
}

// --- the scripts ---------------------------------------------------------------

/// Shell both scripts share. `measure PATH` prints `<newest>\t<kib>` over
/// everything under PATH, symlinks never followed, or `-\t-` when any of it could
/// not be measured — an unreadable directory, a file that vanished mid-walk, a
/// missing tool. `find` fails for all of those, and the `echo fail` it then adds
/// is a line the checks reject, so a partial walk can never pass for a complete
/// one with an older answer. Size is `st_blocks` (512-byte units), which is what
/// `du` counts.
macro_rules! measure_sh {
    () => {
        r#"tab=$(printf '\t')
nl='
'
measure() {
  { find "$1" -exec stat -c '%X %Y %Z %b' {} + 2>/dev/null || echo fail; } | awk '
    BEGIN { m = 0; k = 0; bad = 0 }
    NF != 4 || $1 !~ /^[0-9]+$/ || $2 !~ /^[0-9]+$/ || $3 !~ /^[0-9]+$/ || $4 !~ /^[0-9]+$/ { bad = 1; next }
    {
      for (i = 1; i <= 3; i++) if ($i + 0 > m) m = $i + 0
      k += $4
    }
    END {
      if (bad || NR == 0) printf "-\t-\n"
      else printf "%.0f\t%.0f\n", m, int((k + 1) / 2)
    }'
}
"#
    };
}

/// The scan, run with the volume read-only at `/c`. Prints `now\t<epoch>`, one
/// `<newest>\t<kib>\t<name>` line per top-level entry, dotfiles included, then
/// `end`. A name holding a newline cannot be reported on one line, so it is never
/// reported — and so never deleted.
const SCAN_SCRIPT: &str = concat!(
    measure_sh!(),
    r#"now=$(date +%s) || exit 1
printf 'now\t%s\n' "$now"
for e in /c/* /c/.[!.]* /c/..?*; do
  [ -e "$e" ] || [ -L "$e" ] || continue
  name=${e#/c/}
  case "$name" in *"$nl"*) continue ;; esac
  printf '%s\t%s\n' "$(measure "$e")" "$name"
done
echo end
"#
);

/// The delete, run with the volume read-write at `/c` and read-only at `/r`.
/// `$1` is the cutoff and every later argument an entry name. For each name it
/// re-checks through `/r` — a walk through `/c` would itself refresh the atimes it
/// is reading — keeps anything newer than the cutoff or unmeasurable, renames the
/// rest to `.faber-expired-<epoch>-<n>`, and finally deletes everything carrying
/// that prefix, including any an interrupted pass left. Reports one
/// `<what>\t<name>` line per entry, then `end`.
const DELETE_SCRIPT: &str = concat!(
    measure_sh!(),
    r#"cutoff=$1
shift
case "$cutoff" in ''|*[!0-9]*) exit 2 ;; esac
ts=$(date +%s) || exit 1
n=0
for name in "$@"; do
  case "$name" in
    ''|.|..|*/*|*"$nl"*|.faber-expired-*) continue ;;
  esac
  if [ ! -e "/r/$name" ] && [ ! -L "/r/$name" ]; then
    printf 'gone\t%s\n' "$name"
    continue
  fi
  t=$(measure "/r/$name")
  t=${t%%"$tab"*}
  case "$t" in ''|*[!0-9]*) printf 'kept\t%s\n' "$name"; continue ;; esac
  if [ "$t" -gt "$cutoff" ]; then
    printf 'kept\t%s\n' "$name"
    continue
  fi
  n=$((n + 1))
  dst="/c/.faber-expired-$ts-$n"
  while [ -e "$dst" ] || [ -L "$dst" ]; do
    n=$((n + 1))
    dst="/c/.faber-expired-$ts-$n"
  done
  if mv -- "/c/$name" "$dst"; then
    printf 'moved\t%s\n' "$name"
  else
    printf 'failed\t%s\n' "$name"
  fi
done
for x in /c/.faber-expired-*; do
  [ -e "$x" ] || [ -L "$x" ] || continue
  rm -rf -- "$x" && printf 'removed\t%s\n' "${x#/c/}"
done
echo end
"#
);

// --- argv ------------------------------------------------------------------------

/// One `docker volume ls` of every volume, as one JSON object per volume
/// carrying its name and its `org.faber.kind` label.
///
/// Unfiltered, deliberately: a label filter cannot return the unlabelled volumes
/// that predate ADR-0007, and Docker's `name` filter is a substring match.
/// [`target_volumes`] applies the label rule and the exact-name rule itself.
fn volume_ls_args() -> Vec<String> {
    vec![
        "volume".into(),
        "ls".into(),
        "--format".into(),
        format!(r#"{{"Name":{{{{json .Name}}}},"Kind":{{{{json (.Label "{LABEL_KIND}")}}}}}}"#),
    ]
}

/// The scan container: the volume **read-only** at `/c` (see the module doc for
/// why that is load-bearing), no network, removed on exit, and named so a scan
/// that overruns [`SCAN_TIMEOUT`] can be killed rather than left walking.
fn scan_args(volume: &str, image: &str, container: &str) -> Vec<String> {
    vec![
        "run".into(),
        "--rm".into(),
        "--name".into(),
        container.into(),
        "--network".into(),
        "none".into(),
        "-v".into(),
        format!("{volume}:/c:ro"),
        image.into(),
        "sh".into(),
        "-c".into(),
        SCAN_SCRIPT.into(),
    ]
}

/// The delete container: the volume read-write at `/c` for the renames and
/// deletes, and read-only at `/r` for the re-check. The names follow the script as
/// positional arguments (`$0` is `sh`, `$1` the cutoff), so a name is only ever
/// data to the shell — never text it parses.
fn delete_args(
    volume: &str,
    image: &str,
    container: &str,
    cutoff: u64,
    names: &[&str],
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "run".into(),
        "--rm".into(),
        "--name".into(),
        container.into(),
        "--network".into(),
        "none".into(),
        "-v".into(),
        format!("{volume}:/c"),
        "-v".into(),
        format!("{volume}:/r:ro"),
        image.into(),
        "sh".into(),
        "-c".into(),
        DELETE_SCRIPT.into(),
        "sh".into(),
        cutoff.to_string(),
    ];
    args.extend(names.iter().map(|n| n.to_string()));
    args
}

/// A name for one throwaway container, unique within this process and across
/// restarts. Not `faber-cache-…`, which is the volumes' namespace.
fn container_name(what: &str) -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!(
        "faber-cacheage-{what}-{}-{}",
        crate::audit::now_ms(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

// --- the pass (I/O) ------------------------------------------------------------

/// Why an entry was removed — what the log line and the summary say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Unused for `cache_max_age_days`.
    Expired,
    /// Least recently used, while its volume held more than `cache_max_gib`.
    OverSize,
    /// Least recently used, while the disk was under its floor and no limit was set.
    Pressure,
}

/// What one pass removed, across every cache volume.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Aged {
    /// Entries removed: chosen ones, and interrupted deletes finished.
    pub entries: usize,
    /// The space they held (KiB), as the scan measured it. A floor: an entry the
    /// scan could not size counts nothing.
    pub kib: u64,
    /// Cache volumes something was removed from.
    pub volumes: usize,
    /// Of `entries`, removed for age.
    pub expired: usize,
    /// Of `entries`, removed for the size limit.
    pub over_size: usize,
    /// Of `entries`, removed to relieve disk pressure.
    pub pressure: usize,
}

impl Aged {
    pub fn is_empty(&self) -> bool {
        self.entries == 0
    }

    fn absorb(&mut self, other: Aged) {
        self.entries += other.entries;
        self.kib += other.kib;
        self.volumes += other.volumes;
        self.expired += other.expired;
        self.over_size += other.over_size;
        self.pressure += other.pressure;
    }

    /// The janitor's reclamation-log fragment for this pass.
    pub fn summary(&self) -> String {
        let mut why: Vec<String> = Vec::new();
        if self.expired > 0 {
            why.push(format!("{} expired", self.expired));
        }
        if self.over_size > 0 {
            why.push(format!("{} over the size limit", self.over_size));
        }
        if self.pressure > 0 {
            why.push(format!("{} freed for disk pressure", self.pressure));
        }
        let why = if why.is_empty() {
            String::new()
        } else {
            format!(": {}", why.join(", "))
        };
        format!(
            "cache entries removed: {} ({} MiB in {} volume(s){why})",
            self.entries,
            self.kib / 1024,
            self.volumes
        )
    }
}

/// Every cache volume faber may trim, from one listing. A failure is logged and
/// yields none.
async fn list_targets(served: &[String]) -> Vec<String> {
    match docker::docker_stdout(&volume_ls_args(), CLI_TIMEOUT, untrusted::OUTPUT_CAP).await {
        Ok(text) => target_volumes(&text, served),
        Err(e) => {
            warn!("cache: listing volumes: {e} — trimming nothing this pass");
            Vec::new()
        }
    }
}

/// Apply the operator's limits to every cache volume (ADR-0024 §4, ADR-0055 §1).
///
/// `served` is the repos faber serves, for the pre-label volumes (see
/// [`target_volumes`]); empty narrows the pass to labelled volumes. `probe_image`
/// runs the throwaway containers. Every failure is logged here and costs only the
/// volume it happened in. With no limit set it does nothing, before any Docker call.
pub async fn trim(limits: Limits, probe_image: &str, served: &[String]) -> Aged {
    let plans = trim_plan(limits, probe_image, served).await;
    trim_execute(plans, limits, probe_image).await
}

/// One scanned volume and what [`plan`] would delete from it.
#[derive(Debug)]
pub struct VolumePlan {
    volume: String,
    scan: Scan,
    plan: Vec<(String, Reason)>,
    cutoff: u64,
}

/// Whether executing `plans` includes a size-driven delete, which needs admission
/// paused so no job mounts the volume after the mount check. Age-only plans do not.
pub fn needs_pause(plans: &[VolumePlan]) -> bool {
    plans
        .iter()
        .any(|p| p.plan.iter().any(|(_, r)| *r == Reason::OverSize))
}

/// Phase one of [`trim`]: scan every target volume and plan its deletes. Read-only
/// and slow on a big volume, so it runs without any admission pause. Volumes with
/// nothing to delete are left out.
pub async fn trim_plan(limits: Limits, probe_image: &str, served: &[String]) -> Vec<VolumePlan> {
    let mut plans = Vec::new();
    if !limits.any() {
        return plans;
    }
    for volume in list_targets(served).await {
        match scan_volume(&volume, probe_image).await {
            Ok(scan) => {
                let (plan, cutoff) = plan(&scan, limits, true);
                if plan.is_empty() {
                    debug!(
                        "cache: {volume}: nothing breaks a limit among {} entries",
                        scan.entries.len()
                    );
                } else {
                    plans.push(VolumePlan {
                        volume,
                        scan,
                        plan,
                        cutoff,
                    });
                }
            }
            Err(e) => warn!("cache: {volume}: {e}"),
        }
    }
    plans
}

/// Phase two of [`trim`]: delete what `plans` chose. Per volume, the mount check
/// runs only when the plan holds a size-driven entry; a mounted volume is re-planned
/// without them. The delete script re-checks every entry against the cutoff, so
/// anything touched since the scan stays.
pub async fn trim_execute(plans: Vec<VolumePlan>, limits: Limits, probe_image: &str) -> Aged {
    let mut total = Aged::default();
    for VolumePlan {
        volume,
        scan,
        mut plan,
        mut cutoff,
    } in plans
    {
        if plan.iter().any(|(_, r)| *r == Reason::OverSize) && volume_in_use(&volume).await {
            (plan, cutoff) = self::plan(&scan, limits, false);
        }
        if plan.is_empty() {
            continue;
        }
        match delete_volume(&volume, probe_image, &scan, &plan, cutoff, limits).await {
            Ok(done) => total.absorb(done),
            Err(e) => warn!("cache: {volume}: {e}"),
        }
    }
    total
}

/// Free at least `needed_kib` from the cache volumes, least recently used first
/// across all of them (ADR-0055 §2). For a host whose operator set no limit, from
/// the janitor's pressure pass. Stops at what the scans' sizes say reaches the
/// amount; an entry used in the last [`SIZE_GRACE_SECS`] is never chosen.
pub async fn relieve(needed_kib: u64, probe_image: &str, served: &[String]) -> Aged {
    let mut total = Aged::default();
    if needed_kib == 0 {
        return total;
    }
    let mut scans: Vec<(String, Scan)> = Vec::new();
    for volume in list_targets(served).await {
        match scan_volume(&volume, probe_image).await {
            Ok(scan) => {
                if volume_in_use(&volume).await {
                    continue;
                }
                scans.push((volume, scan));
            }
            Err(e) => warn!("cache: {volume}: {e}"),
        }
    }
    for (volume, pick) in pressure_pick(&scans, needed_kib) {
        let Some((_, scan)) = scans.iter().find(|(v, _)| *v == volume) else {
            continue;
        };
        // Again, immediately before this volume's delete: the scans above took time.
        if volume_in_use(&volume).await {
            continue;
        }
        let plan: Vec<(String, Reason)> = pick
            .names
            .into_iter()
            .map(|n| (n, Reason::Pressure))
            .collect();
        match delete_volume(
            &volume,
            probe_image,
            scan,
            &plan,
            pick.cutoff,
            Limits::default(),
        )
        .await
        {
            Ok(done) => total.absorb(done),
            Err(e) => warn!("cache: {volume}: {e}"),
        }
    }
    total
}

/// Scan one volume. Nothing is deleted on any error.
async fn scan_volume(volume: &str, probe_image: &str) -> anyhow::Result<Scan> {
    // [`target_volumes`] already held the name to Docker's own alphabet; this is
    // the line it must never cross, since the name goes into a mount spec (#105).
    anyhow::ensure!(
        untrusted::object_name(volume),
        "not a volume name Docker allows, nothing scanned"
    );
    let scanner = container_name("scan");
    let text = run_throwaway(
        &scanner,
        &scan_args(volume, probe_image, &scanner),
        SCAN_TIMEOUT,
    )
    .await
    .map_err(|e| anyhow!("scan failed, nothing deleted: {e}"))?;
    let scan = parse_scan(&text)
        .ok_or_else(|| anyhow!("the scan's output was incomplete or malformed, nothing deleted"))?;
    if scan.rejected > 0 {
        warn!(
            "cache: {volume}: {} listing line(s) did not parse — those entries are kept",
            scan.rejected
        );
    }
    let host_now = crate::audit::now_ms() / 1000;
    if !clocks_agree(scan.now, host_now) {
        anyhow::bail!(
            "the Docker VM's clock reads {} and this host's {host_now} — too far apart to judge \
             an age by, nothing deleted",
            scan.now
        );
    }
    Ok(scan)
}

/// What to delete from one scanned volume under `limits`, and the one cutoff that
/// guards the batch: the age rule's entries (and interrupted deletes) as
/// [`Reason::Expired`], then — only when `size_allowed` — the size rule's as
/// [`Reason::OverSize`]. The cutoff is the larger of the two rules' cutoffs; with
/// `size_allowed` false it is the age cutoff alone.
fn plan(scan: &Scan, limits: Limits, size_allowed: bool) -> (Vec<(String, Reason)>, u64) {
    let mut plan: Vec<(String, Reason)> = Vec::new();
    // An interrupted delete always goes, limit or no limit; the age rule says so.
    for e in &scan.entries {
        if e.name.starts_with(EXPIRED_PREFIX)
            || limits.max_age_days.is_some_and(|d| expired(e, scan.now, d))
        {
            plan.push((e.name.clone(), Reason::Expired));
        }
    }
    let mut cutoff_at = limits
        .max_age_days
        .and_then(|d| cutoff(scan.now, d))
        .unwrap_or(0);
    if let (true, Some(cap)) = (size_allowed, limits.max_kib) {
        let pick = size_pick(scan, cap, limits.max_age_days);
        cutoff_at = cutoff_at.max(pick.cutoff);
        plan.extend(pick.names.into_iter().map(|n| (n, Reason::OverSize)));
    }
    (plan, cutoff_at)
}

/// Whether any RUNNING container mounts `volume`. Fails closed: an error or a
/// timeout reads as "mounted", because a size- or pressure-driven delete must
/// never run against a volume a job may be reading (ADR-0055 §3).
///
/// The grace on an entry's timestamps cannot stand in for this: the VM mounts its
/// disk relatime, so a read refreshes an atime at most once a day, and an entry a
/// running job is reading can look hours idle.
async fn volume_in_use(volume: &str) -> bool {
    let args: Vec<String> = vec![
        "ps".into(),
        "-q".into(),
        "--filter".into(),
        format!("volume={volume}"),
    ];
    match docker::docker_stdout(&args, CLI_TIMEOUT, untrusted::OUTPUT_CAP).await {
        Ok(out) if out.trim().is_empty() => false,
        Ok(_) => {
            info!("cache: {volume}: a running container mounts it — no size or pressure delete this pass");
            true
        }
        Err(e) => {
            warn!("cache: {volume}: could not tell whether a container mounts it ({e}) — no size or pressure delete this pass");
            true
        }
    }
}

/// Delete `plan` from one volume in batches, all behind one `cutoff` (the script
/// keeps anything newer), and report what happened.
async fn delete_volume(
    volume: &str,
    probe_image: &str,
    scan: &Scan,
    plan: &[(String, Reason)],
    cutoff: u64,
    limits: Limits,
) -> anyhow::Result<Aged> {
    let mut done = Deleted::default();
    for batch in plan.chunks(DELETE_BATCH) {
        let names: Vec<&str> = batch.iter().map(|(n, _)| n.as_str()).collect();
        let deleter = container_name("rm");
        let args = delete_args(volume, probe_image, &deleter, cutoff, &names);
        match run_throwaway(&deleter, &args, DELETE_TIMEOUT).await {
            Ok(text) => done.absorb(parse_delete(&text)),
            Err(e) => {
                warn!(
                    "cache: {volume}: the delete did not finish ({e}) — anything it renamed \
                     aside is removed on the next pass"
                );
                break;
            }
        }
    }
    let reasons: HashMap<&str, Reason> = plan.iter().map(|(n, r)| (n.as_str(), *r)).collect();
    Ok(report(volume, limits, scan, &reasons, &done))
}

/// Log what one volume's delete did, and tally it from the scan's measurements.
///
/// This pass's renames are counted through `moved`, under the names the scan
/// measured them by; `removed` then adds only the renamed-aside entries the scan
/// itself listed, i.e. the ones an interrupted pass left, so nothing is counted
/// twice.
fn report(
    volume: &str,
    limits: Limits,
    scan: &Scan,
    reasons: &HashMap<&str, Reason>,
    done: &Deleted,
) -> Aged {
    let by_name: HashMap<&str, &Entry> =
        scan.entries.iter().map(|e| (e.name.as_str(), e)).collect();
    let moved: Vec<(&Entry, Reason)> = done
        .moved
        .iter()
        .filter_map(|n| {
            let entry = by_name.get(n.as_str()).copied()?;
            Some((entry, reasons.get(n.as_str()).copied()?))
        })
        .collect();
    let leftovers: Vec<&Entry> = done
        .removed
        .iter()
        .filter_map(|n| by_name.get(n.as_str()).copied())
        .collect();
    let leftover_kib: u64 = leftovers.iter().filter_map(|e| e.kib).sum();
    let of = |why: Reason| -> Vec<&Entry> {
        moved
            .iter()
            .filter(|(_, r)| *r == why)
            .map(|(e, _)| *e)
            .collect()
    };
    let plural = |n: usize| if n == 1 { "y" } else { "ies" };

    let expired = of(Reason::Expired);
    if !expired.is_empty() {
        let idle_days = expired
            .iter()
            .filter_map(|e| e.newest)
            .map(|t| scan.now.saturating_sub(t) / DAY_SECS)
            .min();
        let limit = limits
            .max_age_days
            .map_or(String::new(), |d| format!(", the limit is {d}"));
        info!(
            "cache: {volume}: expired {} entr{} ({} MiB) — the most recently used had been \
             idle {} days{limit} (ADR-0024)",
            expired.len(),
            plural(expired.len()),
            expired.iter().filter_map(|e| e.kib).sum::<u64>() / 1024,
            idle_days.unwrap_or(0)
        );
    }
    let over = of(Reason::OverSize);
    if !over.is_empty() {
        let cap = limits.max_kib.map_or(String::new(), |k| {
            format!(", the limit is {} GiB", k / (1024 * 1024))
        });
        info!(
            "cache: {volume}: removed {} least recently used entr{} ({} MiB) — the volume was \
             over its size limit{cap} (ADR-0055)",
            over.len(),
            plural(over.len()),
            over.iter().filter_map(|e| e.kib).sum::<u64>() / 1024
        );
    }
    let pressure = of(Reason::Pressure);
    if !pressure.is_empty() {
        info!(
            "cache: {volume}: freed {} least recently used entr{} ({} MiB) for disk pressure \
             (ADR-0055)",
            pressure.len(),
            plural(pressure.len()),
            pressure.iter().filter_map(|e| e.kib).sum::<u64>() / 1024
        );
    }
    debug!("cache: {volume}: removed {:?}", done.moved);
    if !leftovers.is_empty() {
        info!(
            "cache: {volume}: finished {} delete(s) an earlier pass was interrupted in \
             ({} MiB)",
            leftovers.len(),
            leftover_kib / 1024
        );
    }
    if !done.kept.is_empty() {
        info!(
            "cache: {volume}: kept {} entr{} the re-check found in use since the listing, or \
             could not measure",
            done.kept.len(),
            plural(done.kept.len())
        );
    }
    if !done.failed.is_empty() {
        warn!(
            "cache: {volume}: could not rename {} entr{} aside, so kept them: {:?}",
            done.failed.len(),
            plural(done.failed.len()),
            done.failed
        );
    }
    let entries = moved.len() + leftovers.len();
    Aged {
        entries,
        kib: moved.iter().filter_map(|(e, _)| e.kib).sum::<u64>() + leftover_kib,
        volumes: usize::from(entries > 0),
        expired: expired.len(),
        over_size: over.len(),
        pressure: pressure.len(),
    }
}

/// Run one throwaway container and return its stdout, killing it if `docker run`
/// fails or overruns `limit`.
///
/// The client giving up does not stop the container — `docker run` has no hold
/// on it once it is killed — so without the `rm -f` a scan that overran would go
/// on walking the volume, and a later pass could start a second one beside it.
/// After an ordinary failure the container is already gone (`--rm`), and the
/// removal is a harmless no-op.
async fn run_throwaway(
    container: &str,
    args: &[String],
    limit: Duration,
) -> anyhow::Result<String> {
    match docker::docker_stdout(args, limit, SCAN_OUTPUT_CAP).await {
        Ok(out) => Ok(out),
        Err(e) => {
            if let Err(rm) = docker::docker_rm(&["rm", "-f", container]).await {
                warn!("cache: removing container {container}: {rm}");
            }
            Err(e)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, newest: Option<u64>) -> Entry {
        Entry {
            name: name.to_string(),
            newest,
            kib: Some(4),
        }
    }

    fn lines(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    /// A Unix time on the order of today's, so arithmetic near it is realistic.
    const NOW: u64 = 1_790_000_000;
    const DAYS: u64 = 14;

    // --- the key: off, on, and the one value that is neither -----------------

    #[test]
    fn ageing_is_off_unless_asked_for_at_least_two_days() {
        assert_eq!(max_age_days(None), None, "absent is ADR-0005 as written");
        assert_eq!(max_age_days(Some(0)), None, "0 is off, stated");
        // relatime refreshes atime at most once a day, so a daily read can look a
        // day old: 1 would delete entries still in daily use.
        assert_eq!(max_age_days(Some(1)), None);
        assert_eq!(max_age_days(Some(2)), Some(2));
        assert_eq!(max_age_days(Some(14)), Some(14));
    }

    #[test]
    fn only_a_one_day_limit_is_warned_about_at_boot() {
        assert!(boot_warning(Some(1)).is_some_and(|w| w.contains("Treating it as off")));
        for quiet in [None, Some(0), Some(2), Some(30)] {
            assert_eq!(boot_warning(quiet), None, "{quiet:?}");
        }
    }

    fn with_host(host: &str) -> crate::config::FileConfig {
        toml::from_str(&format!(
            r#"
            [host]
            {host}

            [[profile]]
            name = "linux-docker"
            labels = ["faber", "docker"]
            backend = "docker"
            image = "ghcr.io/owner/my-runner:latest"
            # Stated, not this test's concern: an unstated grant trips the Linux
            # dind wall in `validate()` (ADR-0025) on the ubuntu CI leg only.
            dind = false
        "#
        ))
        .unwrap()
    }

    /// `faber check-config` and the settings save both run `validate`, so this is
    /// where a human is told — the boot, which nobody watches, only warns.
    #[test]
    fn config_check_rejects_one_day_and_accepts_off_and_real_windows() {
        let errs = with_host("cache_max_age_days = 1").validate();
        assert!(
            errs.iter()
                .any(|e| e.starts_with("host.cache_max_age_days")),
            "{errs:?}"
        );
        for fine in ["cache_max_age_days = 0", "cache_max_age_days = 2", ""] {
            let errs = with_host(fine).validate();
            assert!(errs.is_empty(), "{fine:?}: {errs:?}");
        }
        assert_eq!(with_host("").host.cache_max_age_days, None, "absent parses");
    }

    #[test]
    fn the_settings_save_refuses_one_day_before_writing_anything() {
        let path = std::env::temp_dir().join(format!(
            "faber-cache-age-refused-{}.toml",
            std::process::id()
        ));
        let store = crate::settings::SettingsStore::new(
            path.clone(),
            crate::config::FileConfig::default(),
            false,
        );
        let err = store
            .save(&with_host("cache_max_age_days = 1"))
            .expect_err("a one-day window must not be saved");
        assert!(err.to_string().contains("cache_max_age_days"), "{err}");
        assert!(!path.exists(), "a refused save writes nothing");
    }

    #[test]
    fn an_absent_key_is_not_written_back() {
        // A host that never asked must stay a host that never asked, including
        // across a save from the settings page.
        let text = with_host("").to_toml().unwrap();
        assert!(!text.contains("cache_max_age_days"), "{text}");
        let text = with_host("cache_max_age_days = 21").to_toml().unwrap();
        assert!(text.contains("cache_max_age_days = 21"), "{text}");
    }

    // --- the expiry rule ----------------------------------------------------

    #[test]
    fn an_entry_exactly_the_limit_old_goes_and_a_second_younger_stays() {
        let limit = NOW - DAYS * DAY_SECS;
        assert!(expired(&entry("old.tar", Some(limit)), NOW, DAYS));
        assert!(!expired(&entry("warm.tar", Some(limit + 1)), NOW, DAYS));
        assert!(expired(&entry("ancient.tar", Some(0)), NOW, DAYS));
        assert!(!expired(&entry("future.tar", Some(NOW + 60)), NOW, DAYS));
    }

    #[test]
    fn an_entry_the_scan_could_not_measure_is_kept() {
        assert!(!expired(&entry("half-read", None), NOW, DAYS));
    }

    #[test]
    fn an_interrupted_delete_always_goes() {
        // The rename refreshed its ctime, so its timestamps say "new"; it is
        // faber's own half-finished work, and nothing else may carry the prefix.
        for newest in [Some(NOW), None, Some(0)] {
            assert!(expired(
                &entry(".faber-expired-1790000000-1", newest),
                NOW,
                DAYS
            ));
        }
        // ...but only under the exact prefix.
        assert!(!expired(&entry(".faber-expire", Some(NOW)), NOW, DAYS));
        assert!(!expired(&entry("x.faber-expired-1", Some(NOW)), NOW, DAYS));
    }

    #[test]
    fn a_name_that_is_not_one_entry_never_goes() {
        for name in [
            "",
            ".",
            "..",
            "a/b",
            "/",
            "../up",
            "a\nb",
            "bad\u{FFFD}utf8",
        ] {
            assert!(!safe_name(name), "{name:?}");
            assert!(!expired(&entry(name, Some(0)), NOW, DAYS), "{name:?}");
        }
        // Odd but real names are one entry each, and judged like any other.
        for name in [
            " lead",
            "trail ",
            "tab\there",
            "-rf",
            "$(touch pwned); rm -rf .",
            "cr\r",
            "..x",
            "ünï",
        ] {
            assert!(safe_name(name), "{name:?}");
            assert!(expired(&entry(name, Some(0)), NOW, DAYS), "{name:?}");
        }
    }

    #[test]
    fn a_limit_below_the_minimum_expires_nothing_even_if_passed_directly() {
        // `max_age_days` never hands these on; this is the function that deletes,
        // so it refuses them on its own account too.
        for days in [0, 1] {
            assert!(!expired(&entry("old", Some(0)), NOW, days), "{days}");
        }
        // A window reaching back before the epoch cannot be right either.
        assert!(!expired(&entry("old", Some(0)), 10, DAYS));
        assert!(!expired(&entry("old", Some(0)), NOW, u64::MAX));
    }

    #[test]
    fn a_clock_far_from_this_hosts_is_not_trusted() {
        assert!(clocks_agree(NOW, NOW));
        assert!(clocks_agree(NOW + 3600, NOW));
        assert!(clocks_agree(NOW - 3600, NOW));
        // A VM clock that jumped ahead would make everything look old at once.
        assert!(!clocks_agree(NOW + 30 * DAY_SECS, NOW));
        assert!(!clocks_agree(0, NOW));
    }

    // --- reading the scan ---------------------------------------------------

    #[test]
    fn a_complete_scan_parses_every_entry_exactly() {
        let text = "now\t1790000000\n\
                    1788000000\t2048\tnode_modules-abc.tar.zst\n\
                    1789999000\t-\t.hidden dir\n\
                    -\t12\tunmeasured\n\
                    1700000000\t4\t .faber-expired-1-1\n\
                    end\n";
        let scan = parse_scan(text).expect("complete");
        assert_eq!(scan.now, 1_790_000_000);
        assert_eq!(scan.rejected, 0);
        assert_eq!(
            scan.entries,
            vec![
                Entry {
                    name: "node_modules-abc.tar.zst".into(),
                    newest: Some(1_788_000_000),
                    kib: Some(2048)
                },
                Entry {
                    name: ".hidden dir".into(),
                    newest: Some(1_789_999_000),
                    kib: None
                },
                Entry {
                    name: "unmeasured".into(),
                    newest: None,
                    kib: Some(12)
                },
                // A leading space is part of the name, so this is NOT a
                // renamed-aside entry and is judged by its age like any other.
                Entry {
                    name: " .faber-expired-1-1".into(),
                    newest: Some(1_700_000_000),
                    kib: Some(4)
                },
            ]
        );
    }

    #[test]
    fn names_survive_the_listing_byte_for_byte() {
        // No trimming and no `str::lines`: a trailing space, a trailing `\r` and an
        // embedded tab are all part of the name the delete will be handed.
        let text = "now\t5\n1\t1\ttrail \n1\t1\tcr\r\n1\t1\ttab\there\n1\t1\t-rf\nend\n";
        let names: Vec<String> = parse_scan(text)
            .unwrap()
            .entries
            .into_iter()
            .map(|e| e.name)
            .collect();
        assert_eq!(names, vec!["trail ", "cr\r", "tab\there", "-rf"]);
    }

    #[test]
    fn a_scan_cut_short_is_refused_whole() {
        // No trailer: the container died, or the stream was cut. Reading it as a
        // volume with fewer entries would be a guess.
        assert_eq!(parse_scan("now\t5\n1\t1\ta\n"), None);
        assert_eq!(parse_scan("now\t5\n1\t1\ta\nen"), None);
        // No header, or a header that is not a clock reading.
        assert_eq!(parse_scan("1\t1\ta\nend\n"), None);
        assert_eq!(parse_scan("now\t\nend\n"), None);
        assert_eq!(parse_scan("now\t-5\nend\n"), None);
        assert_eq!(parse_scan("now\t12x\nend\n"), None);
        assert_eq!(parse_scan(""), None);
        assert_eq!(parse_scan("end\n"), None);
        // An empty volume is a complete scan with nothing in it.
        let empty = parse_scan("now\t5\nend\n").unwrap();
        assert!(empty.entries.is_empty() && empty.rejected == 0);
    }

    #[test]
    fn a_line_that_does_not_parse_keeps_its_entry() {
        let text = "now\t1790000000\n\
                    1\t1\tgood\n\
                    fail\t1\tbroken-newest\n\
                    1\t1k\tbroken-size\n\
                    +1\t1\tsigned\n\
                    1 \t1\tspaced\n\
                    1\t1\n\
                    just text\n\
                    \n\
                    1\t1\t\n\
                    1\t1\t..\n\
                    1\t1\ta/b\n\
                    99999999999999999999999\t1\toverflow\n\
                    end\n";
        let scan = parse_scan(text).unwrap();
        assert_eq!(
            scan.entries
                .iter()
                .map(|e| e.name.as_str())
                .collect::<Vec<_>>(),
            vec!["good"]
        );
        assert_eq!(scan.rejected, 11);
    }

    // --- which volumes ---------------------------------------------------------

    /// A volume listing as `volume_ls_args` prints it: one JSON object per volume.
    fn listed(volumes: &[(&str, &str)]) -> String {
        volumes
            .iter()
            .map(|(name, kind)| serde_json::json!({"Name": name, "Kind": kind}).to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn the_targets_are_labelled_caches_and_exact_legacy_names_only() {
        let listing = listed(&[
            // Labelled: a cache whatever it is called.
            ("faber-cache-owner-demo", "cache"),
            ("renamed-by-hand", "cache"),
            // Unlabelled, and exactly the name faber mounts for a served repo: a
            // cache volume from before labels existed.
            ("faber-cache-owner-legacy", ""),
            // Unlabelled lookalikes: never a prefix or a near miss.
            ("faber-cache-owner-legacy-2", ""),
            ("faber-cache-owner-legacy.bak", ""),
            ("faber-cache-owner-legac", ""),
            ("old-faber-cache-owner-legacy", ""),
            // Unlabelled, exact name, but for a repo faber does not serve.
            ("faber-cache-owner-gone", ""),
            // Another faber kind with a cache's exact name is not a cache.
            ("faber-cache-owner-other", "vol"),
            // Somebody else's volumes.
            ("postgres-data", ""),
            ("myproj_npm-cache", ""),
            // Listed twice (both listings can't, but the rule must not care).
            ("faber-cache-owner-demo", "cache"),
        ]);
        let served = lines(&["owner/legacy", "owner/other", "owner/demo"]);
        assert_eq!(
            target_volumes(&listing, &served),
            vec![
                "faber-cache-owner-demo",
                "renamed-by-hand",
                "faber-cache-owner-legacy"
            ]
        );
    }

    #[test]
    fn without_the_served_repos_only_labelled_volumes_are_aged() {
        let listing = listed(&[
            ("faber-cache-owner-demo", "cache"),
            ("faber-cache-owner-legacy", ""),
        ]);
        assert_eq!(
            target_volumes(&listing, &[]),
            vec!["faber-cache-owner-demo"]
        );
    }

    #[test]
    fn the_legacy_name_is_the_one_faber_mounts() {
        // Matched through `cache_volume_name`, not a reimplementation of it, so a
        // repo whose slug folds (case, punctuation) still finds its volume.
        let name = docker::cache_volume_name("Owner/Some.Repo");
        let listing = listed(&[(name.as_str(), "")]);
        assert_eq!(
            target_volumes(&listing, &lines(&["Owner/Some.Repo"])),
            vec![name]
        );
    }

    /// #105: the name goes into `-v name:/c`, where a `/` mounts a host path in its
    /// place and a `:` rewrites the mount. A labelled cache with such a name — which
    /// only a daemon that is lying could report — is never aged, so nothing it would
    /// mount is ever written through.
    #[test]
    fn a_volume_name_that_would_rewrite_the_mount_is_never_aged() {
        let listing = listed(&[
            ("/", "cache"),
            ("/var/lib/docker", "cache"),
            ("vol:/etc", "cache"),
            ("-rf", "cache"),
            ("a b", "cache"),
            ("faber-cache-owner-demo", "cache"),
        ]);
        assert_eq!(
            target_volumes(&listing, &[]),
            vec!["faber-cache-owner-demo"]
        );
        // A tab-separated line from before the listing was JSON is not a row.
        assert!(target_volumes("faber-cache-owner-demo\tcache", &[]).is_empty());
    }

    // --- the containers --------------------------------------------------------

    fn value_after<'a>(args: &'a [String], flag: &str) -> Vec<&'a str> {
        args.windows(2)
            .filter(|w| w[0] == flag)
            .map(|w| w[1].as_str())
            .collect()
    }

    #[test]
    fn the_scan_mounts_the_volume_read_only_and_has_no_network() {
        // Read-only is what keeps the scan from refreshing the atimes it reads.
        let args = scan_args("faber-cache-owner-demo", "busybox", "c1");
        assert_eq!(
            value_after(&args, "-v"),
            vec!["faber-cache-owner-demo:/c:ro"]
        );
        assert_eq!(value_after(&args, "--network"), vec!["none"]);
        assert_eq!(value_after(&args, "--name"), vec!["c1"]);
        assert!(args.contains(&"--rm".to_string()));
        assert_eq!(args.last().map(String::as_str), Some(SCAN_SCRIPT));
    }

    #[test]
    fn the_delete_rechecks_read_only_and_takes_names_as_arguments() {
        let hostile = "$(touch /c/pwned); rm -rf /c #";
        let names = ["a.tar", "-rf", hostile];
        let args = delete_args("faber-cache-owner-demo", "busybox", "c2", 1234, &names);
        assert_eq!(
            value_after(&args, "-v"),
            vec!["faber-cache-owner-demo:/c", "faber-cache-owner-demo:/r:ro"]
        );
        assert_eq!(value_after(&args, "--network"), vec!["none"]);
        // `sh -c SCRIPT sh CUTOFF NAME…`: the script is the constant, untouched,
        // and every name is its own argument after it.
        let c = args.iter().position(|a| a == "-c").unwrap();
        assert_eq!(args[c + 1], DELETE_SCRIPT);
        assert_eq!(&args[c + 2..], ["sh", "1234", "a.tar", "-rf", hostile]);
        assert!(!DELETE_SCRIPT.contains("a.tar"));
    }

    #[test]
    fn the_scripts_agree_with_the_rust_on_what_is_renamed_aside() {
        // The delete script finishes anything under this prefix; `expired` sends
        // it anything under this prefix. One spelling, or leftovers leak.
        assert!(DELETE_SCRIPT.contains(&format!("/c/{EXPIRED_PREFIX}")));
        assert!(DELETE_SCRIPT.contains(&format!("{EXPIRED_PREFIX}*) continue")));
        // The re-check reads through the read-only mount; only renames and
        // deletes go through the writable one.
        assert!(DELETE_SCRIPT.contains(r#"measure "/r/$name""#));
        assert!(!DELETE_SCRIPT.contains(r#"measure "/c/"#));
        // Both end on the trailer the parsers require.
        assert!(SCAN_SCRIPT.ends_with("echo end\n"));
        assert!(DELETE_SCRIPT.ends_with("echo end\n"));
    }

    #[test]
    fn the_volume_listing_is_unfiltered_and_prints_the_kind() {
        let args = volume_ls_args();
        assert!(!args.iter().any(|a| a == "--filter"), "{args:?}");
        assert_eq!(
            args.last().map(String::as_str),
            Some(r#"{"Name":{{json .Name}},"Kind":{{json (.Label "org.faber.kind")}}}"#)
        );
    }

    /// #105: the scan's script is the constant, and the volume — the one value in
    /// its argv the daemon supplied — is held to Docker's own alphabet before it is
    /// ever put into a mount spec.
    #[tokio::test]
    async fn a_volume_name_outside_dockers_alphabet_is_never_scanned() {
        let err = scan_volume("vol:/etc", "busybox")
            .await
            .expect_err("refused before any docker call");
        assert!(err.to_string().contains("nothing scanned"), "{err}");
        let args = scan_args("faber-cache-owner-demo", "busybox", "c1");
        let c = args.iter().position(|a| a == "-c").unwrap();
        assert_eq!(args[c + 1], SCAN_SCRIPT);
        assert_eq!(c + 2, args.len(), "nothing follows the script");
    }

    #[test]
    fn throwaway_containers_are_uniquely_named_outside_the_volume_namespace() {
        let (a, b) = (container_name("scan"), container_name("scan"));
        assert_ne!(a, b);
        assert!(a.starts_with("faber-cacheage-scan-"), "{a}");
        assert!(!a.starts_with("faber-cache-"), "{a}");
    }

    // --- reading the delete's report --------------------------------------------

    #[test]
    fn the_delete_report_is_tallied_from_the_scan_without_double_counting() {
        let scan = parse_scan(
            "now\t1790000000\n\
             1780000000\t2048\told.tar\n\
             1780000000\t1024\tused-since\n\
             1789000000\t-\t.faber-expired-1789000000-1\n\
             1780000000\t512\tleftover-size\n\
             end\n",
        )
        .unwrap();
        let done = parse_delete(
            "moved\told.tar\n\
             kept\tused-since\n\
             gone\tvanished\n\
             removed\t.faber-expired-1790000100-1\n\
             removed\t.faber-expired-1789000000-1\n\
             noise without a tab\n\
             end\n",
        );
        assert_eq!(done.moved, vec!["old.tar"]);
        assert_eq!(done.kept, vec!["used-since"]);
        assert_eq!(done.gone, vec!["vanished"]);
        let reasons: HashMap<&str, Reason> = [("old.tar", Reason::Expired)].into();
        let limits = Limits {
            max_age_days: Some(7),
            max_kib: None,
        };
        let aged = report("faber-cache-owner-demo", limits, &scan, &reasons, &done);
        // old.tar, plus the leftover the scan listed; this pass's own rename is
        // old.tar again under its new name, so it is not counted twice.
        assert_eq!(
            aged,
            Aged {
                entries: 2,
                kib: 2048,
                volumes: 1,
                expired: 1,
                ..Aged::default()
            }
        );
        assert_eq!(
            aged.summary(),
            "cache entries removed: 2 (2 MiB in 1 volume(s): 1 expired)"
        );
        assert!(report("v", limits, &scan, &reasons, &Deleted::default()).is_empty());
    }

    // --- the size and pressure picks (ADR-0055) --------------------------------

    const OLD: u64 = NOW - 10 * DAY_SECS;

    fn sized(name: &str, newest: Option<u64>, kib: Option<u64>) -> Entry {
        Entry {
            name: name.to_string(),
            newest,
            kib,
        }
    }

    fn scan_of(entries: Vec<Entry>) -> Scan {
        Scan {
            now: NOW,
            entries,
            rejected: 0,
        }
    }

    #[test]
    fn the_size_key_is_off_for_absent_and_zero() {
        assert_eq!(max_kib(None), None);
        assert_eq!(max_kib(Some(0)), None);
        assert_eq!(max_kib(Some(2)), Some(2 * 1024 * 1024));
        assert!(
            operator_set(None, Some(0)),
            "0 is still the operator's choice"
        );
        assert!(operator_set(Some(0), None));
        assert!(!operator_set(None, None));
        assert!(!Limits::from_config(Some(0), Some(0)).any());
        assert!(Limits::from_config(None, Some(1)).any());
    }

    #[test]
    fn the_size_pick_takes_least_recently_used_until_under_the_cap() {
        let scan = scan_of(vec![
            sized("c", Some(OLD + 300), Some(100)),
            sized("a", Some(OLD + 100), Some(100)),
            sized("b", Some(OLD + 200), Some(100)),
            sized("d", Some(OLD + 400), Some(100)),
        ]);
        // 400 held, cap 250: two oldest go, leaving 200.
        let pick = size_pick(&scan, 250, None);
        assert_eq!(pick.names, vec!["a", "b"]);
        assert_eq!(pick.cutoff, OLD + 200);
        // Exactly at the cap stops: 400 - 100 = 300 <= 300.
        assert_eq!(size_pick(&scan, 300, None).names, vec!["a"]);
        // Already within the cap: nothing.
        assert_eq!(size_pick(&scan, 400, None), Pick::default());
    }

    #[test]
    fn the_size_pick_never_takes_what_was_used_inside_the_grace() {
        let scan = scan_of(vec![
            sized("old", Some(OLD), Some(100)),
            sized("edge", Some(NOW - SIZE_GRACE_SECS), Some(100)),
            sized("recent", Some(NOW - SIZE_GRACE_SECS + 1), Some(100)),
        ]);
        let pick = size_pick(&scan, 0, None);
        assert_eq!(pick.names, vec!["old", "edge"], "recent stays over the cap");
    }

    #[test]
    fn the_size_pick_skips_unsafe_unmeasured_and_unsized_entries() {
        let scan = scan_of(vec![
            sized("..", Some(OLD), Some(500)),
            sized("a/b", Some(OLD), Some(500)),
            sized("half-read", None, Some(500)),
            sized("unsized", Some(OLD), None),
            sized(".faber-expired-1-1", Some(OLD), Some(500)),
            sized("ok", Some(OLD + 5), Some(100)),
        ]);
        assert_eq!(size_pick(&scan, 0, None).names, vec!["ok"]);
    }

    #[test]
    fn the_size_pick_counts_what_the_age_rule_already_frees() {
        let aged = NOW - 20 * DAY_SECS;
        let scan = scan_of(vec![
            sized("stale", Some(aged), Some(300)),
            sized("a", Some(OLD + 1), Some(100)),
            sized("b", Some(OLD + 2), Some(100)),
        ]);
        // 500 held; the age rule frees 300, leaving 200 <= 250: nothing more.
        assert_eq!(size_pick(&scan, 250, Some(14)), Pick::default());
        // Cap 150: 200 left, one more entry goes, and the stale one is not repeated.
        let pick = size_pick(&scan, 150, Some(14));
        assert_eq!(pick.names, vec!["a"]);
        assert_eq!(pick.cutoff, OLD + 1);
        // Without the age rule the stale entry is simply the oldest.
        assert_eq!(size_pick(&scan, 250, None).names, vec!["stale"]);
    }

    #[test]
    fn the_size_pick_of_nothing_is_nothing() {
        assert_eq!(size_pick(&scan_of(vec![]), 0, Some(14)), Pick::default());
    }

    #[test]
    fn the_pressure_pick_is_globally_least_recently_used_and_stops_when_met() {
        let v1 = (
            "v1".to_string(),
            scan_of(vec![
                sized("a", Some(OLD + 10), Some(100)),
                sized("c", Some(OLD + 30), Some(100)),
            ]),
        );
        let v2 = (
            "v2".to_string(),
            scan_of(vec![
                sized("b", Some(OLD + 20), Some(100)),
                sized("recent", Some(NOW - 60), Some(9999)),
                sized("bad/name", Some(OLD), Some(9999)),
                sized("half-read", None, Some(9999)),
            ]),
        );
        let picks = pressure_pick(&[v1.clone(), v2.clone()], 150);
        assert_eq!(
            picks,
            vec![
                (
                    "v1".to_string(),
                    Pick {
                        names: vec!["a".into()],
                        cutoff: OLD + 10
                    }
                ),
                (
                    "v2".to_string(),
                    Pick {
                        names: vec!["b".into()],
                        cutoff: OLD + 20
                    }
                ),
            ]
        );
        // Exactly met stops: 100 needed is one entry.
        let one = pressure_pick(&[v1.clone(), v2.clone()], 100);
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].1.names, vec!["a"]);
        // More than exists takes every eligible entry and no more.
        let all = pressure_pick(&[v1, v2], u64::MAX);
        let n: usize = all.iter().map(|(_, p)| p.names.len()).sum();
        assert_eq!(n, 3);
    }

    #[test]
    fn the_pressure_pick_of_nothing_needed_or_nothing_held_is_nothing() {
        let v = (
            "v".to_string(),
            scan_of(vec![sized("a", Some(OLD), Some(10))]),
        );
        assert!(pressure_pick(&[v], 0).is_empty());
        assert!(pressure_pick(&[], 100).is_empty());
        assert!(pressure_pick(&[("v".to_string(), scan_of(vec![]))], 100).is_empty());
    }

    // --- the size key in the config ------------------------------------------

    #[test]
    fn the_size_key_parses_validates_and_round_trips() {
        assert_eq!(with_host("").host.cache_max_gib, None, "absent parses");
        for fine in ["cache_max_gib = 0", "cache_max_gib = 20"] {
            let errs = with_host(fine).validate();
            assert!(errs.is_empty(), "{fine:?}: {errs:?}");
        }
        assert_eq!(with_host("cache_max_gib = 20").host.cache_max_gib, Some(20));
        let text = with_host("cache_max_gib = 20").to_toml().unwrap();
        assert!(text.contains("cache_max_gib = 20"), "{text}");
        let text = with_host("").to_toml().unwrap();
        assert!(
            !text.contains("cache_max_gib"),
            "an absent key is not written: {text}"
        );
    }

    // --- the plan for one volume, both limits ------------------------------------

    #[test]
    fn the_plan_combines_both_limits_behind_the_larger_cutoff() {
        let stale = NOW - 20 * DAY_SECS;
        let scan = scan_of(vec![
            sized("stale", Some(stale), Some(300)),
            sized(".faber-expired-1-1", Some(NOW), Some(1)),
            sized("a", Some(OLD + 1), Some(100)),
            sized("b", Some(OLD + 2), Some(100)),
        ]);
        let limits = Limits {
            max_age_days: Some(14),
            max_kib: Some(150 * 1024 / 1024),
        };
        let (plan, cutoff_at) = plan(&scan, limits, true);
        assert_eq!(
            plan,
            vec![
                ("stale".to_string(), Reason::Expired),
                (".faber-expired-1-1".to_string(), Reason::Expired),
                ("a".to_string(), Reason::OverSize),
            ]
        );
        assert_eq!(cutoff_at, (OLD + 1).max(NOW - 14 * DAY_SECS));
    }

    #[test]
    fn a_mounted_volume_keeps_only_the_expired_entries_and_the_age_cutoff() {
        let stale = NOW - 20 * DAY_SECS;
        let scan = scan_of(vec![
            sized("stale", Some(stale), Some(300)),
            sized("a", Some(OLD + 1), Some(100)),
        ]);
        let limits = Limits {
            max_age_days: Some(14),
            max_kib: Some(1),
        };
        let (plan, cutoff_at) = plan(&scan, limits, false);
        assert_eq!(plan, vec![("stale".to_string(), Reason::Expired)]);
        assert_eq!(cutoff_at, NOW - 14 * DAY_SECS);
        // Size alone, mounted: nothing, and no cutoff.
        let size_only = Limits {
            max_age_days: None,
            max_kib: Some(1),
        };
        assert_eq!(plan_of(&scan, size_only), (vec![], 0));
    }

    fn plan_of(scan: &Scan, limits: Limits) -> (Vec<(String, Reason)>, u64) {
        plan(scan, limits, false)
    }

    #[test]
    fn a_plan_set_needs_a_pause_only_for_size_driven_deletes() {
        let vp = |plan: Vec<(String, Reason)>| VolumePlan {
            volume: "v".into(),
            scan: scan_of(vec![]),
            plan,
            cutoff: 0,
        };
        assert!(!needs_pause(&[]));
        assert!(!needs_pause(&[vp(vec![("a".into(), Reason::Expired)])]));
        assert!(needs_pause(&[
            vp(vec![("a".into(), Reason::Expired)]),
            vp(vec![("b".into(), Reason::OverSize)]),
        ]));
    }

    #[test]
    fn no_limit_plans_only_interrupted_deletes() {
        let scan = scan_of(vec![
            sized("a", Some(OLD), Some(100)),
            sized(".faber-expired-1-1", Some(NOW), Some(1)),
        ]);
        let (p, c) = plan(&scan, Limits::default(), true);
        assert_eq!(p, vec![(".faber-expired-1-1".to_string(), Reason::Expired)]);
        assert_eq!(c, 0);
    }
}
