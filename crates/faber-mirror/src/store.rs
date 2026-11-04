//! The content-addressed object store every adapter keeps its bytes in (ADR-0047
//! §4, §5, §8).
//!
//! Objects are kept once, by the lowercase hex of their hash, under
//! `objects/<aa>/<bb>/<key>`: an LFS object by its sha256 `oid`, an npm tarball
//! by the sha512 of its `dist.integrity` (or, for the oldest packages, the sha1
//! of its `shasum`). The three lengths never overlap, so one key names one
//! ecosystem's object ([`ecosystem_of`]).
//!
//! Who may read an LFS object is a separate fact: the per-repo index,
//! `index/<owner>/<repo>`, one `oid` per line, appended only after GitHub's batch
//! API confirmed that the repo has the object. An object stored for one repo is
//! never served to another until that repo's own confirmation adds it to its
//! index. npm tarballs are public, and have no index ([`Store::keep`]).
//!
//! The store is capped (`[mirror] max_size_gib`, one cap across ecosystems) and
//! evicts least recently used.
//! Eviction unlinks a file; a response already streaming it holds the open file
//! and finishes, because an unlinked inode lives until its last reader closes
//! it. That is what makes "eviction never breaks a read" hold without any
//! reader bookkeeping.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::{StoreStats, Usage};

/// Whether `oid` is a sha256 in the form LFS writes: 64 lowercase hex digits.
pub fn valid_oid(oid: &str) -> bool {
    oid.len() == 64 && lower_hex(oid)
}

/// Whether `key` names a stored object: the lowercase hex of a sha1 (40), a
/// sha256 (64) or a sha512 (128).
pub fn valid_key(key: &str) -> bool {
    matches!(key.len(), 40 | 64 | 128) && lower_hex(key)
}

fn lower_hex(s: &str) -> bool {
    s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The ecosystem a stored key belongs to: a sha256 is an LFS oid; npm keys its
/// tarballs by sha512, or sha1 for the oldest packages.
pub fn ecosystem_of(key: &str) -> &'static str {
    if key.len() == 64 {
        "lfs"
    } else {
        "npm"
    }
}

/// Whether `owner/repo` is a GitHub repository name safe to use as a path: two
/// parts of ASCII letters, digits, `.`, `_` and `-`, neither starting with `.`.
pub fn valid_repo(owner_repo: &str) -> bool {
    let Some((owner, repo)) = owner_repo.split_once('/') else {
        return false;
    };
    [owner, repo].iter().all(|p| {
        !p.is_empty()
            && p.len() <= 100
            && !p.starts_with('.')
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    })
}

#[derive(Debug, Clone, Copy)]
struct Obj {
    size: u64,
    used: u64,
}

#[derive(Default)]
struct Inner {
    objects: HashMap<String, Obj>,
    /// `(last used, oid)`, oldest first: the eviction order.
    lru: BTreeSet<(u64, String)>,
    total: u64,
    /// Bytes fills in flight have been promised: their temp files count
    /// toward the cap from the moment they start ([`Store::reserve`]).
    reserved: u64,
    clock: u64,
    index: HashMap<String, HashSet<String>>,
}

impl Inner {
    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    fn insert(&mut self, oid: &str, size: u64, used: u64) {
        if let Some(old) = self.objects.insert(oid.to_string(), Obj { size, used }) {
            self.lru.remove(&(old.used, oid.to_string()));
            self.total -= old.size;
        }
        self.lru.insert((used, oid.to_string()));
        self.total += size;
    }

    fn remove(&mut self, oid: &str) -> Option<Obj> {
        let obj = self.objects.remove(oid)?;
        self.lru.remove(&(obj.used, oid.to_string()));
        self.total -= obj.size;
        Some(obj)
    }
}

/// Why an object was not stored. The caller serves it anyway, uncached.
#[derive(Debug, PartialEq, Eq)]
pub enum NotStored {
    /// Larger than the whole cap.
    TooBig,
    /// The store could not write it.
    Io(String),
}

/// The store. One per service.
pub struct Store {
    root: PathBuf,
    max_bytes: AtomicU64,
    /// Why the store cannot be written, when its directories could not be made.
    broken: Option<String>,
    inner: Mutex<Inner>,
}

impl Store {
    /// Open the store at `root`, creating its directories and reading what a
    /// previous run left: the objects (their mtime is their last use) and the
    /// per-repo indexes. A store whose directories cannot be made still opens,
    /// broken: it serves nothing from disk and stores nothing, and the service
    /// passes every request through.
    pub fn open(root: &Path, max_bytes: u64) -> Self {
        let mut store = Self {
            root: root.to_path_buf(),
            max_bytes: AtomicU64::new(max_bytes),
            broken: None,
            inner: Mutex::new(Inner::default()),
        };
        for dir in ["objects", "tmp", "index"] {
            if let Err(e) = std::fs::create_dir_all(root.join(dir)) {
                store.broken = Some(format!(
                    "the mirror's store at {} is not usable: {e}",
                    root.display()
                ));
                return store;
            }
        }
        // A temp file is a fill that did not finish; it is never an object.
        if let Ok(rd) = std::fs::read_dir(root.join("tmp")) {
            for e in rd.flatten() {
                let _ = std::fs::remove_file(e.path());
            }
        }
        let mut found: Vec<(std::time::SystemTime, String, u64)> = Vec::new();
        for a in read_dirs(&root.join("objects")) {
            for b in read_dirs(&a) {
                let Ok(rd) = std::fs::read_dir(&b) else {
                    continue;
                };
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    let Ok(meta) = e.metadata() else { continue };
                    if valid_key(&name) && meta.is_file() {
                        let at = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                        found.push((at, name, meta.len()));
                    }
                }
            }
        }
        found.sort();
        {
            let inner = store.inner.get_mut().unwrap();
            for (_, oid, size) in found {
                let used = inner.tick();
                inner.insert(&oid, size, used);
            }
            for owner in read_dirs(&root.join("index")) {
                let Ok(rd) = std::fs::read_dir(&owner) else {
                    continue;
                };
                for e in rd.flatten() {
                    let repo = format!(
                        "{}/{}",
                        owner.file_name().unwrap_or_default().to_string_lossy(),
                        e.file_name().to_string_lossy()
                    );
                    if !valid_repo(&repo) {
                        continue;
                    }
                    let text = std::fs::read_to_string(e.path()).unwrap_or_default();
                    let set = inner.index.entry(repo).or_default();
                    set.extend(text.lines().filter(|l| valid_oid(l)).map(String::from));
                }
            }
        }
        store
    }

    /// Why the store cannot be written, if it cannot.
    pub fn broken(&self) -> Option<&str> {
        self.broken.as_deref()
    }

    /// The cap, in bytes.
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes.load(Ordering::Relaxed)
    }

    /// Change the cap. Takes effect at the next store or eviction.
    pub fn set_max_bytes(&self, bytes: u64) {
        self.max_bytes.store(bytes, Ordering::Relaxed);
    }

    /// Where the object `oid` (any [`valid_key`]) lives.
    pub fn object_path(&self, oid: &str) -> PathBuf {
        self.root
            .join("objects")
            .join(&oid[0..2])
            .join(&oid[2..4])
            .join(oid)
    }

    /// A fresh temp path for a fill of `oid`.
    pub fn tmp_path(&self, oid: &str) -> PathBuf {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        self.root.join("tmp").join(format!(
            "{oid}.{}.{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ))
    }

    /// The stored size of `oid`, when it is stored.
    pub fn has(&self, oid: &str) -> Option<u64> {
        self.inner.lock().unwrap().objects.get(oid).map(|o| o.size)
    }

    /// Whether `repo` may read `oid` from the store: it is in the repo's index
    /// and its bytes are here.
    pub fn indexed(&self, repo: &str, oid: &str) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.objects.contains_key(oid) && inner.index.get(repo).is_some_and(|s| s.contains(oid))
    }

    /// Record that GitHub confirmed `repo` has `oid`.
    pub fn grant(&self, repo: &str, oid: &str) -> Result<(), String> {
        if !valid_repo(repo) || !valid_oid(oid) {
            return Err("not a repo and an oid".into());
        }
        let mut inner = self.inner.lock().unwrap();
        if !inner
            .index
            .entry(repo.to_string())
            .or_default()
            .insert(oid.to_string())
        {
            return Ok(());
        }
        drop(inner);
        let (owner, name) = repo.split_once('/').unwrap_or_default();
        let dir = self.root.join("index").join(owner);
        std::fs::create_dir_all(&dir)
            .and_then(|_| {
                std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(dir.join(name))
            })
            .and_then(|mut f| writeln!(f, "{oid}"))
            .map_err(|e| format!("writing the index of {repo}: {e}"))
    }

    /// Mark `oid` used now: it moves to the back of the eviction order, and its
    /// file's mtime records that for the next start.
    pub fn touch(&self, oid: &str) {
        let mut inner = self.inner.lock().unwrap();
        let used = inner.tick();
        if let Some(size) = inner.objects.get(oid).map(|o| o.size) {
            inner.insert(oid, size, used);
        }
        drop(inner);
        if let Ok(f) = std::fs::File::options()
            .write(true)
            .open(self.object_path(oid))
        {
            let _ = f.set_modified(std::time::SystemTime::now());
        }
    }

    /// The file for `oid` vanished under the store: forget it.
    pub fn forget(&self, oid: &str) {
        self.inner.lock().unwrap().remove(oid);
    }

    /// Move the verified file `tmp` into the store as `oid`, making room by
    /// evicting the least recently used objects, and index it for `repo`. On
    /// any failure `tmp` is left for the caller, who serves from it and removes
    /// it.
    pub fn commit(&self, tmp: &Path, oid: &str, size: u64, repo: &str) -> Result<(), NotStored> {
        self.keep(tmp, oid, size)?;
        self.grant(repo, oid).map_err(NotStored::Io)
    }

    /// Move the verified file `tmp` into the store as `key`, with no per-repo
    /// index: for public content any job may read (an npm tarball). As
    /// [`Store::commit`] otherwise.
    pub fn keep(&self, tmp: &Path, key: &str, size: u64) -> Result<(), NotStored> {
        if !valid_key(key) {
            return Err(NotStored::Io(format!("{key:?} is not a store key")));
        }
        if let Some(why) = &self.broken {
            return Err(NotStored::Io(why.clone()));
        }
        let max = self.max_bytes();
        if size > max {
            return Err(NotStored::TooBig);
        }
        let dest = self.object_path(key);
        {
            let mut inner = self.inner.lock().unwrap();
            // The caller's reservation (which includes `size`) already counts
            // it; without one, room is made for it here.
            let held = inner.reserved.max(size);
            self.evict_locked(&mut inner, max.saturating_sub(held));
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(|e| NotStored::Io(e.to_string()))?;
            }
            std::fs::rename(tmp, &dest).map_err(|e| NotStored::Io(e.to_string()))?;
            let used = inner.tick();
            inner.insert(key, size, used);
        }
        Ok(())
    }

    /// Evict least recently used objects until the store and the fills in
    /// flight hold at most `target` bytes. Returns how many bytes went.
    pub fn evict_to(&self, target: u64) -> u64 {
        let mut inner = self.inner.lock().unwrap();
        let keep = target.saturating_sub(inner.reserved);
        self.evict_locked(&mut inner, keep)
    }

    /// Make room for a fill of `size` bytes before its temp file is written,
    /// and count them toward the cap until the guard drops. `None` when they
    /// cannot fit beside the store and the other fills in flight, even after
    /// evicting: the request then passes through and writes nothing.
    pub fn reserve(self: &Arc<Self>, size: u64) -> Option<Reservation> {
        if self.broken.is_some() {
            return None;
        }
        let max = self.max_bytes();
        let mut inner = self.inner.lock().unwrap();
        let room = max.checked_sub(inner.reserved)?.checked_sub(size)?;
        self.evict_locked(&mut inner, room);
        inner.reserved += size;
        Some(Reservation {
            store: self.clone(),
            size,
        })
    }

    fn evict_locked(&self, inner: &mut Inner, target: u64) -> u64 {
        let mut freed = 0;
        while inner.total > target {
            let Some((_, oid)) = inner.lru.first().cloned() else {
                break;
            };
            if let Some(obj) = inner.remove(&oid) {
                freed += obj.size;
            }
            // Unlinked, not truncated: a response streaming it keeps its open
            // file and finishes.
            let _ = std::fs::remove_file(self.object_path(&oid));
        }
        freed
    }

    /// What the store holds, in all and per ecosystem.
    pub fn stats(&self) -> StoreStats {
        let inner = self.inner.lock().unwrap();
        let mut usage: std::collections::BTreeMap<String, Usage> = Default::default();
        for (key, obj) in &inner.objects {
            let u = usage.entry(ecosystem_of(key).to_string()).or_default();
            u.objects += 1;
            u.bytes += obj.size;
        }
        StoreStats {
            objects: inner.objects.len() as u64,
            bytes: inner.total,
            max_bytes: self.max_bytes(),
            in_flight: inner.reserved,
            usage,
        }
    }
}

/// Room held for one fill in flight ([`Store::reserve`]).
pub struct Reservation {
    store: Arc<Store>,
    size: u64,
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut inner = self.store.inner.lock().unwrap();
        inner.reserved = inner.reserved.saturating_sub(self.size);
    }
}

fn read_dirs(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir)
        .map(|rd| {
            rd.flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .map(|e| e.path())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("faber-mirror-store-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn oid(n: u8) -> String {
        format!("{:02x}", n).repeat(32)
    }

    fn put(store: &Store, n: u8, size: usize, repo: &str) {
        let tmp = store.tmp_path(&oid(n));
        std::fs::write(&tmp, vec![n; size]).unwrap();
        store.commit(&tmp, &oid(n), size as u64, repo).unwrap();
    }

    #[test]
    fn oids_and_repos_are_validated() {
        assert!(valid_oid(&oid(1)));
        assert!(!valid_oid(&oid(1).to_uppercase().replace('0', "A")));
        assert!(!valid_oid("abc"));
        assert!(valid_repo("owner/demo"));
        assert!(valid_repo("o-1/d.e_m"));
        for bad in [
            "owner", "owner/", "/demo", "o/../x", "o/.git", "o/d/e", "o/d e",
        ] {
            assert!(!valid_repo(bad), "{bad}");
        }
    }

    #[test]
    fn keys_are_hashes_of_three_lengths_one_ecosystem_each() {
        assert!(
            valid_key(&"a".repeat(40)) && valid_key(&"b".repeat(64)) && valid_key(&"c".repeat(128))
        );
        for bad in [
            "",
            "abc",
            &"a".repeat(63),
            &"A".repeat(64),
            &"g".repeat(40),
            "../../etc/passwd",
        ] {
            assert!(!valid_key(bad), "{bad}");
        }
        assert_eq!(ecosystem_of(&oid(1)), "lfs");
        assert_eq!(ecosystem_of(&"a".repeat(128)), "npm");
        assert_eq!(ecosystem_of(&"a".repeat(40)), "npm");
    }

    #[test]
    fn a_kept_object_has_no_index_and_shares_the_cap_and_the_restart() {
        let dir = scratch("keep");
        let store = Store::open(&dir, 1 << 20);
        put(&store, 1, 10, "owner/a");
        let key = "ab".repeat(64);
        let tmp = store.tmp_path(&key);
        std::fs::write(&tmp, vec![9; 30]).unwrap();
        store.keep(&tmp, &key, 30).unwrap();
        assert_eq!(store.has(&key), Some(30));
        assert!(
            !store.indexed("owner/a", &key),
            "no repo index for a public object"
        );
        let s = store.stats();
        assert_eq!((s.objects, s.bytes), (2, 40));
        assert_eq!(
            s.usage["lfs"],
            Usage {
                objects: 1,
                bytes: 10
            }
        );
        assert_eq!(
            s.usage["npm"],
            Usage {
                objects: 1,
                bytes: 30
            }
        );
        // A restart finds both.
        let again = Store::open(&dir, 1 << 20);
        assert_eq!(again.has(&key), Some(30));
        assert_eq!(again.stats().usage["npm"].bytes, 30);
        // A key that is not a hash is never a path.
        let tmp = store.tmp_path("x");
        std::fs::write(&tmp, b"x").unwrap();
        assert!(matches!(store.keep(&tmp, "../x", 1), Err(NotStored::Io(_))));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_object_is_readable_only_by_the_repos_whose_index_names_it() {
        let dir = scratch("index");
        let store = Store::open(&dir, 1 << 20);
        put(&store, 1, 10, "owner/a");
        assert!(store.indexed("owner/a", &oid(1)));
        assert!(!store.indexed("owner/b", &oid(1)), "stored is not granted");
        assert_eq!(store.has(&oid(1)), Some(10));
        store.grant("owner/b", &oid(1)).unwrap();
        assert!(store.indexed("owner/b", &oid(1)));

        // Both survive a restart.
        let again = Store::open(&dir, 1 << 20);
        assert!(again.indexed("owner/a", &oid(1)) && again.indexed("owner/b", &oid(1)));
        assert_eq!(again.stats().bytes, 10);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cap_evicts_least_recently_used_first() {
        let dir = scratch("lru");
        let store = Store::open(&dir, 30);
        put(&store, 1, 10, "owner/a");
        put(&store, 2, 10, "owner/a");
        put(&store, 3, 10, "owner/a");
        store.touch(&oid(1));
        put(&store, 4, 10, "owner/a");
        assert_eq!(store.has(&oid(2)), None, "the least recently used went");
        assert!(store.has(&oid(1)).is_some(), "a touched one stays");
        assert!(!store.object_path(&oid(2)).exists());
        assert!(!store.indexed("owner/a", &oid(2)));
        assert_eq!(store.stats().bytes, 30);

        let tmp = store.tmp_path(&oid(5));
        std::fs::write(&tmp, vec![5; 31]).unwrap();
        assert_eq!(
            store.commit(&tmp, &oid(5), 31, "owner/a"),
            Err(NotStored::TooBig)
        );
        assert!(tmp.exists(), "left for the caller to serve from");

        assert_eq!(store.evict_to(24), 10);
        assert_eq!(store.stats().bytes, 20);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fill_in_flight_counts_toward_the_cap_and_the_low_disk_eviction() {
        let dir = scratch("reserve");
        let store = Arc::new(Store::open(&dir, 30));
        put(&store, 1, 10, "owner/a");
        put(&store, 2, 10, "owner/a");
        // A 20-byte fill makes room before writing: the oldest object goes.
        let r = store.reserve(20).unwrap();
        assert_eq!(store.has(&oid(1)), None);
        assert_eq!(store.stats().in_flight, 20);
        // A second fill cannot fit beside it and the store, even evicting.
        assert!(store.reserve(15).is_none());
        // Low disk: toward 80% of 30 = 24, counting the 20 in flight.
        store.evict_to(24);
        assert_eq!(store.stats().bytes, 0);
        drop(r);
        assert_eq!(store.stats().in_flight, 0);
        assert!(store.reserve(31).is_none(), "larger than the cap");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn eviction_never_breaks_a_read_already_open() {
        use std::io::Read as _;
        let dir = scratch("open");
        let store = Store::open(&dir, 1 << 20);
        put(&store, 7, 100_000, "owner/a");
        let mut reading = std::fs::File::open(store.object_path(&oid(7))).unwrap();
        let mut first = vec![0u8; 10];
        reading.read_exact(&mut first).unwrap();
        assert_eq!(store.evict_to(0), 100_000);
        let mut rest = Vec::new();
        reading.read_to_end(&mut rest).unwrap();
        assert_eq!(first.len() + rest.len(), 100_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_store_that_cannot_make_its_directories_opens_broken() {
        let dir = scratch("broken");
        let file = dir.join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        let store = Store::open(&file, 1 << 20);
        assert!(store.broken().is_some());
        let tmp = dir.join("t");
        std::fs::write(&tmp, b"x").unwrap();
        assert!(matches!(
            store.commit(&tmp, &oid(1), 1, "owner/a"),
            Err(NotStored::Io(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
