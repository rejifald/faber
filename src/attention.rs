//! The operator's dismissals of attention rows, kept so a restart and every
//! client agree on them (ADR a-warning-that-does-not-stop-jobs-can-be-dismissed,
//! ADR-0064 §3).
//!
//! The daemon does not know which rows exist: the UI decides that, and hands
//! over a row's `key` and its `ident` (what the row said when it was dismissed)
//! as opaque strings. Storing them here, not in the browser, is what makes a
//! dismissal in the popover hold in the dashboard and survive a restart.

use std::path::PathBuf;
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// The longest `key` accepted, in bytes.
pub const MAX_KEY_BYTES: usize = 256;
/// The longest `ident` accepted, in bytes.
pub const MAX_IDENT_BYTES: usize = 4096;
/// The most dismissals kept; the oldest goes when a new one needs the room.
pub const MAX_DISMISSED: usize = 256;

/// One dismissed row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dismissal {
    pub key: String,
    pub ident: String,
    pub at_ms: u64,
}

/// What `/api/state` carries as `attention`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionView {
    #[serde(default)]
    pub dismissed: Vec<Dismissal>,
    /// Trust changes at or before this time are acknowledged (ADR-0064 §4).
    #[serde(default)]
    pub trust_acked_ms: Option<u64>,
}

/// The dismissals, in memory and (when it has a path) in `attention.json`.
pub struct AttentionStore {
    state: Mutex<AttentionView>,
    /// `None` in tests, which must never touch the real file.
    path: Option<PathBuf>,
    now: fn() -> u64,
}

impl AttentionStore {
    /// An empty store that persists nowhere.
    pub fn in_memory() -> Self {
        Self {
            state: Mutex::default(),
            path: None,
            now: crate::audit::now_ms,
        }
    }

    /// Load `<state dir>/attention.json`. Missing is empty; unreadable or corrupt
    /// is empty with a warning: a dismissal is a convenience and must never
    /// stop the daemon booting.
    pub fn load(path: Option<PathBuf>) -> Self {
        let state = match path.as_deref().map(std::fs::read_to_string) {
            None => AttentionView::default(),
            Some(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => AttentionView::default(),
            Some(Err(e)) => {
                tracing::warn!("attention dismissals not loaded: {e}");
                AttentionView::default()
            }
            Some(Ok(text)) => serde_json::from_str(&text).unwrap_or_else(|e| {
                tracing::warn!("attention.json is not valid, starting empty: {e}");
                AttentionView::default()
            }),
        };
        Self {
            state: Mutex::new(state),
            path,
            now: crate::audit::now_ms,
        }
    }

    /// The default location, beside `overrides.json`.
    pub fn default_path() -> Option<PathBuf> {
        crate::platform::state_dir().map(|d| d.join("attention.json"))
    }

    pub fn view(&self) -> AttentionView {
        self.state.lock().unwrap().clone()
    }

    /// Add the dismissal for `key`, or replace the one there.
    pub fn dismiss(&self, key: &str, ident: &str) -> Result<(), String> {
        if key.is_empty() {
            return Err("key must not be empty".to_string());
        }
        if key.len() > MAX_KEY_BYTES {
            return Err(format!("key is longer than {MAX_KEY_BYTES} bytes"));
        }
        if ident.len() > MAX_IDENT_BYTES {
            return Err(format!("ident is longer than {MAX_IDENT_BYTES} bytes"));
        }
        let mut s = self.state.lock().unwrap();
        s.dismissed.retain(|d| d.key != key);
        s.dismissed.push(Dismissal {
            key: key.to_string(),
            ident: ident.to_string(),
            at_ms: (self.now)(),
        });
        while s.dismissed.len() > MAX_DISMISSED {
            // Oldest by time; the first of equals, which is the earliest added.
            let oldest = (0..s.dismissed.len())
                .min_by_key(|&i| s.dismissed[i].at_ms)
                .unwrap_or(0);
            s.dismissed.remove(oldest);
        }
        self.persist(&s);
        Ok(())
    }

    /// Remove `key`'s dismissal, or every one when `key` is `None`.
    pub fn restore(&self, key: Option<&str>) {
        let mut s = self.state.lock().unwrap();
        match key {
            Some(k) => s.dismissed.retain(|d| d.key != k),
            None => s.dismissed.clear(),
        }
        self.persist(&s);
    }

    /// Acknowledge trust changes up to `until_ms`; `None` clears it.
    pub fn ack_trust(&self, until_ms: Option<u64>) {
        let mut s = self.state.lock().unwrap();
        s.trust_acked_ms = until_ms;
        self.persist(&s);
    }

    /// Write temp-then-rename while the lock is held, so two writers cannot
    /// leave an older state on disk. A failed write is logged, not surfaced: the
    /// dismissal still holds until the next restart.
    fn persist(&self, s: &AttentionView) {
        let Some(path) = &self.path else { return };
        let result = (|| -> anyhow::Result<()> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let tmp = path.with_extension("json.tmp");
            std::fs::write(&tmp, serde_json::to_vec_pretty(s)?)?;
            std::fs::rename(&tmp, path)?;
            Ok(())
        })();
        if let Err(e) = result {
            tracing::warn!("attention dismissals not saved to {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reconcile::tests::TempDir;
    use std::sync::atomic::{AtomicU64, Ordering};

    static CLOCK: AtomicU64 = AtomicU64::new(1);

    /// A store whose clock ticks once per read, so `at_ms` orders the writes.
    fn store(path: Option<PathBuf>) -> AttentionStore {
        let mut s = AttentionStore::load(path);
        s.now = || CLOCK.fetch_add(1, Ordering::Relaxed);
        s
    }

    #[test]
    fn dismissing_a_key_again_replaces_it() {
        let s = store(None);
        s.dismiss("memory:a", "one").unwrap();
        s.dismiss("cpu:a", "x").unwrap();
        s.dismiss("memory:a", "two").unwrap();
        let v = s.view();
        assert_eq!(v.dismissed.len(), 2);
        let d = v.dismissed.iter().find(|d| d.key == "memory:a").unwrap();
        assert_eq!(d.ident, "two");
    }

    #[test]
    fn restore_removes_one_key_or_all() {
        let s = store(None);
        s.dismiss("a", "").unwrap();
        s.dismiss("b", "").unwrap();
        s.restore(Some("a"));
        assert_eq!(s.view().dismissed.len(), 1);
        assert_eq!(s.view().dismissed[0].key, "b");
        s.dismiss("c", "").unwrap();
        s.restore(None);
        assert!(s.view().dismissed.is_empty());
    }

    #[test]
    fn a_full_store_drops_the_oldest() {
        let s = store(None);
        for i in 0..MAX_DISMISSED {
            s.dismiss(&format!("k{i}"), "").unwrap();
        }
        s.dismiss("new", "").unwrap();
        let v = s.view();
        assert_eq!(v.dismissed.len(), MAX_DISMISSED);
        assert!(v.dismissed.iter().all(|d| d.key != "k0"));
        assert!(v.dismissed.iter().any(|d| d.key == "k1"));
        assert!(v.dismissed.iter().any(|d| d.key == "new"));
    }

    #[test]
    fn oversized_or_empty_input_is_rejected() {
        let s = store(None);
        assert!(s.dismiss("", "x").is_err());
        assert!(s.dismiss(&"k".repeat(MAX_KEY_BYTES + 1), "x").is_err());
        assert!(s.dismiss("k", &"i".repeat(MAX_IDENT_BYTES + 1)).is_err());
        assert!(s
            .dismiss(&"k".repeat(MAX_KEY_BYTES), &"i".repeat(MAX_IDENT_BYTES))
            .is_ok());
        assert_eq!(s.view().dismissed.len(), 1);
    }

    #[test]
    fn the_trust_acknowledgement_sets_and_clears() {
        let s = store(None);
        s.ack_trust(Some(42));
        assert_eq!(s.view().trust_acked_ms, Some(42));
        s.ack_trust(None);
        assert_eq!(s.view().trust_acked_ms, None);
    }

    #[test]
    fn dismissals_survive_a_reload() {
        let tmp = TempDir::new();
        let path = tmp.path.join("nested").join("attention.json");
        let s = store(Some(path.clone()));
        s.dismiss("memory:a", "ident").unwrap();
        s.ack_trust(Some(7));
        let back = AttentionStore::load(Some(path)).view();
        assert_eq!(back, s.view());
        assert_eq!(back.dismissed[0].ident, "ident");
        assert_eq!(back.trust_acked_ms, Some(7));
    }

    #[test]
    fn a_missing_or_corrupt_file_loads_empty() {
        let tmp = TempDir::new();
        let path = tmp.path.join("attention.json");
        assert_eq!(
            AttentionStore::load(Some(path.clone())).view(),
            AttentionView::default()
        );
        std::fs::write(&path, "{ not json").unwrap();
        assert_eq!(
            AttentionStore::load(Some(path)).view(),
            AttentionView::default()
        );
    }
}
