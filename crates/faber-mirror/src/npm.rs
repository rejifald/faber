//! The npm adapter: a pull-through of the public registry (ADR-0047 §5).
//!
//! A job's npm, pnpm and bun read `NPM_CONFIG_REGISTRY`, yarn berry reads
//! `YARN_NPM_REGISTRY_SERVER`; faber points both at
//! `http://faber-mirror:7470/npm/`, so the default registry's requests arrive
//! here. Scoped registries a project configures itself are untouched: they never
//! reach this adapter.
//!
//! - **Tarballs** (`GET /<name>/-/<file>.tgz`, scoped too) are kept in the
//!   shared store ([`crate::store`]) by the hash their packument publishes in
//!   `dist.integrity` (sha512; the legacy sha1 `shasum` only when there is no
//!   sha512). A fill hashes what it downloads and keeps it only on a match; on a
//!   mismatch the job gets `502` and nothing is served or kept. npm never
//!   republishes a version, so the path-to-hash a packument gives is recorded
//!   (`npm/tarballs/`) and a later hit needs no packument at all: an `npm ci`
//!   with a lockfile asks for tarballs only.
//! - **Packuments** (`GET /<name>`) are name-addressed: a stored one decides
//!   which tarball a later install resolves. Only faber fetches one, and only
//!   from upstream; a job's request can cause a fetch, never choose its content.
//!   The full and the abbreviated (`Accept: application/vnd.npm.install-v1+json`)
//!   forms are cached apart, in memory, for [`PACKUMENT_TTL`], and served stale
//!   when upstream errors. Each `dist.tarball` on the registry is rewritten to
//!   point here.
//! - **Everything else passes through, never cached**: any request carrying
//!   `Authorization`, any method but `GET` (a publish is a `PUT`), and every
//!   other endpoint (`/-/whoami`, `/-/v1/search`, the audit endpoints, a
//!   version's own manifest). It is forwarded to the registry as it came, and
//!   the answer back as it went, redirects included.
//!
//! **Fill off, or passing through.** With `fill = "off"` nothing new is kept:
//! stored tarballs are served, a miss is fetched and streamed without being
//! kept, and packuments are fetched every time. While the host is short of disk
//! or the store cannot be written, tarballs are likewise streamed uncached; a
//! streamed tarball whose hash is known is still hashed on the way, its last
//! chunk held back until it matched, and cut off short of its length on a
//! mismatch, so the job never gets a complete wrong file.
//!
//! **Who is asking** is the address the request arrived on ([`crate::Peer`]),
//! which the daemon's sync names per container. It attributes the counters and
//! the mirror's own failures to a job; it never decides what a request may read,
//! because everything here is public.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::rejection::ExtensionRejection;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::Router;
use base64::Engine as _;
use futures_util::StreamExt;
use serde::Deserialize;
use sha2::{Digest, Sha256, Sha512};
use tokio::io::AsyncWriteExt;

use crate::control::{Control, Grant};
use crate::egress::Egress;
use crate::lfs::{href_base, serve_file, ByteStream, UpstreamError};
use crate::store::{valid_key, NotStored, Store};
use crate::{Fill, Peer};

/// How long a packument is served from the cache before it is fetched again
/// (ADR-0047 §5). The registry's own `Cache-Control` for packuments is
/// `max-age=300`, so a client of the registry sees the same staleness.
pub const PACKUMENT_TTL: Duration = Duration::from_secs(300);

/// The media type of the abbreviated packument npm, pnpm and yarn install from.
pub const ABBREVIATED: &str = "application/vnd.npm.install-v1+json";

/// The largest packument read whole: rewritten, cached, searched for a
/// tarball's hash. A larger one (the full packuments of a few very prolific
/// packages; abbreviated ones are far smaller) is streamed through as the
/// registry sent it, its tarball links not rewritten: npm's own
/// `replace-registry-host` still sends those to the mirror. The service runs in
/// 256 MiB, so a request holds at most this, and its rewritten copy.
const PACKUMENT_MAX: usize = 16 << 20;

/// The cache's whole size, in memory: a quarter of the service's.
const CACHE_MAX: usize = 64 << 20;

/// The largest tarball the mirror stores. The registry refuses packages much
/// larger; a bigger one is streamed through.
pub const MAX_TARBALL_BYTES: u64 = 1 << 30;

/// Headers that describe one hop, never forwarded either way. `host` is the
/// mirror's, and `content-length` is set again from the body.
const HOP_BY_HOP: &[&str] = &[
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
    "host",
];

// --- upstream -----------------------------------------------------------------------

/// One request to the registry.
pub struct Ask {
    pub method: Method,
    /// Path and query, starting with `/`, appended to the registry's origin.
    pub path: String,
    pub headers: HeaderMap,
    pub body: Body,
    /// Follow redirects, each re-checked against [`Egress`]: only for the
    /// mirror's own anonymous `GET`s. A forwarded request's redirect goes back
    /// to the job as it came, so its credentials never follow one.
    pub follow: bool,
}

/// The registry's answer.
pub struct Reply {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: ByteStream,
}

/// The registry, as the adapter uses it. Behind a trait so tests use an
/// in-house fake and never reach the real one.
pub trait Upstream: Send + Sync + 'static {
    fn send(&self, ask: Ask) -> impl Future<Output = Result<Reply, UpstreamError>> + Send;
}

/// The real upstream: [`crate::NPMJS`], through [`Egress::npmjs`]: https to the
/// registry's host only, public addresses only, at most
/// [`crate::egress::MAX_REDIRECTS`] redirects, each re-checked.
pub struct Registry {
    base: String,
    client: reqwest::Client,
    egress: Egress,
}

impl Registry {
    pub fn new(base: &str, egress: Egress) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .user_agent(concat!("faber-mirror/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(60))
            // Redirects are followed by hand, so each hop is checked.
            .redirect(reqwest::redirect::Policy::none())
            .dns_resolver(egress.resolver())
            .build()
            .map_err(|e| format!("building the HTTP client: {e}"))?;
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            client,
            egress,
        })
    }
}

fn bad_gateway(message: String) -> UpstreamError {
    UpstreamError::Upstream {
        status: 502,
        message,
    }
}

impl Upstream for Registry {
    async fn send(&self, ask: Ask) -> Result<Reply, UpstreamError> {
        let refuse = |why: String| bad_gateway(format!("not fetching from the registry: {why}"));
        let mut url = self
            .egress
            .check(&format!("{}{}", self.base, ask.path))
            .map_err(refuse)?;
        let body = reqwest::Body::wrap_stream(ask.body.into_data_stream());
        let mut res = self
            .client
            .request(ask.method.clone(), url.clone())
            .headers(ask.headers.clone())
            .body(body)
            .send()
            .await
            .map_err(|e| {
                bad_gateway(format!("the registry did not answer: {}", e.without_url()))
            })?;
        let mut hops = 0;
        while ask.follow && res.status().is_redirection() {
            if hops == crate::egress::MAX_REDIRECTS {
                return Err(refuse(format!(
                    "more than {} redirects",
                    crate::egress::MAX_REDIRECTS
                )));
            }
            hops += 1;
            let next = res
                .headers()
                .get(header::LOCATION)
                .and_then(|l| l.to_str().ok())
                .and_then(|l| url.join(l).ok())
                .ok_or_else(|| refuse("a redirect with no usable Location".into()))?;
            url = self.egress.check(next.as_str()).map_err(refuse)?;
            res = self
                .client
                .get(url.clone())
                .headers(ask.headers.clone())
                .send()
                .await
                .map_err(|e| {
                    bad_gateway(format!("the registry did not answer: {}", e.without_url()))
                })?;
        }
        let status = res.status();
        let headers = res.headers().clone();
        let body: ByteStream = Box::pin(futures_util::stream::unfold(res, |mut res| async {
            match res.chunk().await {
                Ok(Some(b)) => Some((Ok(b), res)),
                Ok(None) => None,
                Err(e) => Some((
                    Err(format!("reading from the registry: {}", e.without_url())),
                    res,
                )),
            }
        }));
        Ok(Reply {
            status,
            headers,
            body,
        })
    }
}

// --- what a request names -----------------------------------------------------------

/// What a `GET` under `/npm/` asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// A packument: `/<name>`, `/@scope/<name>` or `/@scope%2f<name>`.
    Packument { name: String },
    /// A tarball: `/<name>/-/<file>.tgz`, scoped too. `path` is the normalized
    /// path, the one the registry serves it at and its packuments name.
    Tarball { name: String, path: String },
    /// Anything else: passed through.
    Other,
}

fn name_part(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 214
        && !s.starts_with('.')
        && !s.starts_with('-')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'~'))
}

fn scope_part(s: &str) -> bool {
    s.strip_prefix('@').is_some_and(name_part)
}

fn file_part(s: &str) -> bool {
    s.len() <= 300
        && s.ends_with(".tgz")
        && !s.starts_with('.')
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'~' | b'+'))
}

/// What `raw` (the path under `/npm`, still percent-encoded) asks for. The
/// encodings clients use for a scoped name (`%2f`, `%2F`, `%40`) are read;
/// anything else encoded is not a name, and passes through.
pub fn classify(raw: &str) -> Target {
    let decoded = raw
        .replace("%2f", "/")
        .replace("%2F", "/")
        .replace("%40", "@");
    if decoded.contains('%') {
        return Target::Other;
    }
    let Some(rest) = decoded.strip_prefix('/') else {
        return Target::Other;
    };
    let parts: Vec<&str> = rest.split('/').collect();
    match parts.as_slice() {
        [n] if name_part(n) => Target::Packument {
            name: n.to_string(),
        },
        [s, n] if scope_part(s) && name_part(n) => Target::Packument {
            name: format!("{s}/{n}"),
        },
        [n, "-", f] if name_part(n) && file_part(f) => Target::Tarball {
            name: n.to_string(),
            path: format!("/{n}/-/{f}"),
        },
        [s, n, "-", f] if scope_part(s) && name_part(n) && file_part(f) => Target::Tarball {
            name: format!("{s}/{n}"),
            path: format!("/{s}/{n}/-/{f}"),
        },
        _ => Target::Other,
    }
}

/// Where the registry serves `name`'s packument: a scoped name's slash encoded,
/// as npm itself asks.
fn packument_path(name: &str) -> String {
    format!("/{}", name.replace('/', "%2f"))
}

/// Which packument form a request asks for, by its `Accept`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Form {
    Full,
    Abbreviated,
}

impl Form {
    fn of(headers: &HeaderMap) -> Self {
        let abbreviated = headers
            .get_all(header::ACCEPT)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| v.contains(ABBREVIATED));
        if abbreviated {
            Self::Abbreviated
        } else {
            Self::Full
        }
    }

    fn accept(self) -> &'static str {
        match self {
            Self::Full => "application/json",
            Self::Abbreviated => ABBREVIATED,
        }
    }
}

// --- integrity ------------------------------------------------------------------------

/// The hash a tarball must have, from its packument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Integrity {
    /// sha512, or sha1 for a package published before npm wrote `integrity`.
    pub sha512: bool,
    /// The lowercase hex of the hash: its key in the store.
    pub hex: String,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

impl Integrity {
    /// From a version's `dist`: the sha512 of its `integrity` (an SRI string,
    /// which may list several hashes), else its sha1, from `shasum` or from a
    /// `sha1-` entry of `integrity`. `None` when it has neither.
    pub fn from_dist(integrity: Option<&str>, shasum: Option<&str>) -> Option<Self> {
        let sri = integrity.unwrap_or_default();
        let decoded = |prefix: &str, len: usize| {
            sri.split_whitespace().find_map(|t| {
                let b64 = t.strip_prefix(prefix)?;
                let b64 = b64.split('?').next().unwrap_or_default();
                let raw = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
                (raw.len() == len).then(|| hex(&raw))
            })
        };
        if let Some(hex) = decoded("sha512-", 64) {
            return Some(Self { sha512: true, hex });
        }
        let sha1 = shasum
            .map(|s| s.trim().to_ascii_lowercase())
            .filter(|s| s.len() == 40 && valid_key(s))
            .or_else(|| decoded("sha1-", 20));
        sha1.map(|hex| Self { sha512: false, hex })
    }

    fn hasher(&self) -> Hasher {
        if self.sha512 {
            Hasher::Sha512(Sha512::new())
        } else {
            Hasher::Sha1(sha1::Sha1::new())
        }
    }
}

enum Hasher {
    Sha512(Sha512),
    Sha1(sha1::Sha1),
}

impl Hasher {
    fn update(&mut self, b: &[u8]) {
        match self {
            Self::Sha512(h) => h.update(b),
            Self::Sha1(h) => h.update(b),
        }
    }

    fn finish(self) -> String {
        match self {
            Self::Sha512(h) => hex(&h.finalize()),
            Self::Sha1(h) => hex(&h.finalize()),
        }
    }
}

#[derive(Deserialize)]
struct WirePackument {
    #[serde(default)]
    versions: HashMap<String, WireVersion>,
}

#[derive(Deserialize)]
struct WireVersion {
    #[serde(default)]
    dist: Option<WireDist>,
}

#[derive(Deserialize)]
struct WireDist {
    #[serde(default)]
    tarball: Option<String>,
    #[serde(default)]
    integrity: Option<String>,
    #[serde(default)]
    shasum: Option<String>,
}

/// The hash of the tarball at `path` (a normalized tarball path) in the
/// packument `body`: the version whose `dist.tarball`, on the registry, is that
/// path. `None` when no version names it, or it has no hash.
pub fn integrity_for(body: &[u8], path: &str) -> Option<Integrity> {
    let p: WirePackument = serde_json::from_slice(body).ok()?;
    p.versions.into_values().find_map(|v| {
        let d = v.dist?;
        let at = d.tarball?;
        let at = at.strip_prefix(crate::NPMJS)?;
        let Target::Tarball { path: named, .. } = classify(at) else {
            return None;
        };
        (named == path)
            .then(|| Integrity::from_dist(d.integrity.as_deref(), d.shasum.as_deref()))
            .flatten()
    })
}

/// `body` with every `"tarball"` value on the registry pointed at `base`
/// instead (`http://faber-mirror:7470/npm`). Only object keys are followed by
/// a `:` in JSON (a quote inside a string is escaped), so only `dist.tarball`
/// values are touched, whatever the packument's spacing.
pub fn rewrite_tarballs(body: &[u8], base: &str) -> Vec<u8> {
    const KEY: &[u8] = b"\"tarball\"";
    let from = crate::NPMJS.as_bytes();
    let mut out = Vec::with_capacity(body.len() + body.len() / 50);
    let mut i = 0;
    while let Some(at) = find(&body[i..], KEY) {
        let key_end = i + at + KEY.len();
        let mut j = key_end;
        while j < body.len() && body[j].is_ascii_whitespace() {
            j += 1;
        }
        if body.get(j) != Some(&b':') {
            out.extend_from_slice(&body[i..key_end]);
            i = key_end;
            continue;
        }
        j += 1;
        while j < body.len() && body[j].is_ascii_whitespace() {
            j += 1;
        }
        if body.get(j) == Some(&b'"') && body[j + 1..].starts_with(from) {
            out.extend_from_slice(&body[i..=j]);
            out.extend_from_slice(base.as_bytes());
            i = j + 1 + from.len();
        } else {
            out.extend_from_slice(&body[i..j]);
            i = j;
        }
    }
    out.extend_from_slice(&body[i..]);
    out
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

// --- the adapter -----------------------------------------------------------------------

struct Cached {
    body: Bytes,
    content_type: HeaderValue,
    at: Instant,
}

/// Packuments in memory, by form and name, oldest evicted first past
/// [`CACHE_MAX`].
#[derive(Default)]
struct Packuments {
    entries: HashMap<(Form, String), Cached>,
    bytes: usize,
}

impl Packuments {
    fn put(&mut self, key: (Form, String), c: Cached) {
        if c.body.len() > PACKUMENT_MAX {
            return;
        }
        if let Some(old) = self.entries.remove(&key) {
            self.bytes -= old.body.len();
        }
        while self.bytes + c.body.len() > CACHE_MAX {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, v)| v.at)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(gone) = self.entries.remove(&oldest) {
                self.bytes -= gone.body.len();
            }
        }
        self.bytes += c.body.len();
        self.entries.insert(key, c);
    }
}

/// The npm adapter's state.
pub struct Npm<U> {
    control: Arc<Control>,
    store: Arc<Store>,
    upstream: U,
    ttl: Duration,
    packuments: Mutex<Packuments>,
    /// One fill per key at a time.
    fills: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Which tarball path has which hash: one file per path, named by the
    /// sha256 of the path, holding the store key.
    paths: PathBuf,
    /// Records written since the last prune.
    recorded_count: AtomicU64,
}

/// How many records are written between two prunes ([`Npm::prune`]).
const PRUNE_EVERY: u64 = 1024;

impl<U: Upstream> Npm<U> {
    /// The adapter, keeping its path records under `<cache>/npm/`.
    pub fn new(control: Arc<Control>, store: Arc<Store>, cache: &Path, upstream: U) -> Arc<Self> {
        Self::with_ttl(control, store, cache, upstream, PACKUMENT_TTL)
    }

    /// As [`Npm::new`], with another packument lifetime (a test's).
    pub fn with_ttl(
        control: Arc<Control>,
        store: Arc<Store>,
        cache: &Path,
        upstream: U,
        ttl: Duration,
    ) -> Arc<Self> {
        let paths = cache.join("npm").join("tarballs");
        if let Err(e) = std::fs::create_dir_all(&paths) {
            tracing::warn!("npm: making {}: {e}", paths.display());
        }
        let npm = Arc::new(Self {
            control,
            store,
            upstream,
            ttl,
            packuments: Mutex::new(Packuments::default()),
            fills: Mutex::new(HashMap::new()),
            paths,
            recorded_count: AtomicU64::new(0),
        });
        let gone = npm.prune();
        if gone > 0 {
            tracing::info!("npm: pruned {gone} records of tarballs no longer stored");
        }
        npm
    }

    /// Why a tarball miss would not be kept right now, if it would not.
    fn passing_through(&self) -> Option<&'static str> {
        if self.control.npm() == Some(Fill::Off) {
            Some("fill is off")
        } else if self.control.low_disk() {
            Some("host disk low")
        } else if self.store.broken().is_some() {
            Some("store not writable")
        } else {
            None
        }
    }

    fn fill_lock(&self, key: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut f = self.fills.lock().unwrap();
        f.retain(|_, l| Arc::strong_count(l) > 1);
        f.entry(key.to_string()).or_default().clone()
    }

    fn path_file(&self, path: &str) -> PathBuf {
        self.paths.join(hex(&Sha256::digest(path.as_bytes())))
    }

    /// The store key recorded for the tarball at `path`.
    async fn recorded(&self, path: &str) -> Option<String> {
        let text = tokio::fs::read_to_string(self.path_file(path)).await.ok()?;
        let key = text.trim();
        valid_key(key).then(|| key.to_string())
    }

    /// Remove the records whose tarball is no longer stored, so the records
    /// follow the store's cap rather than grow beside it. Run when the adapter
    /// starts and after every [`PRUNE_EVERY`] records written. Returns how many
    /// went.
    pub fn prune(&self) -> usize {
        let Ok(rd) = std::fs::read_dir(&self.paths) else {
            return 0;
        };
        let mut gone = 0;
        for e in rd.flatten() {
            let path = e.path();
            let stored = std::fs::read_to_string(&path)
                .ok()
                .map(|t| t.trim().to_string())
                .filter(|k| valid_key(k))
                .is_some_and(|k| self.store.has(&k).is_some());
            if !stored && std::fs::remove_file(&path).is_ok() {
                gone += 1;
            }
        }
        gone
    }

    /// Record that the tarball at `path` hashes to `key`. Best-effort: without
    /// it, the next request for it asks the packument again.
    async fn record(self: &Arc<Self>, path: &str, key: &str) {
        let file = self.path_file(path);
        let tmp = file.with_extension(format!("tmp{}", std::process::id()));
        let wrote = tokio::fs::write(&tmp, key.as_bytes()).await;
        if let Err(e) = wrote.and(tokio::fs::rename(&tmp, &file).await) {
            tracing::debug!("npm: recording {path}: {e}");
            let _ = tokio::fs::remove_file(&tmp).await;
        }
        if self.recorded_count.fetch_add(1, Ordering::Relaxed) % PRUNE_EVERY == PRUNE_EVERY - 1 {
            let this = self.clone();
            tokio::task::spawn_blocking(move || this.prune());
        }
    }
}

/// The adapter's name and routes.
pub struct Adapter<U>(pub Arc<Npm<U>>);

impl<U: Upstream> crate::Adapter for Adapter<U> {
    fn name(&self) -> &'static str {
        "npm"
    }

    fn routes(&self) -> Router {
        Router::new()
            .route("/", any(handle::<U>))
            .route("/{*rest}", any(handle::<U>))
            .with_state(self.0.clone())
    }
}

/// An error in the registry's own shape.
fn npm_error(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({ "error": message }).to_string();
    (status, [(header::CONTENT_TYPE, "application/json")], body).into_response()
}

/// Who a request is counted against.
struct Asker {
    grant: Option<Grant>,
}

impl Asker {
    fn repo(&self) -> &str {
        self.grant
            .as_ref()
            .map_or(crate::UNATTRIBUTED, |g| g.repo.as_str())
    }

    /// A fault of the mirror's own, counted against the container when it is
    /// known (ADR-0047 §7).
    fn fail(&self, control: &Control, why: &str) {
        if let Some(g) = &self.grant {
            control.fail(&g.id, why);
        }
    }
}

async fn handle<U: Upstream>(
    State(npm): State<Arc<Npm<U>>>,
    // axum 0.8 gives `ConnectInfo` no `Option` form; its rejection is the
    // "none" case: a router served without connect info sees no peer.
    peer: Result<ConnectInfo<Peer>, ExtensionRejection>,
    req: Request,
) -> Response {
    if npm.control.npm().is_none() {
        return npm_error(StatusCode::NOT_FOUND, "npm is not served by this mirror");
    }
    let asker = Asker {
        grant: peer
            .ok()
            .and_then(|ConnectInfo(p)| p.local)
            .and_then(|a| npm.control.by_addr(a.ip())),
    };
    let plain_get = req.method() == Method::GET
        && !req.headers().contains_key(header::AUTHORIZATION)
        && req.uri().query().is_none();
    let target = if plain_get {
        classify(req.uri().path())
    } else {
        Target::Other
    };
    match target {
        Target::Packument { name } => {
            let form = Form::of(req.headers());
            packument(&npm, &asker, &name, form, req.headers()).await
        }
        Target::Tarball { name, path } => tarball(&npm, &asker, &name, &path).await,
        Target::Other => forward(&npm, &asker, req).await,
    }
}

/// `headers` less the hop-by-hop ones.
fn end_to_end(headers: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (k, v) in headers {
        if !HOP_BY_HOP.contains(&k.as_str()) {
            out.append(k.clone(), v.clone());
        }
    }
    out
}

/// A stream that counts what it passes to `repo` as fetched upstream.
fn counted(
    body: ByteStream,
    control: Arc<Control>,
    repo: String,
) -> impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> {
    body.map(move |chunk| {
        let b = chunk.map_err(std::io::Error::other)?;
        let n = b.len() as u64;
        control.count_npm(&repo, |c| c.bytes_from_upstream += n);
        Ok(b)
    })
}

/// Forward the request to the registry as it came, and its answer back as it
/// went: never cached, never redirected by the mirror.
async fn forward<U: Upstream>(npm: &Npm<U>, asker: &Asker, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let path = parts
        .uri
        .path_and_query()
        .map_or("/", |p| p.as_str())
        .to_string();
    let ask = Ask {
        method: parts.method,
        path,
        headers: end_to_end(&parts.headers),
        body,
        follow: false,
    };
    let reply = match npm.upstream.send(ask).await {
        Ok(r) => r,
        Err(UpstreamError::Upstream { status, message }) => {
            let s = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            return npm_error(s, &message);
        }
        Err(UpstreamError::Local(message)) => {
            asker.fail(&npm.control, &message);
            return npm_error(StatusCode::INTERNAL_SERVER_ERROR, &message);
        }
    };
    npm.control.count_npm(asker.repo(), |c| {
        c.misses += 1;
        c.passed_through += 1;
    });
    let body = counted(reply.body, npm.control.clone(), asker.repo().to_string());
    let mut res = Body::from_stream(body).into_response();
    *res.status_mut() = reply.status;
    *res.headers_mut() = end_to_end(&reply.headers);
    res
}

/// A packument read whole, or one too large to be, already streaming.
enum Fetched {
    Whole {
        status: StatusCode,
        body: Bytes,
        content_type: HeaderValue,
    },
    Streaming(Reply),
}

/// Fetch `name`'s packument in `form`, anonymously, reading it whole when it
/// is at most [`PACKUMENT_MAX`].
async fn fetch_packument<U: Upstream>(
    npm: &Npm<U>,
    name: &str,
    form: Form,
) -> Result<Fetched, UpstreamError> {
    let mut headers = HeaderMap::new();
    headers.insert(header::ACCEPT, HeaderValue::from_static(form.accept()));
    let mut reply = npm
        .upstream
        .send(Ask {
            method: Method::GET,
            path: packument_path(name),
            headers,
            body: Body::empty(),
            follow: true,
        })
        .await?;
    let declared = reply
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<usize>().ok());
    if declared.is_some_and(|n| n > PACKUMENT_MAX) {
        return Ok(Fetched::Streaming(reply));
    }
    let mut buf = Vec::with_capacity(declared.unwrap_or(0));
    while let Some(chunk) = reply.body.next().await {
        let chunk = chunk.map_err(bad_gateway)?;
        buf.extend_from_slice(&chunk);
        if buf.len() > PACKUMENT_MAX {
            // Larger than it said, or it said nothing: stream what is read and
            // the rest, as it came.
            let read = futures_util::stream::iter([Ok(Bytes::from(buf))]);
            reply.body = Box::pin(read.chain(reply.body));
            reply.headers.remove(header::CONTENT_LENGTH);
            return Ok(Fetched::Streaming(reply));
        }
    }
    let content_type = reply
        .headers
        .get(header::CONTENT_TYPE)
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static("application/json"));
    Ok(Fetched::Whole {
        status: reply.status,
        body: Bytes::from(buf),
        content_type,
    })
}

/// A packument, as the cache or the registry gave it.
struct Got {
    body: Bytes,
    content_type: HeaderValue,
    from_cache: bool,
}

/// `name`'s packument in `form`: from the cache while it is fresh (fill on),
/// else from the registry, kept when fill is on, and the stale copy when the
/// registry fails. `Err` is the response to give instead.
async fn get_packument<U: Upstream>(
    npm: &Npm<U>,
    asker: &Asker,
    name: &str,
    form: Form,
) -> Result<Got, Box<Response>> {
    let caching = npm.control.npm() == Some(Fill::Relaxed);
    let key = (form, name.to_string());
    let stale = |npm: &Npm<U>| -> Option<Got> {
        let p = npm.packuments.lock().unwrap();
        p.entries.get(&key).map(|c| Got {
            body: c.body.clone(),
            content_type: c.content_type.clone(),
            from_cache: true,
        })
    };
    if caching {
        let p = npm.packuments.lock().unwrap();
        if let Some(c) = p.entries.get(&key).filter(|c| c.at.elapsed() < npm.ttl) {
            return Ok(Got {
                body: c.body.clone(),
                content_type: c.content_type.clone(),
                from_cache: true,
            });
        }
    }
    let fetched = match fetch_packument(npm, name, form).await {
        Ok(f) => f,
        Err(UpstreamError::Upstream { status, message }) => {
            if let Some(got) = stale(npm).filter(|_| caching) {
                tracing::info!("npm: serving {name} stale: {message}");
                return Ok(got);
            }
            let s = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            return Err(Box::new(npm_error(s, &message)));
        }
        Err(UpstreamError::Local(message)) => {
            asker.fail(&npm.control, &message);
            return Err(Box::new(npm_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &message,
            )));
        }
    };
    match fetched {
        Fetched::Whole {
            status,
            body,
            content_type,
        } if status == StatusCode::OK => {
            npm.control
                .count_npm(asker.repo(), |c| c.bytes_from_upstream += body.len() as u64);
            if caching {
                npm.packuments.lock().unwrap().put(
                    key,
                    Cached {
                        body: body.clone(),
                        content_type: content_type.clone(),
                        at: Instant::now(),
                    },
                );
            }
            Ok(Got {
                body,
                content_type,
                from_cache: false,
            })
        }
        Fetched::Whole {
            status,
            body,
            content_type,
        } => {
            if status.is_server_error() {
                if let Some(got) = stale(npm).filter(|_| caching) {
                    tracing::info!("npm: serving {name} stale: the registry answered {status}");
                    return Ok(got);
                }
            }
            // A 404, or the registry's own error: as it came, uncached.
            Err(Box::new(
                (status, [(header::CONTENT_TYPE, content_type)], body).into_response(),
            ))
        }
        Fetched::Streaming(reply) => {
            npm.control.count_npm(asker.repo(), |c| {
                c.misses += 1;
                c.passed_through += 1;
            });
            let body = counted(reply.body, npm.control.clone(), asker.repo().to_string());
            let mut res = Body::from_stream(body).into_response();
            *res.status_mut() = reply.status;
            *res.headers_mut() = end_to_end(&reply.headers);
            Err(Box::new(res))
        }
    }
}

async fn packument<U: Upstream>(
    npm: &Npm<U>,
    asker: &Asker,
    name: &str,
    form: Form,
    headers: &HeaderMap,
) -> Response {
    let got = match get_packument(npm, asker, name, form).await {
        Ok(g) => g,
        Err(res) => return *res,
    };
    let base = format!("{}/npm", href_base(headers));
    let body = rewrite_tarballs(&got.body, &base);
    let n = body.len() as u64;
    npm.control.count_npm(asker.repo(), |c| {
        if got.from_cache {
            c.hits += 1;
            c.bytes_from_cache += n;
        } else {
            c.misses += 1;
        }
    });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, got.content_type)],
        body,
    )
        .into_response()
}

/// Serve the stored object `key`, counting a hit when `hit`. `None` when it is
/// not (or no longer) there.
async fn from_store<U: Upstream>(
    npm: &Npm<U>,
    asker: &Asker,
    key: &str,
    hit: bool,
) -> Option<Response> {
    npm.store.has(key)?;
    match tokio::fs::File::open(npm.store.object_path(key)).await {
        Ok(f) => {
            let size = match f.metadata().await {
                Ok(m) => m.len(),
                Err(e) => {
                    asker.fail(&npm.control, &format!("reading a stored tarball: {e}"));
                    return None;
                }
            };
            npm.store.touch(key);
            if hit {
                npm.control.count_npm(asker.repo(), |c| {
                    c.hits += 1;
                    c.bytes_from_cache += size;
                });
            }
            Some(serve_file(f, size))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            npm.store.forget(key);
            None
        }
        Err(e) => {
            asker.fail(&npm.control, &format!("opening a stored tarball: {e}"));
            None
        }
    }
}

async fn tarball<U: Upstream>(
    npm: &Arc<Npm<U>>,
    asker: &Asker,
    name: &str,
    path: &str,
) -> Response {
    // A hit needs no packument: the hash was recorded when it was kept.
    let mut want: Option<Integrity> = None;
    if let Some(key) = npm.recorded(path).await {
        if let Some(r) = from_store(npm, asker, &key, true).await {
            return r;
        }
        want = Some(Integrity {
            sha512: key.len() == 128,
            hex: key,
        });
    }
    // A miss. Its hash comes from the packument, only when it would be kept:
    // passing through, the job's client checks the hash itself.
    if want.is_none() && npm.passing_through().is_none() {
        if let Ok(got) = get_packument(npm, asker, name, Form::Abbreviated).await {
            let body = got.body.clone();
            let p = path.to_string();
            want = tokio::task::spawn_blocking(move || integrity_for(&body, &p))
                .await
                .ok()
                .flatten();
            if let Some(w) = &want {
                npm.record(path, &w.hex).await;
                if let Some(r) = from_store(npm, asker, &w.hex, true).await {
                    return r;
                }
            }
        }
    }
    let Some(want) = want else {
        return stream_tarball(npm, asker, path, None).await;
    };
    if npm.passing_through().is_some() {
        return stream_tarball(npm, asker, path, Some(want)).await;
    }

    // Fill: one per key at a time; a second request waits and finds it stored.
    let lock = npm.fill_lock(&want.hex);
    let _held = lock.lock().await;
    if let Some(r) = from_store(npm, asker, &want.hex, true).await {
        return r;
    }
    let reply = match open_tarball(npm, asker, path).await {
        Ok(r) => r,
        Err(res) => return *res,
    };
    let size = reply
        .headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|n| *n <= MAX_TARBALL_BYTES);
    // The fill's temp file counts toward the cap from the start: room is made
    // for it, or it is streamed through and nothing is written.
    let room = size.and_then(|n| npm.store.reserve(n));
    let (Some(size), Some(room)) = (size, room) else {
        return streamed(npm, asker, reply, Some(want));
    };
    let filled = fill(npm, asker, reply.body, &want, size).await;
    drop(room);
    match filled {
        Filled::Stored => match from_store(npm, asker, &want.hex, false).await {
            Some(r) => r,
            None => stream_tarball(npm, asker, path, Some(want)).await,
        },
        Filled::Kept(tmp, f, n) => {
            // Verified but not storable: served from the temp file, unlinked
            // under the open handle.
            let _ = std::fs::remove_file(&tmp);
            serve_file(f, n)
        }
        Filled::Mismatch(why) => npm_error(StatusCode::BAD_GATEWAY, &why),
        Filled::Upstream(why) => npm_error(StatusCode::BAD_GATEWAY, &why),
        Filled::LocalFault => stream_tarball(npm, asker, path, Some(want)).await,
    }
}

/// Start the tarball's download from the registry. A refusal is the
/// registry's answer, passed on (a `404` stays a `404`).
async fn open_tarball<U: Upstream>(
    npm: &Npm<U>,
    asker: &Asker,
    path: &str,
) -> Result<Reply, Box<Response>> {
    let reply = npm
        .upstream
        .send(Ask {
            method: Method::GET,
            path: path.to_string(),
            headers: HeaderMap::new(),
            body: Body::empty(),
            follow: true,
        })
        .await;
    match reply {
        Ok(r) if r.status == StatusCode::OK => Ok(r),
        Ok(r) => {
            let status = r.status;
            let headers = end_to_end(&r.headers);
            let mut res = Body::from_stream(r.body).into_response();
            *res.status_mut() = status;
            *res.headers_mut() = headers;
            Err(Box::new(res))
        }
        Err(UpstreamError::Upstream { status, message }) => Err(Box::new(npm_error(
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            &message,
        ))),
        Err(UpstreamError::Local(message)) => {
            asker.fail(&npm.control, &message);
            Err(Box::new(npm_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &message,
            )))
        }
    }
}

/// Fetch the tarball and stream it without keeping it.
async fn stream_tarball<U: Upstream>(
    npm: &Npm<U>,
    asker: &Asker,
    path: &str,
    want: Option<Integrity>,
) -> Response {
    match open_tarball(npm, asker, path).await {
        Ok(reply) => streamed(npm, asker, reply, want),
        Err(res) => *res,
    }
}

/// Stream an open tarball download to the job, uncached, counting it. With
/// `want`, the bytes are hashed on the way and the stream is cut off before its
/// end when they do not match, so the job never receives a whole wrong file.
fn streamed<U: Upstream>(
    npm: &Npm<U>,
    asker: &Asker,
    reply: Reply,
    want: Option<Integrity>,
) -> Response {
    npm.control.count_npm(asker.repo(), |c| {
        c.misses += 1;
        c.passed_through += 1;
    });
    let control = npm.control.clone();
    let repo = asker.repo().to_string();
    // One chunk is held back while a hash is expected: the last one goes out
    // only once the whole download matched. On a mismatch the stream ends with
    // an error instead, short of its `Content-Length`, so hyper aborts the
    // connection and the job's client sees a truncated download, never a whole
    // wrong file. Checking after the last chunk would be too late: hyper ends a
    // response once its declared length is written.
    let state = Held {
        body: reply.body,
        hasher: want.as_ref().map(Integrity::hasher),
        want: want.map(|w| w.hex),
        held: None,
        done: false,
    };
    let body = futures_util::stream::unfold(state, move |mut st| {
        let control = control.clone();
        let repo = repo.clone();
        async move {
            loop {
                if st.done {
                    return None;
                }
                match st.body.next().await {
                    Some(Ok(b)) => {
                        let n = b.len() as u64;
                        control.count_npm(&repo, |c| c.bytes_from_upstream += n);
                        let Some(h) = st.hasher.as_mut() else {
                            return Some((Ok(b), st));
                        };
                        h.update(&b);
                        match st.held.replace(b) {
                            Some(prev) => return Some((Ok(prev), st)),
                            None => continue,
                        }
                    }
                    Some(Err(e)) => {
                        st.done = true;
                        return Some((Err(std::io::Error::other(e)), st));
                    }
                    None => {
                        st.done = true;
                        let (Some(h), Some(want)) = (st.hasher.take(), st.want.take()) else {
                            return None;
                        };
                        let got = h.finish();
                        if got == want {
                            return st.held.take().map(|b| (Ok(b), st));
                        }
                        tracing::warn!("npm: the registry's bytes do not match their integrity");
                        let err = std::io::Error::other(format!(
                            "the registry's bytes hash to {got}, not the packument's {want}"
                        ));
                        return Some((Err(err), st));
                    }
                }
            }
        }
    });
    let mut res = Body::from_stream(body).into_response();
    *res.status_mut() = reply.status;
    let headers = res.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    if let Some(len) = reply.headers.get(header::CONTENT_LENGTH) {
        headers.insert(header::CONTENT_LENGTH, len.clone());
    }
    res
}

/// A streamed tarball's state: the download, its hash so far, the hash it
/// must have, and the chunk held back until that is known.
struct Held {
    body: ByteStream,
    hasher: Option<Hasher>,
    want: Option<String>,
    held: Option<Bytes>,
    done: bool,
}

enum Filled {
    Stored,
    /// Verified, not stored; served from this temp file.
    Kept(PathBuf, tokio::fs::File, u64),
    /// The registry's bytes are not the tarball its packument names. Nothing
    /// is stored or served.
    Mismatch(String),
    Upstream(String),
    /// The store failed; stream it instead (ADR-0047 §7: local faults degrade).
    LocalFault,
}

/// Download to a temp file, at most `size` bytes (the registry's
/// `Content-Length`), hash it, and keep it when the hash is `want`.
async fn fill<U: Upstream>(
    npm: &Npm<U>,
    asker: &Asker,
    mut body: ByteStream,
    want: &Integrity,
    size: u64,
) -> Filled {
    let tmp = npm.store.tmp_path(&want.hex);
    let mut file = match tokio::fs::File::create(&tmp).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("npm: creating a temp file: {e}");
            asker.fail(&npm.control, &format!("creating a temp file: {e}"));
            return Filled::LocalFault;
        }
    };
    let mut hasher = want.hasher();
    let mut n: u64 = 0;
    let mut fault: Option<Filled> = None;
    while let Some(chunk) = body.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                fault = Some(Filled::Upstream(e));
                break;
            }
        };
        n += chunk.len() as u64;
        if n > size {
            fault = Some(Filled::Mismatch(format!(
                "the registry sent more than the {size} bytes it announced; not kept"
            )));
            break;
        }
        if npm.control.low_disk() {
            fault = Some(Filled::LocalFault);
            break;
        }
        hasher.update(&chunk);
        if let Err(e) = file.write_all(&chunk).await {
            tracing::warn!("npm: writing a fill: {e}");
            fault = Some(Filled::LocalFault);
            break;
        }
    }
    if fault.is_none() {
        if let Err(e) = file.flush().await {
            tracing::warn!("npm: flushing a fill: {e}");
            fault = Some(Filled::LocalFault);
        }
    }
    drop(file);
    if let Some(f) = fault {
        let _ = tokio::fs::remove_file(&tmp).await;
        return f;
    }
    let got = hasher.finish();
    if got != want.hex || n != size {
        let _ = tokio::fs::remove_file(&tmp).await;
        tracing::warn!("npm: the registry's bytes do not match their integrity; not kept");
        return Filled::Mismatch(format!(
            "the registry's bytes hash to {got} ({n} bytes), not the packument's {}; not kept",
            want.hex
        ));
    }
    npm.control.count_npm(asker.repo(), |c| {
        c.misses += 1;
        c.bytes_from_upstream += n;
    });
    let store = npm.store.clone();
    let (t, k) = (tmp.clone(), want.hex.clone());
    let kept = tokio::task::spawn_blocking(move || store.keep(&t, &k, n))
        .await
        .unwrap_or_else(|e| Err(NotStored::Io(e.to_string())));
    match kept {
        Ok(()) => Filled::Stored,
        Err(why) => {
            if let NotStored::Io(e) = &why {
                tracing::warn!("npm: storing {}: {e}", want.hex);
            }
            match tokio::fs::File::open(&tmp).await {
                Ok(f) => Filled::Kept(tmp, f, n),
                Err(_) => Filled::LocalFault,
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests;
