//! `_faber`'s read of the root checker's verdict file ([`super::report`]).
//!
//! Two readers share it: the limiter's gate ([`watch`], fed to
//! [`crate::limiter::Limiter::observe_wall`]) and the probe proof
//! ([`crate::vm::wall_proof`]), which also reads the counters the checker
//! publishes on a green tick. Both judge the file the same way
//! ([`super::report::judge_at`]): root's alone, every directory above it too,
//! fresh, and of a schema this release reads. Anything else is red.

use std::path::Path;
use std::time::Duration;

use super::report::{judge_at, Judgement, VerdictFile};
use crate::platform::record::MetaFacts;
use crate::setup::exe::PathFacts;

/// How old a verdict may be before the reader calls the checker dead. The
/// checker rewrites it every [`super::cli::DEFAULT_INTERVAL_SECS`].
pub const MAX_AGE_SECS: u64 = 30;

/// How often [`watch`] reads the verdict.
pub const POLL: Duration = Duration::from_secs(5);

/// What the reader gathered of the verdict file: its bytes, its own facts and
/// its directory's, and the chain of every directory up to `/`.
#[derive(Debug, Clone)]
pub struct Evidence {
    pub bytes: Vec<u8>,
    pub facts: MetaFacts,
    pub chain: Vec<PathFacts>,
}

/// One read of the verdict file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    pub judgement: Judgement,
    /// The parsed file, only when the judgement is not red: a red file's
    /// contents are never acted on.
    pub file: Option<VerdictFile>,
}

impl Reading {
    fn red(why: String) -> Self {
        Reading {
            judgement: Judgement::Red(why),
            file: None,
        }
    }
}

/// Judge what was read at `now_unix`. Pure.
pub fn judge_reading(evidence: Result<Evidence, String>, now_unix: u64) -> Reading {
    let e = match evidence {
        Ok(e) => e,
        Err(why) => return Reading::red(format!("the verdict file cannot be read: {why}")),
    };
    let judgement = judge_at(
        &e.bytes,
        &e.facts,
        &e.chain,
        now_unix,
        MAX_AGE_SECS,
        super::RULES_VERSION,
    );
    if let Judgement::Red(_) = judgement {
        return Reading {
            judgement,
            file: None,
        };
    }
    match serde_json::from_slice(&e.bytes) {
        Ok(file) => Reading {
            judgement,
            file: Some(file),
        },
        Err(why) => Reading::red(format!("the verdict file does not parse: {why}")),
    }
}

/// The verdict file at `path`: its bytes, refused past
/// [`super::paths::STORED_READ_CAP`], its facts, its directory's, and its chain
/// of every directory up to `/`.
///
/// The file is opened once, never through a symlink at `path` itself
/// (`O_NOFOLLOW`), and judged by that descriptor's own `fstat`: its owner,
/// mode and type, and the chain's entry for the file itself. So the facts and
/// the bytes come from one file, whatever happens to the name meanwhile. The
/// directories above it are judged by path, and every one must be root's
/// alone, so only root could change what they hold. The checker replaces the
/// file by rename on every tick, so a name that moves on after the open is
/// expected, and changes nothing about what was judged.
pub fn read_evidence(path: &Path) -> Result<Evidence, String> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};

    let shown = |e: std::io::Error| format!("{}: {e}", path.display());
    let mut opened = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(crate::setup::fsx::READ_NOFOLLOW)
        .open(path)
        .map_err(|e| match std::fs::symlink_metadata(path) {
            Ok(m) if m.file_type().is_symlink() => {
                format!("{}: a symlink, which is never followed", path.display())
            }
            _ => shown(e),
        })?;
    let file = opened.metadata().map_err(shown)?;
    let parent = path.parent().unwrap_or(Path::new("/"));
    let dir = std::fs::symlink_metadata(parent).map_err(shown)?;
    let facts = MetaFacts {
        // `O_NOFOLLOW` refused a symlink, and `fstat` never reports one.
        is_symlink: file.file_type().is_symlink(),
        is_regular: file.file_type().is_file(),
        uid: file.uid(),
        mode: file.mode(),
        parent_is_dir: dir.file_type().is_dir(),
        parent_uid: dir.uid(),
        parent_mode: dir.mode(),
    };
    let mut chain = crate::setup::exe::chain_of(path).map_err(shown)?;
    // The file's own entry, from the descriptor rather than the name.
    let leaf = chain
        .first_mut()
        .ok_or_else(|| format!("{}: no path facts", path.display()))?;
    leaf.uid = file.uid();
    leaf.gid = file.gid();
    leaf.mode = file.mode();
    // The verdict carries counters for every network, so it grows with the
    // plan; a larger one is refused rather than read cut off.
    let cap = super::paths::STORED_READ_CAP;
    let mut bytes = Vec::new();
    (&mut opened)
        .take(cap + 1)
        .read_to_end(&mut bytes)
        .map_err(shown)?;
    if bytes.len() as u64 > cap {
        return Err(format!(
            "{} is larger than {cap} bytes; refusing it",
            path.display()
        ));
    }
    Ok(Evidence {
        bytes,
        facts,
        chain,
    })
}

/// The time now, in unix seconds; 0 if the clock is before 1970, which every
/// verdict then reads as dated in the future (red).
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The time now, in unix nanoseconds; 0 if the clock is before 1970.
pub fn now_unix_nanos() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
}

/// The time now on the host's monotonic clock (`CLOCK_MONOTONIC`), in
/// nanoseconds; 0 if it cannot be read, which no probe proof accepts.
///
/// One clock for every process on the host: the root checker stamps its
/// counter reads with it and `_faber` compares its own readings against them
/// ([`crate::vm::wall_proof`]). Unlike the wall clock it never steps, so a
/// reading the checker took before a probe can never look newer than it.
pub fn now_monotonic_nanos() -> u64 {
    use std::os::raw::{c_int, c_long};

    #[repr(C)]
    struct Timespec {
        tv_sec: c_long,
        tv_nsec: c_long,
    }
    extern "C" {
        /// POSIX, on both macOS and Linux.
        fn clock_gettime(clock: c_int, tp: *mut Timespec) -> c_int;
    }
    #[cfg(target_os = "macos")]
    const CLOCK_MONOTONIC: c_int = 6;
    #[cfg(not(target_os = "macos"))]
    const CLOCK_MONOTONIC: c_int = 1;

    let mut t = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: `t` is a valid, writable `struct timespec` for the call.
    if unsafe { clock_gettime(CLOCK_MONOTONIC, &mut t) } != 0 {
        return 0;
    }
    let (Ok(secs), Ok(nanos)) = (u64::try_from(t.tv_sec), u64::try_from(t.tv_nsec)) else {
        return 0;
    };
    secs.saturating_mul(1_000_000_000).saturating_add(nanos)
}

/// Read and judge the checker's verdict file where setup installs it.
/// Blocking: call it from `spawn_blocking`.
pub fn read_now() -> Reading {
    judge_reading(
        read_evidence(Path::new(super::paths::VERDICT_CANONICAL)),
        now_unix(),
    )
}

/// [`read_now`] off the async runtime. A read that does not finish is red.
pub async fn read() -> Reading {
    tokio::task::spawn_blocking(read_now)
        .await
        .unwrap_or_else(|e| Reading::red(format!("the verdict read did not finish: {e}")))
}

/// Feed the limiter's wall gate: judge the verdict at once and every
/// [`POLL`]. A poller that stops leaves the gate to go stale and close
/// ([`crate::vm::admit::WALL_MAX_AGE_MS`]).
pub async fn watch(limiter: std::sync::Arc<crate::limiter::Limiter>) {
    loop {
        limiter.observe_wall(read().await.judgement);
        tokio::time::sleep(POLL).await;
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::super::report::State;
    use super::*;

    pub(crate) const NOW: u64 = 1_800_000_000;

    pub(crate) fn root_facts() -> MetaFacts {
        MetaFacts {
            is_symlink: false,
            is_regular: true,
            uid: 0,
            mode: 0o100644,
            parent_is_dir: true,
            parent_uid: 0,
            parent_mode: 0o40755,
        }
    }

    pub(crate) fn root_chain() -> Vec<PathFacts> {
        [
            "/private/var/db/faber/wall/verdict.json",
            "/private/var/db/faber/wall",
            "/",
        ]
        .iter()
        .map(|p| PathFacts {
            path: p.into(),
            uid: 0,
            gid: 0,
            mode: if p.ends_with(".json") {
                0o100644
            } else {
                0o40755
            },
        })
        .collect()
    }

    pub(crate) fn evidence(file: &VerdictFile) -> Evidence {
        Evidence {
            bytes: file.to_json(),
            facts: root_facts(),
            chain: root_chain(),
        }
    }

    pub(crate) fn green_at(at: u64) -> VerdictFile {
        VerdictFile {
            schema: super::super::report::SCHEMA,
            state: State::Green,
            reasons: vec![],
            rules_version: super::super::RULES_VERSION,
            digest: "d".repeat(64),
            pending_networks: vec![],
            checked_at_unix: at,
            load_generation: 7,
            counters: None,
        }
    }

    #[test]
    fn a_fresh_green_file_is_green_and_parsed() {
        let r = judge_reading(Ok(evidence(&green_at(NOW))), NOW);
        assert_eq!(r.judgement, Judgement::Green);
        assert_eq!(r.file, Some(green_at(NOW)));
    }

    #[test]
    fn red_stale_untrusted_or_unreadable_is_red_with_no_file() {
        let mut red = green_at(NOW);
        red.state = State::Red;
        let mut writable = evidence(&green_at(NOW));
        writable.facts.mode = 0o100666;
        let mut faber_dir = evidence(&green_at(NOW));
        faber_dir.chain[1].uid = 451;
        for (what, e) in [
            ("red", Ok(evidence(&red))),
            ("stale", Ok(evidence(&green_at(NOW - MAX_AGE_SECS - 1)))),
            ("future", Ok(evidence(&green_at(NOW + MAX_AGE_SECS + 1)))),
            ("writable", Ok(writable)),
            ("a directory _faber owns", Ok(faber_dir)),
            ("unreadable", Err("ENOENT".to_string())),
        ] {
            let r = judge_reading(e, NOW);
            assert!(matches!(r.judgement, Judgement::Red(_)), "{what}: {r:?}");
            assert_eq!(r.file, None, "{what}");
        }
    }

    /// A fresh directory under the temp dir, removed by the caller.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("faber-reader-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_monotonic_clock_reads_and_moves_forward() {
        let a = now_monotonic_nanos();
        std::thread::sleep(std::time::Duration::from_millis(2));
        let b = now_monotonic_nanos();
        assert!(a > 0, "the clock did not read");
        assert!(b >= a + 1_000_000, "{a} then {b}");
    }

    #[test]
    fn a_symlink_at_the_verdict_path_is_never_followed() {
        let dir = scratch("symlink");
        let real = dir.join("real.json");
        std::fs::write(&real, green_at(NOW).to_json()).unwrap();
        let link = dir.join("verdict.json");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let read = read_evidence(&link);
        std::fs::remove_dir_all(&dir).unwrap();
        let why = read.expect_err("a symlink was followed");
        assert!(why.contains("symlink"), "{why}");
        // And a reading of it is red.
        let r = judge_reading(Err(why), NOW);
        assert!(matches!(r.judgement, Judgement::Red(_)), "{r:?}");
    }

    #[test]
    fn the_facts_are_the_opened_files_own() {
        use std::os::unix::fs::MetadataExt;
        let dir = scratch("fstat");
        let path = dir.join("verdict.json");
        let bytes = green_at(NOW).to_json();
        std::fs::write(&path, &bytes).unwrap();
        let read = read_evidence(&path);
        let meta = std::fs::metadata(&path).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
        let e = read.unwrap();
        assert_eq!(e.bytes, bytes);
        assert!(e.facts.is_regular && !e.facts.is_symlink);
        assert_eq!((e.facts.uid, e.facts.mode), (meta.uid(), meta.mode()));
        assert_eq!(
            (e.chain[0].uid, e.chain[0].gid, e.chain[0].mode),
            (meta.uid(), meta.gid(), meta.mode())
        );
        assert!(e.facts.parent_is_dir);
    }

    /// A verdict over the cap is refused, not read cut off, and a reading of
    /// it is red: it never parses as a shorter, different verdict.
    #[test]
    fn an_oversize_verdict_is_refused_and_red() {
        let dir = scratch("oversize");
        let path = dir.join("verdict.json");
        let size = usize::try_from(super::super::paths::STORED_READ_CAP).unwrap() + 1;
        std::fs::write(&path, vec![b' '; size]).unwrap();
        let read = read_evidence(&path);
        std::fs::remove_dir_all(&dir).unwrap();
        let why = read.expect_err("an oversize verdict was read");
        assert!(why.contains("larger than"), "{why}");
        let r = judge_reading(Err(why), NOW);
        assert!(matches!(r.judgement, Judgement::Red(_)), "{r:?}");
    }

    #[test]
    fn older_rules_are_out_of_date_and_still_parsed() {
        let mut old = green_at(NOW);
        old.rules_version = 0;
        let r = judge_reading(Ok(evidence(&old)), NOW);
        assert!(matches!(r.judgement, Judgement::OutOfDate { .. }), "{r:?}");
        assert!(r.file.is_some());
    }
}
