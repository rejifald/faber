//! Putting the kept runner image into faber's Linux VM (ADR-0026, "The runner
//! image moves first").
//!
//! The runner image is local-only: in no registry, and it stays so. At ADR-0031's
//! cut the operator `docker save`s it out of Docker Desktop and `sudo faber setup
//! --runner-image-tar` keeps the archive under
//! [`crate::setup::vm_images::runner_dir`], `root:_faber` 0440. The Linux VM's
//! Docker starts empty, and a rebuilt engine disk is empty again, so loading
//! the image is the daemon's job each time the engine comes into service, not a
//! step an operator repeats.
//!
//! On every such transition [`LoadingSink`] holds the engine back
//! ([`EngineStatus::LoadingImage`]: jobs wait, they do not fail on a pull that
//! runners started with `--pull=never` cannot do) and runs [`Loader::reconcile`]:
//!
//! 1. For each archive in the directory, read the `RepoTags` of its top-level
//!    `manifest.json`: the headers are walked and that one member read, never
//!    the layers.
//! 2. Ask the engine whether it has every one of those tags
//!    (`GET /images/<tag>/json`). All present: nothing to do.
//! 3. Otherwise stream the archive into `POST /images/load`, as `_faber`
//!    through the VM's socket ([`SocketEngine`]), and ask again.
//! 4. Say what is wrong, if anything: an archive that could not be read or
//!    loaded, or a configured profile image that is neither in the engine nor
//!    in any archive ("missing, no backup"). That goes to `/api/state`
//!    (`budget.vm.runner_image`), the dashboard and the tray.
//!
//! The archive is opened read-only and never written, renamed or removed: it is
//! the image's only backup (AGENTS.md, never-reap). Loads are bounded: one at a
//! time ([`Loader`] holds a lock across a pass), a timeout each, and an archive
//! over [`MAX_TARBALL_BYTES`] is refused.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use tracing::{info, warn};

use super::engine::{EngineSink, EngineStatus};
use crate::setup::vm_images::MAX_TAR_ENTRIES;

/// The largest archive that is loaded. A runner image is a few GiB; this is the
/// bound on a file in a directory only root can write.
pub const MAX_TARBALL_BYTES: u64 = 32 << 30;

/// The most archives looked at in the directory.
pub const MAX_TARBALLS: usize = 16;

/// The largest `manifest.json` that is read.
const MAX_MANIFEST_BYTES: u64 = 4 << 20;

/// The most of a response that is read: `/images/load` answers with a line per
/// layer.
const MAX_RESPONSE_BYTES: u64 = 1 << 20;

/// How long each call may take.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoadTimeouts {
    /// Asking whether the engine has an image.
    pub check: Duration,
    /// One `docker load`, from the first byte sent to the last answered.
    pub load: Duration,
}

impl Default for LoadTimeouts {
    fn default() -> Self {
        Self {
            check: Duration::from_secs(30),
            load: Duration::from_secs(30 * 60),
        }
    }
}

/// The engine's Docker, as far as the loader needs it.
pub trait ImageEngine: Send + Sync {
    /// Whether the engine has the image `reference`.
    fn has_image(
        &self,
        reference: &str,
    ) -> impl std::future::Future<Output = Result<bool, String>> + Send;
    /// Stream the `docker save` archive at `path` (`len` bytes) into the engine.
    fn load(
        &self,
        path: &Path,
        len: u64,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;
}

/// One archive in the directory, as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    pub name: String,
    pub path: PathBuf,
    pub len: u64,
    /// The `RepoTags` of its manifest, or why they could not be read.
    pub tags: Result<Vec<String>, String>,
}

/// What a pass came to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    /// Archives streamed into the engine.
    pub loaded: Vec<String>,
    /// Archives whose tags the engine already had.
    pub present: Vec<String>,
    /// Archives that could not be loaded, with why.
    pub failed: Vec<(String, String)>,
    /// Configured images in neither the engine nor any archive.
    pub no_backup: Vec<String>,
}

impl Report {
    /// The sentence for `/api/state`, or `None` when nothing is wrong.
    pub fn problem(&self) -> Option<String> {
        let mut parts = Vec::new();
        for (name, why) in &self.failed {
            parts.push(format!("the kept runner image {name} is not usable: {why}"));
        }
        for image in &self.no_backup {
            parts.push(format!(
                "runner image {image} is missing from faber's Linux VM and no kept backup \
                 provides it — jobs that need it fail; keep a `docker save` archive of it with \
                 `sudo faber setup --runner-image-tar`"
            ));
        }
        (!parts.is_empty()).then(|| parts.join("; "))
    }
}

/// The loader: one per engine, holding the lock that keeps loads to one at a time.
pub struct Loader<E> {
    engine: E,
    dir: PathBuf,
    timeouts: LoadTimeouts,
    one_at_a_time: tokio::sync::Mutex<()>,
}

impl<E: ImageEngine> Loader<E> {
    pub fn new(engine: E, dir: PathBuf, timeouts: LoadTimeouts) -> Self {
        Self {
            engine,
            dir,
            timeouts,
            one_at_a_time: tokio::sync::Mutex::new(()),
        }
    }

    async fn has(&self, reference: &str) -> Result<bool, String> {
        match tokio::time::timeout(self.timeouts.check, self.engine.has_image(reference)).await {
            Ok(r) => r,
            Err(_) => Err(format!("no answer within {:?}", self.timeouts.check)),
        }
    }

    /// Bring the engine up to what the kept archives hold, then judge `wanted`
    /// (the profile images). Never fails: what went wrong is in the [`Report`].
    pub async fn reconcile(&self, wanted: &[String]) -> Report {
        self.reconcile_unless(wanted, || false).await
    }

    /// [`Self::reconcile`], except that when `stale` says so once the lock is
    /// taken the pass does nothing: a pass that waited behind another while the
    /// engine flapped has no one left to report to.
    pub async fn reconcile_unless(&self, wanted: &[String], stale: impl Fn() -> bool) -> Report {
        let _one = self.one_at_a_time.lock().await;
        if stale() {
            return Report::default();
        }
        let dir = self.dir.clone();
        let kept = tokio::task::spawn_blocking(move || scan(&dir))
            .await
            .unwrap_or_default();
        let mut report = Report::default();
        let mut provided: Vec<String> = Vec::new();

        for k in &kept {
            let tags = match &k.tags {
                Ok(t) if !t.is_empty() => t,
                Ok(_) => {
                    report.failed.push((
                        k.name.clone(),
                        "its manifest has no RepoTags, so faber cannot tell whether it is loaded"
                            .into(),
                    ));
                    continue;
                }
                Err(why) => {
                    report.failed.push((k.name.clone(), why.clone()));
                    continue;
                }
            };
            provided.extend(tags.iter().map(|t| normalize(t)));

            match self.missing_of(tags).await {
                Err(why) => {
                    report
                        .failed
                        .push((k.name.clone(), format!("asking the engine: {why}")));
                    continue;
                }
                Ok(missing) if missing.is_empty() => {
                    report.present.push(k.name.clone());
                    continue;
                }
                Ok(_) => {}
            }
            if k.len > MAX_TARBALL_BYTES {
                report.failed.push((
                    k.name.clone(),
                    format!(
                        "it is {} bytes, over the {MAX_TARBALL_BYTES}-byte limit",
                        k.len
                    ),
                ));
                continue;
            }
            info!(
                "runner image: loading {} ({} bytes) into faber's Linux VM",
                k.name, k.len
            );
            let loaded =
                tokio::time::timeout(self.timeouts.load, self.engine.load(&k.path, k.len)).await;
            match loaded {
                Err(_) => report.failed.push((
                    k.name.clone(),
                    format!("loading it took over {:?}", self.timeouts.load),
                )),
                Ok(Err(why)) => report
                    .failed
                    .push((k.name.clone(), format!("loading it: {why}"))),
                Ok(Ok(())) => match self.missing_of(tags).await {
                    Ok(still) if still.is_empty() => {
                        info!("runner image: loaded {}", k.name);
                        report.loaded.push(k.name.clone());
                    }
                    Ok(still) => report.failed.push((
                        k.name.clone(),
                        format!("loaded, but the engine still lacks {}", still.join(", ")),
                    )),
                    Err(why) => report
                        .failed
                        .push((k.name.clone(), format!("checking after the load: {why}"))),
                },
            }
        }

        for w in wanted {
            if provided.contains(&normalize(w)) {
                continue;
            }
            // An unanswered question is not a finding.
            if let Ok(false) = self.has(w).await {
                report.no_backup.push(w.clone());
            }
        }
        report
    }

    /// The tags in `tags` the engine lacks.
    async fn missing_of(&self, tags: &[String]) -> Result<Vec<String>, String> {
        let mut missing = Vec::new();
        for t in tags {
            if !self.has(t).await? {
                missing.push(t.clone());
            }
        }
        Ok(missing)
    }
}

/// `reference` with `:latest` when it names no tag and no digest.
fn normalize(reference: &str) -> String {
    let last = reference.rsplit('/').next().unwrap_or(reference);
    if last.contains(':') || last.contains('@') {
        reference.to_string()
    } else {
        format!("{reference}:latest")
    }
}

/// A tag safe to put in a request path.
fn tag_problem(tag: &str) -> Option<&'static str> {
    if tag.is_empty() || tag.len() > 255 {
        return Some("an image tag is empty or over 255 bytes");
    }
    if tag.contains("..")
        || !tag.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b':' | b'/' | b'@' | b'-')
        })
    {
        return Some("an image tag has characters outside A-Z a-z 0-9 . _ : / @ -");
    }
    None
}

// ---------------------------------------------------------------------------
// The directory and the archives
// ---------------------------------------------------------------------------

/// Every regular file in `dir` (at most [`MAX_TARBALLS`], by name), with its
/// tags. A missing directory is an empty list: a host with no kept image. Reads
/// only; nothing is created, changed or removed.
pub fn scan(dir: &Path) -> Vec<Kept> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| !n.starts_with('.'))
        .collect();
    names.sort();
    if names.len() > MAX_TARBALLS {
        warn!(
            "runner image: {} files in {}; only the first {MAX_TARBALLS} are looked at",
            names.len(),
            dir.display()
        );
        names.truncate(MAX_TARBALLS);
    }
    names
        .into_iter()
        .map(|name| {
            let path = dir.join(&name);
            match open_regular(&path) {
                Ok((mut f, len)) => Kept {
                    tags: repo_tags(&mut f, len),
                    name,
                    path,
                    len,
                },
                Err(why) => Kept {
                    name,
                    path,
                    len: 0,
                    tags: Err(why),
                },
            }
        })
        .collect()
}

/// `path` opened read-only, and checked to be a regular file and not a symlink
/// (the directory is root's alone to write, so the check before the open is not
/// racing anyone who could win).
pub fn open_regular(path: &Path) -> Result<(std::fs::File, u64), String> {
    let link = std::fs::symlink_metadata(path).map_err(|e| format!("reading it: {e}"))?;
    if !link.file_type().is_file() {
        return Err("it is not a regular file".into());
    }
    let mut open = std::fs::OpenOptions::new();
    open.read(true);
    // macOS's `O_NOFOLLOW` (as `wall/sys.rs` spells it): the engine runs only
    // there; elsewhere the check above and the handle's own are what hold.
    #[cfg(target_os = "macos")]
    std::os::unix::fs::OpenOptionsExt::custom_flags(&mut open, 0x0100);
    let f = open.open(path).map_err(|e| format!("opening it: {e}"))?;
    let meta = f.metadata().map_err(|e| format!("reading it: {e}"))?;
    if !meta.is_file() {
        return Err("it is not a regular file".into());
    }
    Ok((f, meta.len()))
}

fn octal(field: &[u8]) -> Option<u64> {
    let text: String = field
        .iter()
        .take_while(|&&b| b != 0)
        .map(|&b| b as char)
        .collect();
    let digits = text.trim();
    if digits.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(digits, 8).ok()
}

/// The `RepoTags` of every image in a `docker save` archive: walk the ustar
/// headers to the top-level `manifest.json` and read just that member.
pub fn repo_tags<R: Read + Seek>(r: &mut R, len: u64) -> Result<Vec<String>, String> {
    let mut header = [0u8; 512];
    let mut at: u64 = 0;
    for _ in 0..MAX_TAR_ENTRIES {
        if at + 512 > len {
            return Err("it ends before a manifest.json entry: not a `docker save` archive".into());
        }
        r.seek(SeekFrom::Start(at)).map_err(|e| e.to_string())?;
        r.read_exact(&mut header).map_err(|e| e.to_string())?;
        if header.iter().all(|&b| b == 0) {
            return Err("it has no manifest.json entry: not a `docker save` archive".into());
        }
        if &header[257..262] != b"ustar" {
            return Err(format!("the header at byte {at} is not a ustar header"));
        }
        let size = octal(&header[124..136])
            .ok_or_else(|| format!("the header at byte {at} has an unreadable size"))?;
        let take = |range: std::ops::Range<usize>| {
            let f = &header[range];
            let end = f.iter().position(|&b| b == 0).unwrap_or(f.len());
            String::from_utf8_lossy(&f[..end]).into_owned()
        };
        let mut name = take(0..100);
        let prefix = take(345..500);
        if !prefix.is_empty() {
            name = format!("{prefix}/{name}");
        }
        let regular = matches!(header[156], b'0' | 0);
        if regular && matches!(name.as_str(), "manifest.json" | "./manifest.json") {
            if size > MAX_MANIFEST_BYTES || at + 512 + size > len {
                return Err("its manifest.json is too large or runs past the end".into());
            }
            let mut buf = vec![0u8; size as usize];
            r.read_exact(&mut buf).map_err(|e| e.to_string())?;
            return parse_repo_tags(&buf);
        }
        let body = size
            .div_ceil(512)
            .checked_mul(512)
            .ok_or("an entry is too large")?;
        at = at
            .checked_add(512)
            .and_then(|a| a.checked_add(body))
            .ok_or("an entry is too large")?;
        if at > len {
            return Err(format!("an entry runs past the end of the {len}-byte file"));
        }
    }
    Err(format!(
        "no manifest.json in its first {MAX_TAR_ENTRIES} entries: not a `docker save` archive"
    ))
}

fn parse_repo_tags(manifest: &[u8]) -> Result<Vec<String>, String> {
    let v: serde_json::Value =
        serde_json::from_slice(manifest).map_err(|e| format!("its manifest.json: {e}"))?;
    let images = v
        .as_array()
        .ok_or("its manifest.json is not a list of images")?;
    let mut tags = Vec::new();
    for image in images {
        let Some(list) = image.get("RepoTags").and_then(|t| t.as_array()) else {
            continue;
        };
        for t in list {
            let t = t.as_str().ok_or("a RepoTags entry is not text")?;
            if let Some(p) = tag_problem(t) {
                return Err(p.into());
            }
            tags.push(t.to_string());
        }
    }
    Ok(tags)
}

// ---------------------------------------------------------------------------
// The VM's Docker socket
// ---------------------------------------------------------------------------

/// The Linux VM's Docker over its host socket
/// ([`crate::platform::vm_docker_socket`]): HTTP/1.0 straight to the Engine API,
/// so no `docker` binary is involved and the daemon answers without chunking.
pub struct SocketEngine {
    socket: PathBuf,
}

impl SocketEngine {
    pub fn new(socket: PathBuf) -> Self {
        Self { socket }
    }

    async fn call(
        &self,
        head: String,
        body: Option<(std::fs::File, u64)>,
    ) -> Result<(u16, Vec<u8>), String> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut s = tokio::net::UnixStream::connect(&self.socket)
            .await
            .map_err(|e| format!("connecting to {}: {e}", self.socket.display()))?;
        s.write_all(head.as_bytes())
            .await
            .map_err(|e| format!("sending the request: {e}"))?;
        if let Some((file, len)) = body {
            let mut file = tokio::fs::File::from_std(file).take(len);
            let sent = tokio::io::copy(&mut file, &mut s)
                .await
                .map_err(|e| format!("streaming the archive: {e}"))?;
            if sent != len {
                return Err(format!(
                    "the archive shrank while it was sent ({sent} of {len} bytes)"
                ));
            }
        }
        let mut raw = Vec::new();
        (&mut s)
            .take(MAX_RESPONSE_BYTES)
            .read_to_end(&mut raw)
            .await
            .map_err(|e| format!("reading the answer: {e}"))?;
        parse_response(&raw)
    }
}

fn parse_response(raw: &[u8]) -> Result<(u16, Vec<u8>), String> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("the answer has no header")?;
    let head = String::from_utf8_lossy(&raw[..split]).to_ascii_lowercase();
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .ok_or("the answer has no status line")?;
    let body = &raw[split + 4..];
    if head.contains("transfer-encoding: chunked") {
        return Ok((status, dechunk(body)?));
    }
    Ok((status, body.to_vec()))
}

fn dechunk(mut body: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    loop {
        let eol = body
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or("a chunked answer ends early")?;
        let size_text = String::from_utf8_lossy(&body[..eol]);
        let size = usize::from_str_radix(size_text.split(';').next().unwrap_or("").trim(), 16)
            .map_err(|_| "a chunk size is not hex")?;
        body = &body[eol + 2..];
        if size == 0 {
            return Ok(out);
        }
        // A hostile size must not overflow, and nothing larger than the response
        // cap can be in the body anyway.
        let end = size
            .checked_add(2)
            .filter(|&e| size as u64 <= MAX_RESPONSE_BYTES && body.len() >= e)
            .ok_or("a chunked answer ends early or has an absurd chunk size")?;
        out.extend_from_slice(&body[..size]);
        body = &body[end..];
    }
}

/// The first `error` a `/images/load` answer reports, if any.
fn load_error(body: &[u8]) -> Option<String> {
    String::from_utf8_lossy(body)
        .lines()
        .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
        .find_map(|v| {
            v.get("error")
                .and_then(|e| e.as_str())
                .map(|e| e.chars().take(300).collect())
        })
}

impl ImageEngine for SocketEngine {
    async fn has_image(&self, reference: &str) -> Result<bool, String> {
        if let Some(p) = tag_problem(reference) {
            return Err(p.into());
        }
        let head = format!(
            "GET /images/{reference}/json HTTP/1.0\r\nHost: docker\r\nConnection: close\r\n\r\n"
        );
        match self.call(head, None).await? {
            (200, _) => Ok(true),
            (404, _) => Ok(false),
            (code, body) => Err(format!(
                "the engine answered {code}: {}",
                String::from_utf8_lossy(&body)
                    .chars()
                    .take(200)
                    .collect::<String>()
            )),
        }
    }

    async fn load(&self, path: &Path, len: u64) -> Result<(), String> {
        let (file, actual) = open_regular(path)?;
        if actual != len {
            return Err("the archive changed size since it was read".into());
        }
        let head = format!(
            "POST /images/load?quiet=1 HTTP/1.0\r\nHost: docker\r\nConnection: close\r\n\
             Content-Type: application/x-tar\r\nContent-Length: {len}\r\n\r\n"
        );
        match self.call(head, Some((file, len))).await? {
            (200, body) => match load_error(&body) {
                Some(e) => Err(e),
                None => Ok(()),
            },
            (code, body) => Err(format!(
                "the engine answered {code}: {}",
                String::from_utf8_lossy(&body)
                    .chars()
                    .take(200)
                    .collect::<String>()
            )),
        }
    }
}

// ---------------------------------------------------------------------------
// Hooked in where the engine is judged ready
// ---------------------------------------------------------------------------

/// Where the loader reports what is wrong.
pub trait ImageSink: Send + Sync {
    fn runner_image(&self, problem: Option<String>);
}

impl ImageSink for Arc<crate::limiter::Limiter> {
    fn runner_image(&self, problem: Option<String>) {
        self.observe_runner_image(problem);
    }
}

/// The counter holds a plain integer, so a panic while it was held left nothing
/// half-done: take the lock anyway rather than panic every later status.
fn lock_ignoring_poison<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// An [`EngineSink`] that, when the engine reaches `InService`, first reports
/// [`EngineStatus::LoadingImage`], runs the loader, and only then passes
/// `InService` on. A status that arrives meanwhile (the engine went down again)
/// wins: the stale pass reports nothing more.
pub struct LoadingSink<R, E> {
    inner: R,
    loader: Arc<Loader<E>>,
    wanted: Arc<Vec<String>>,
    generation: Arc<Mutex<u64>>,
}

impl<R, E> LoadingSink<R, E> {
    pub fn new(inner: R, loader: Loader<E>, wanted: Vec<String>) -> Self {
        Self {
            inner,
            loader: Arc::new(loader),
            wanted: Arc::new(wanted),
            generation: Arc::new(Mutex::new(0)),
        }
    }
}

impl<R, E> EngineSink for LoadingSink<R, E>
where
    R: EngineSink + ImageSink + Clone + 'static,
    E: ImageEngine + 'static,
{
    fn engine(&self, status: &EngineStatus, held_mib: u64) {
        // The lock covers the bump and the report together, so a stale pass can
        // never report `InService` between a newer status's bump and its report.
        let mut g = lock_ignoring_poison(&self.generation);
        *g += 1;
        let mine = *g;
        if *status != EngineStatus::InService {
            self.inner.engine(status, held_mib);
            return;
        }
        self.inner.engine(&EngineStatus::LoadingImage, held_mib);
        drop(g);
        let (inner, loader, wanted, generation) = (
            self.inner.clone(),
            self.loader.clone(),
            self.wanted.clone(),
            self.generation.clone(),
        );
        tokio::spawn(async move {
            let current = || *lock_ignoring_poison(&generation) != mine;
            if current() {
                return;
            }
            let report = loader.reconcile_unless(&wanted, current).await;
            let g = lock_ignoring_poison(&generation);
            if *g != mine {
                return;
            }
            let problem = report.problem();
            match &problem {
                Some(p) => warn!("runner image: {p}"),
                None => info!(
                    "runner image: loaded {}, already present {}",
                    report.loaded.len(),
                    report.present.len()
                ),
            }
            inner.runner_image(problem);
            inner.engine(&EngineStatus::InService, held_mib);
            drop(g);
        });
    }
}

// ---------------------------------------------------------------------------
// Tests, against a fake engine
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::io::Write;
    use std::sync::Mutex;

    /// A one-entry ustar archive holding `manifest.json`, then padding.
    fn archive(tags: &[&str]) -> Vec<u8> {
        let manifest = serde_json::to_vec(&serde_json::json!([{
            "Config": "abc.json",
            "RepoTags": tags,
            "Layers": ["l/layer.tar"],
        }]))
        .unwrap();
        let mut header = [0u8; 512];
        header[..13].copy_from_slice(b"manifest.json");
        header[100..107].copy_from_slice(b"0000444");
        header[124..135].copy_from_slice(format!("{:011o}", manifest.len()).as_bytes());
        header[156] = b'0';
        header[257..262].copy_from_slice(b"ustar");
        header[148..156].fill(b' ');
        let sum: u64 = header.iter().map(|&b| u64::from(b)).sum();
        header[148..155].copy_from_slice(format!("{sum:06o}\0").as_bytes());
        header[155] = b' ';
        let mut out = header.to_vec();
        out.extend_from_slice(&manifest);
        out.resize(out.len().div_ceil(512) * 512 + 1024, 0);
        out
    }

    #[derive(Default)]
    struct World {
        have: BTreeSet<String>,
        /// What a load adds to the engine.
        loads_in: Vec<String>,
        loads: Vec<PathBuf>,
        running: usize,
        max_running: usize,
        fail_load: Option<String>,
        /// The load takes this long.
        load_for: Duration,
    }

    #[derive(Clone, Default)]
    struct Fake(Arc<Mutex<World>>);

    impl ImageEngine for Fake {
        async fn has_image(&self, reference: &str) -> Result<bool, String> {
            Ok(self.0.lock().unwrap().have.contains(&normalize(reference)))
        }
        async fn load(&self, path: &Path, _len: u64) -> Result<(), String> {
            let for_ = {
                let mut w = self.0.lock().unwrap();
                w.running += 1;
                w.max_running = w.max_running.max(w.running);
                w.loads.push(path.to_path_buf());
                w.load_for
            };
            tokio::time::sleep(for_).await;
            let mut w = self.0.lock().unwrap();
            w.running -= 1;
            if let Some(e) = w.fail_load.clone() {
                return Err(e);
            }
            let add = w.loads_in.clone();
            w.have.extend(add);
            Ok(())
        }
    }

    fn keep(dir: &Path, name: &str, tags: &[&str]) -> PathBuf {
        let p = dir.join(name);
        let mut f = std::fs::File::create(&p).unwrap();
        f.write_all(&archive(tags)).unwrap();
        p
    }

    fn loader(f: &Fake, dir: &Path) -> Loader<Fake> {
        Loader::new(f.clone(), dir.to_path_buf(), LoadTimeouts::default())
    }

    #[test]
    fn the_manifest_tags_are_read_without_the_layers() {
        let a = archive(&["owner/demo:1", "ghcr.io/owner/my-runner:latest"]);
        let tags = repo_tags(&mut std::io::Cursor::new(&a), a.len() as u64).unwrap();
        assert_eq!(tags, ["owner/demo:1", "ghcr.io/owner/my-runner:latest"]);
        let mut gz = a.clone();
        gz[0] = 0x1f;
        assert!(repo_tags(&mut std::io::Cursor::new(&gz), gz.len() as u64).is_err());
        let bad = archive(&["no spaces allowed"]);
        assert!(repo_tags(&mut std::io::Cursor::new(&bad), bad.len() as u64).is_err());
    }

    #[tokio::test]
    async fn it_loads_when_a_tag_is_missing() {
        let tmp = tempdir();
        let path = keep(&tmp, "runner.tar", &["ghcr.io/owner/my-runner:latest"]);
        let f = Fake::default();
        f.0.lock().unwrap().loads_in = vec!["ghcr.io/owner/my-runner:latest".into()];
        let r = loader(&f, &tmp)
            .reconcile(&["ghcr.io/owner/my-runner".into()])
            .await;
        assert_eq!(r.loaded, ["runner.tar"]);
        assert_eq!(f.0.lock().unwrap().loads, [path]);
        assert_eq!(r.problem(), None, "{r:?}");
    }

    #[tokio::test]
    async fn it_skips_when_every_tag_is_present() {
        let tmp = tempdir();
        keep(&tmp, "runner.tar", &["owner/a:1", "owner/a:2"]);
        let f = Fake::default();
        f.0.lock().unwrap().have = ["owner/a:1".to_string(), "owner/a:2".to_string()].into();
        let r = loader(&f, &tmp).reconcile(&[]).await;
        assert_eq!(r.present, ["runner.tar"]);
        assert!(r.loaded.is_empty());
        assert!(f.0.lock().unwrap().loads.is_empty());
        assert_eq!(r.problem(), None);
    }

    #[tokio::test]
    async fn a_partly_present_archive_is_loaded() {
        let tmp = tempdir();
        keep(&tmp, "runner.tar", &["owner/a:1", "owner/a:2"]);
        let f = Fake::default();
        {
            let mut w = f.0.lock().unwrap();
            w.have = ["owner/a:1".to_string()].into();
            w.loads_in = vec!["owner/a:2".into()];
        }
        let r = loader(&f, &tmp).reconcile(&[]).await;
        assert_eq!(r.loaded, ["runner.tar"]);
    }

    #[tokio::test]
    async fn it_reports_missing_with_no_backup() {
        let tmp = tempdir();
        let f = Fake::default();
        let r = loader(&f, &tmp)
            .reconcile(&["ghcr.io/owner/my-runner:latest".into()])
            .await;
        assert_eq!(r.no_backup, ["ghcr.io/owner/my-runner:latest"]);
        let p = r.problem().unwrap();
        assert!(p.contains("missing") && p.contains("no kept backup"), "{p}");
        // An archive for some other image is no backup for this one.
        keep(&tmp, "other.tar", &["owner/other:1"]);
        f.0.lock().unwrap().loads_in = vec!["owner/other:1".into()];
        let r = loader(&f, &tmp)
            .reconcile(&["ghcr.io/owner/my-runner:latest".into()])
            .await;
        assert_eq!(r.no_backup.len(), 1);
        // A wanted image the engine has is fine.
        f.0.lock()
            .unwrap()
            .have
            .insert("ghcr.io/owner/my-runner:latest".into());
        let r = loader(&f, &tmp)
            .reconcile(&["ghcr.io/owner/my-runner:latest".into()])
            .await;
        assert!(r.no_backup.is_empty());
    }

    #[tokio::test]
    async fn a_failed_load_is_reported_and_not_retried_within_the_pass() {
        let tmp = tempdir();
        keep(&tmp, "runner.tar", &["owner/a:1"]);
        let f = Fake::default();
        f.0.lock().unwrap().fail_load = Some("no space left".into());
        let r = loader(&f, &tmp).reconcile(&["owner/a:1".into()]).await;
        assert_eq!(r.failed.len(), 1);
        assert!(r.problem().unwrap().contains("no space left"));
        assert_eq!(f.0.lock().unwrap().loads.len(), 1);
    }

    #[tokio::test]
    async fn it_never_touches_the_tarball() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempdir();
        let path = keep(&tmp, "runner.tar", &["owner/a:1"]);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o440)).unwrap();
        let before = (
            std::fs::read(&path).unwrap(),
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            std::fs::metadata(&path).unwrap().permissions().mode(),
        );
        let f = Fake::default();
        f.0.lock().unwrap().loads_in = vec!["owner/a:1".into()];
        loader(&f, &tmp).reconcile(&["owner/a:1".into()]).await;
        let again = Fake::default();
        loader(&again, &tmp).reconcile(&[]).await;
        let after = (
            std::fs::read(&path).unwrap(),
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            std::fs::metadata(&path).unwrap().permissions().mode(),
        );
        assert_eq!(before, after);
        let names: Vec<_> = std::fs::read_dir(&tmp)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["runner.tar"], "nothing was created or removed");
    }

    #[tokio::test(start_paused = true)]
    async fn only_one_load_runs_at_a_time() {
        let tmp = tempdir();
        keep(&tmp, "a.tar", &["owner/a:1"]);
        keep(&tmp, "b.tar", &["owner/b:1"]);
        let f = Fake::default();
        {
            let mut w = f.0.lock().unwrap();
            w.loads_in = vec!["owner/a:1".into(), "owner/b:1".into()];
            w.load_for = Duration::from_secs(10);
        }
        let l = Arc::new(loader(&f, &tmp));
        let (l1, l2) = (l.clone(), l.clone());
        let (a, b) = tokio::join!(
            tokio::spawn(async move { l1.reconcile(&[]).await }),
            tokio::spawn(async move { l2.reconcile(&[]).await })
        );
        let (a, b) = (a.unwrap(), b.unwrap());
        // The second pass waited, then found the first's load had done it all
        // (the fake adds every tag at once): without the lock both would load.
        assert_eq!(a.loaded.len() + b.loaded.len(), 1, "{a:?} {b:?}");
        assert_eq!(f.0.lock().unwrap().loads.len(), 1);
        assert_eq!(f.0.lock().unwrap().max_running, 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_load_that_hangs_times_out() {
        let tmp = tempdir();
        keep(&tmp, "a.tar", &["owner/a:1"]);
        let f = Fake::default();
        f.0.lock().unwrap().load_for = Duration::from_secs(86_400);
        let r = loader(&f, &tmp).reconcile(&[]).await;
        assert!(r.failed[0].1.contains("took over"), "{r:?}");
    }

    #[tokio::test]
    async fn an_unreadable_archive_is_reported_not_loaded() {
        let tmp = tempdir();
        std::fs::write(tmp.join("junk.tar"), vec![7u8; 2048]).unwrap();
        std::os::unix::fs::symlink("/etc/hosts", tmp.join("link.tar")).unwrap();
        let f = Fake::default();
        let r = loader(&f, &tmp).reconcile(&[]).await;
        assert_eq!(r.failed.len(), 1, "a symlink is not looked at: {r:?}");
        assert!(f.0.lock().unwrap().loads.is_empty());
    }

    #[test]
    fn a_hostile_chunk_size_is_refused_not_overflowed() {
        let huge = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nffffffffffffffff\r\nx";
        assert!(parse_response(huge).is_err());
        let big = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n7fffffff\r\nx";
        assert!(parse_response(big).is_err());
    }

    #[test]
    fn answers_are_parsed() {
        let plain = b"HTTP/1.0 404 Not Found\r\nContent-Type: x\r\n\r\n{\"message\":\"no\"}";
        assert_eq!(parse_response(plain).unwrap().0, 404);
        let chunked =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked).unwrap(), (200, b"hello".to_vec()));
        assert_eq!(
            load_error(b"{\"stream\":\"x\"}\n{\"error\":\"boom\"}\n").as_deref(),
            Some("boom")
        );
        assert_eq!(load_error(b"{\"stream\":\"Loaded image: a:1\"}\n"), None);
    }

    /// A stub Docker on a Unix socket: the real client against real bytes.
    #[tokio::test]
    async fn the_socket_client_speaks_to_a_docker_shaped_server() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let tmp = tempdir();
        // Short: a unix socket path must fit `SUN_LEN`, which a CI work
        // directory's temp dir does not.
        let sock = PathBuf::from(format!("/tmp/fbr-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock);
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen2 = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut c, _)) = listener.accept().await else {
                    return;
                };
                let mut buf = Vec::new();
                let mut chunk = [0u8; 4096];
                // Read until the header ends, then the declared body.
                let (line, body_len) = loop {
                    let n = c.read(&mut chunk).await.unwrap();
                    buf.extend_from_slice(&chunk[..n]);
                    if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..i]).to_string();
                        let len = head
                            .lines()
                            .find_map(|l| l.strip_prefix("Content-Length: "))
                            .and_then(|v| v.parse::<usize>().ok())
                            .unwrap_or(0);
                        while buf.len() < i + 4 + len {
                            let n = c.read(&mut chunk).await.unwrap();
                            buf.extend_from_slice(&chunk[..n]);
                        }
                        break (head.lines().next().unwrap().to_string(), len);
                    }
                };
                seen2
                    .lock()
                    .unwrap()
                    .push(format!("{line} body={body_len}"));
                let reply: &[u8] = if line.contains("/images/owner/known:1/json") {
                    b"HTTP/1.0 200 OK\r\n\r\n{}"
                } else if line.contains("/images/load") {
                    b"HTTP/1.0 200 OK\r\n\r\n{\"stream\":\"Loaded image: owner/known:1\"}\n"
                } else {
                    b"HTTP/1.0 404 Not Found\r\n\r\n{\"message\":\"no such image\"}"
                };
                c.write_all(reply).await.unwrap();
            }
        });
        let e = SocketEngine::new(sock.clone());
        assert!(e.has_image("owner/known:1").await.unwrap());
        assert!(!e.has_image("owner/other:1").await.unwrap());
        assert!(e.has_image("bad tag").await.is_err());
        let p = keep(&tmp, "r.tar", &["owner/known:1"]);
        let len = std::fs::metadata(&p).unwrap().len();
        e.load(&p, len).await.unwrap();
        let seen = seen.lock().unwrap().clone();
        assert!(
            seen.contains(&format!("POST /images/load?quiet=1 HTTP/1.0 body={len}")),
            "{seen:?}"
        );
        let _ = std::fs::remove_file(&sock);
    }

    #[derive(Clone, Default)]
    struct Spy(Arc<Mutex<Vec<String>>>);
    impl EngineSink for Spy {
        fn engine(&self, status: &EngineStatus, _held: u64) {
            self.0.lock().unwrap().push(format!("{status:?}"));
        }
    }
    impl ImageSink for Spy {
        fn runner_image(&self, problem: Option<String>) {
            self.0
                .lock()
                .unwrap()
                .push(format!("problem={}", problem.is_some()));
        }
    }

    #[tokio::test]
    async fn in_service_waits_for_the_load() {
        let tmp = tempdir();
        keep(&tmp, "r.tar", &["owner/a:1"]);
        let f = Fake::default();
        f.0.lock().unwrap().loads_in = vec!["owner/a:1".into()];
        let spy = Spy::default();
        let sink = LoadingSink::new(spy.clone(), loader(&f, &tmp), vec!["owner/a:1".into()]);
        sink.engine(&EngineStatus::InService, 1);
        assert_eq!(
            *spy.0.lock().unwrap(),
            ["LoadingImage"],
            "Docker is not served yet"
        );
        // The scan runs on a blocking thread, so wait in real time, not in yields.
        for _ in 0..500 {
            if spy.0.lock().unwrap().iter().any(|l| l == "InService") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(
            *spy.0.lock().unwrap(),
            ["LoadingImage", "problem=false", "InService"]
        );
        // A restart of the engine loads again, and a status in between wins.
        sink.engine(&EngineStatus::InService, 1);
        sink.engine(&EngineStatus::Down("it stopped".into()), 0);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let log = spy.0.lock().unwrap().clone();
        assert_eq!(log.last().unwrap(), "Down(\"it stopped\")", "{log:?}");
    }

    #[tokio::test]
    async fn a_pass_made_stale_while_queued_does_nothing() {
        let tmp = tempdir();
        keep(&tmp, "r.tar", &["owner/a:1"]);
        let f = Fake::default();
        f.0.lock().unwrap().loads_in = vec!["owner/a:1".into()];
        let l = loader(&f, &tmp);
        let r = l.reconcile_unless(&[], || true).await;
        assert_eq!(r, Report::default());
        assert!(f.0.lock().unwrap().loads.is_empty(), "a stale pass loaded");
        let r = l.reconcile_unless(&[], || false).await;
        assert_eq!(r.loaded, ["r.tar"]);
    }

    #[test]
    fn a_poisoned_generation_still_locks() {
        let m = Arc::new(Mutex::new(0u64));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("poison it");
        })
        .join();
        assert!(m.lock().is_err());
        *lock_ignoring_poison(&m) += 1;
        assert_eq!(*lock_ignoring_poison(&m), 1);
    }

    fn tempdir() -> PathBuf {
        use std::sync::atomic::AtomicUsize;
        static N: AtomicUsize = AtomicUsize::new(0);
        let p = std::env::temp_dir().join(format!(
            "faber-runner-image-{}-{}",
            std::process::id(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }
}
