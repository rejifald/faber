//! The trust-change notice (ADR-0028): the daemon lists every allowed trust
//! widening of the last 7 days in `/api/state`'s `trust_changes`, and offers no
//! way to clear one. The app keeps its own copy of what it has seen, in its own
//! config directory (which `_faber` cannot write), so a daemon list that loses
//! an entry early, by a restart that dropped the audit log or by tampering,
//! shows as "shrank" rather than as good news.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// How long a change stays on the daemon's list, and in our copy.
pub const WINDOW_MS: u64 = 7 * 24 * 3600 * 1000;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Seen {
    pub ts_ms: u64,
    pub key: String,
    pub sentence: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SeenFile {
    #[serde(default)]
    seen: Vec<Seen>,
}

fn seen_path(dir: &Path) -> PathBuf {
    dir.join("trust_seen.json")
}

pub fn load_seen(dir: &Path) -> Vec<Seen> {
    std::fs::read_to_string(seen_path(dir))
        .ok()
        .and_then(|t| serde_json::from_str::<SeenFile>(&t).ok())
        .unwrap_or_default()
        .seen
}

pub fn save_seen(dir: &Path, seen: &[Seen]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = seen_path(dir);
    let tmp = path.with_extension("tmp");
    let body = serde_json::to_vec_pretty(&SeenFile {
        seen: seen.to_vec(),
    })
    .map_err(std::io::Error::other)?;
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, path)
}

/// The daemon's `trust_changes`, tolerantly: absent, not an array, or an entry
/// without `ts_ms`/`key` (an older or odd daemon) yields what can be read.
pub fn parse_changes(state: &serde_json::Value) -> Vec<Seen> {
    state
        .get("trust_changes")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| {
                    Some(Seen {
                        ts_ms: e.get("ts_ms")?.as_u64()?,
                        key: e.get("key")?.as_str()?.to_string(),
                        sentence: e
                            .get("sentence")
                            .and_then(|s| s.as_str())
                            .unwrap_or_default()
                            .to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, PartialEq, Eq)]
pub struct Outcome {
    /// Some seen entry still inside the window is missing from the daemon's list.
    pub shrank: bool,
    /// Our copy after folding the daemon's list in, pruned past the window.
    pub seen: Vec<Seen>,
}

/// Pure: fold the daemon's list into what we have seen. The entries to show are
/// the daemon's own list; this decides the warning and the copy to keep.
pub fn evaluate(seen: &[Seen], daemon: &[Seen], now_ms: u64) -> Outcome {
    let live = |e: &Seen| now_ms.saturating_sub(e.ts_ms) < WINDOW_MS;
    let same = |a: &Seen, b: &Seen| a.ts_ms == b.ts_ms && a.key == b.key;
    let shrank = seen
        .iter()
        .any(|s| live(s) && !daemon.iter().any(|d| same(s, d)));
    let mut kept: Vec<Seen> = seen.iter().filter(|e| live(e)).cloned().collect();
    for d in daemon.iter().filter(|e| live(e)) {
        if !kept.iter().any(|k| same(k, d)) {
            kept.push(d.clone());
        }
    }
    Outcome { shrank, seen: kept }
}

/// Whether `/api/state` is standby's stub: it has no audit log, so its
/// `trust_changes` is always empty and says nothing about the record.
pub fn is_standby_stub(state: &serde_json::Value) -> bool {
    state.get("standby").is_some_and(|v| !v.is_null())
}

/// Pure: the decision for one tick. `None` means leave the saved copy alone and
/// raise no alarm (a standby stub); otherwise the [`evaluate`] outcome.
pub fn decide(seen: &[Seen], state: &serde_json::Value, now_ms: u64) -> Option<Outcome> {
    if is_standby_stub(state) {
        return None;
    }
    Some(evaluate(seen, &parse_changes(state), now_ms))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DAY: u64 = 24 * 3600 * 1000;

    fn e(ts_ms: u64, key: &str) -> Seen {
        Seen {
            ts_ms,
            key: key.into(),
            sentence: format!("{key} widened"),
        }
    }

    #[test]
    fn first_sight_records_and_does_not_shrink() {
        let now = 10 * DAY;
        let d = vec![e(now - DAY, "update.enabled")];
        let o = evaluate(&[], &d, now);
        assert!(!o.shrank);
        assert_eq!(o.seen, d);
    }

    #[test]
    fn a_missing_entry_inside_the_window_shrinks() {
        let now = 10 * DAY;
        let seen = vec![e(now - DAY, "a"), e(now - 2 * DAY, "b")];
        let o = evaluate(&seen, &[e(now - DAY, "a")], now);
        assert!(o.shrank);
        assert_eq!(
            o.seen.len(),
            2,
            "the missing entry is kept until it ages out"
        );
    }

    #[test]
    fn an_aged_out_entry_is_not_a_shrink_and_is_pruned() {
        let now = 10 * DAY;
        let seen = vec![e(now - 8 * DAY, "old"), e(now - DAY, "a")];
        let o = evaluate(&seen, &[e(now - DAY, "a")], now);
        assert!(!o.shrank);
        assert_eq!(o.seen, vec![e(now - DAY, "a")]);
    }

    #[test]
    fn an_empty_daemon_list_with_live_seen_entries_shrinks() {
        let now = 10 * DAY;
        assert!(evaluate(&[e(now - DAY, "a")], &[], now).shrank);
    }

    #[test]
    fn same_ts_different_key_is_a_different_entry() {
        let now = 10 * DAY;
        let o = evaluate(&[e(now - DAY, "a")], &[e(now - DAY, "b")], now);
        assert!(o.shrank);
    }

    #[test]
    fn parsing_is_tolerant() {
        assert!(parse_changes(&serde_json::json!({})).is_empty());
        assert!(parse_changes(&serde_json::json!({"trust_changes": "x"})).is_empty());
        let v = serde_json::json!({"trust_changes": [
            {"ts_ms": 5, "key": "k", "sentence": "s", "via": "socket", "uid": null, "route": "r"},
            {"key": "no ts"}, 7
        ]});
        assert_eq!(
            parse_changes(&v),
            vec![Seen {
                ts_ms: 5,
                key: "k".into(),
                sentence: "s".into()
            }]
        );
    }

    #[test]
    fn the_copy_round_trips_through_a_file() {
        let dir = std::env::temp_dir().join(format!("faber-trust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(load_seen(&dir).is_empty());
        save_seen(&dir, &[e(1, "a")]).unwrap();
        assert_eq!(load_seen(&dir), vec![e(1, "a")]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_standby_stub_is_skipped_and_never_shrinks() {
        let now = 10 * DAY;
        let seen = vec![e(now - DAY, "a")];
        let stub = serde_json::json!({"trust_changes": [], "standby": {"since_ms": 1}});
        assert!(decide(&seen, &stub, now).is_none());
        let live = serde_json::json!({"trust_changes": []});
        assert!(decide(&seen, &live, now).expect("evaluated").shrank);
    }
}
