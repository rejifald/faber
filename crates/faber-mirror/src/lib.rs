//! faber's pull-through mirror service (ADR-0047).
//!
//! One `faber-mirror` container runs per host, on the Docker engine the runners
//! use. faber connects it to each opted-in job's private network under the alias
//! [`ALIAS`] and points the job's tools at it with environment it injects. A
//! tool's request then reaches this service, which answers from its cache or
//! fetches from upstream, fills the cache, and answers. Only faber's service
//! fills the cache; a job only reads through it.
//!
//! This crate is two things:
//!
//! - **the service** (`src/main.rs`): an axum server on [`PORT`] with
//!   `GET /healthz` and one route tree per [`Adapter`];
//! - **the contract the daemon shares**: the port, the alias, the health
//!   report's shape ([`Health`]), and the probe the daemon runs inside the
//!   container ([`parse_probe_response`]). The daemon depends on this library,
//!   so the two cannot disagree about any of them.
//!
//! Two adapters ship: Git LFS ([`lfs`]) and npm ([`npm`]). An ecosystem is one
//! [`Adapter`] registered in [`builtin`]; nothing else in the service changes.
//!
//! **Who is asking.** An LFS request carries its container's token in its
//! path. An npm request carries nothing: the registry URL faber injects is the
//! same for every job, so no secret ends up in a lockfile or a job log. The
//! service tells containers apart by the address the request arrived on
//! instead ([`Peer`]): it has one address on each job network it is connected
//! to, and the daemon's sync names the address it has on each container's
//! network ([`SyncEntry::addr`]).
//!
//! The daemon talks to the service only through `docker exec`: the health probe,
//! and `faber-mirror sync` ([`control`]), which hands it the token table and the
//! host's disk state on stdin and prints the counters back. Both reach the
//! server on the container's own loopback, so no port is ever published.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

pub mod control;
pub mod egress;
pub mod lfs;
pub mod npm;
pub mod store;

/// The port the service listens on inside its container, and the port a job
/// reaches it on at `http://faber-mirror:<PORT>`. Fixed: faber injects it into
/// every opted-in runner, so it is part of the contract, not a setting.
pub const PORT: u16 = 7470;

/// The host name a job reaches the service by, on its own network. faber
/// connects the container to the job's network with `--alias faber-mirror`.
pub const ALIAS: &str = "faber-mirror";

/// Where the service keeps its cache inside the container: faber's named volume
/// `faber-mirror-cache` is mounted here.
pub const CACHE_DIR: &str = "/cache";

/// The variable faber passes the cache's size limit in, in GiB (`[mirror]
/// max_size_gib`).
pub const MAX_SIZE_ENV: &str = "FABER_MIRROR_MAX_SIZE_GIB";

/// The control port: bound to `127.0.0.1` inside the container, so only a
/// process in the container itself (`docker exec faber-mirror faber-mirror
/// sync`) reaches it. No job network can.
pub const ADMIN_PORT: u16 = 7471;

/// Where faber mounts its GitHub credential, read-only: a named volume holding
/// one `0600` git-credential file, filled the way the git mirror's refresh
/// credential is (ADR-0035 §1).
pub const CREDENTIAL_DIR: &str = "/faber/auth";

/// The credential file inside [`CREDENTIAL_DIR`].
pub const CREDENTIAL_FILE: &str = "credentials";

/// The uid:gid the service runs as (the image's `USER`), which the credential
/// file is handed to.
pub const SERVICE_USER: &str = "65532:65532";

/// Where LFS objects are fetched from.
pub const GITHUB: &str = "https://github.com";

/// The one registry the npm adapter fetches from (ADR-0047 §5).
pub const NPMJS: &str = "https://registry.npmjs.org";

/// The counters' key for a request the service could not attribute to a
/// container: one that arrived on an address no sync has named yet.
pub const UNATTRIBUTED: &str = "(unattributed)";

// --- the daemon's wire ---------------------------------------------------------------

/// `[mirror.lfs] fill` and `[mirror.npm] fill` (ADR-0047 §3, §6). `strict` is
/// reserved and does not exist in this release.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fill {
    /// Any opted-in job's miss makes the mirror fetch, verify and store it.
    Relaxed,
    /// Hits are served; a miss is fetched and passed through, never stored.
    Off,
}

impl Fill {
    pub fn word(self) -> &'static str {
        match self {
            Self::Relaxed => "relaxed",
            Self::Off => "off",
        }
    }
}

/// One runner container the service answers for: its id (its job network's
/// name, which is not secret), its repo, its token, and the service's own
/// address on its network. A token of `None` keeps whatever token the service
/// holds for the id; an `addr` of `None` likewise keeps the address. No
/// `Debug`: it carries a secret.
#[derive(Clone, Serialize, Deserialize)]
pub struct SyncEntry {
    pub id: String,
    pub repo: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    /// The service's IP address on this container's network: a request that
    /// arrives on it came from that network, so from that container's job
    /// ([`Peer`]). Not a secret; it attributes, it does not authorize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub addr: Option<String>,
}

/// What a sync sets: the whole table, whether LFS and npm are served and how,
/// and whether the host is short of disk. No `Debug`: it carries secrets.
#[derive(Clone, Default, Serialize, Deserialize)]
pub struct SyncRequest {
    pub entries: Vec<SyncEntry>,
    #[serde(default)]
    pub lfs: Option<Fill>,
    #[serde(default)]
    pub npm: Option<Fill>,
    /// Host free disk is below ADR-0024's admission floor plus 5 GiB: store
    /// nothing and evict toward 80% of the cap (ADR-0047 §8).
    #[serde(default)]
    pub low_disk: bool,
    /// The cap in bytes, when the daemon states it (it overrides
    /// [`MAX_SIZE_ENV`]).
    #[serde(default)]
    pub max_size_bytes: Option<u64>,
    /// The daemon's first sync since it started: add, revoke nothing, so the
    /// containers it adopted across its restart keep their tokens.
    #[serde(default)]
    pub adopt: bool,
}

/// Per-repo counters of one ecosystem since the service started. CI logs carry
/// no byte counts, so these are how the trial measures the mirror (ADR-0047
/// §10). For LFS a request is an object; for npm it is a packument or a
/// tarball, and anything passed through.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoCounters {
    /// Requests served from the cache.
    pub hits: u64,
    /// Requests that went upstream (stored or passed through).
    pub misses: u64,
    /// Bytes served from the cache.
    pub bytes_from_cache: u64,
    /// Bytes fetched from upstream.
    pub bytes_from_upstream: u64,
    /// Misses served without being stored.
    #[serde(default)]
    pub passed_through: u64,
}

/// Requests from one container that failed because of the mirror itself.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobFailures {
    pub count: u64,
    pub last: String,
}

/// What the store holds.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreStats {
    pub objects: u64,
    pub bytes: u64,
    pub max_bytes: u64,
    /// Bytes of fills in flight, counted toward the cap before they are kept.
    #[serde(default)]
    pub in_flight: u64,
    /// The same figures per ecosystem (`lfs`, `npm`), under the one cap.
    #[serde(default)]
    pub usage: BTreeMap<String, Usage>,
}

/// One ecosystem's share of the store.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub objects: u64,
    pub bytes: u64,
}

/// What a sync prints back.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    /// Changes when the service restarts, which fails any request in flight
    /// across it.
    pub instance: String,
    /// The ids the token table holds.
    #[serde(default)]
    pub ids: Vec<String>,
    /// LFS counters, by `owner/repo`.
    #[serde(default)]
    pub repos: BTreeMap<String, RepoCounters>,
    /// npm counters, by `owner/repo`, or [`UNATTRIBUTED`].
    #[serde(default)]
    pub npm: BTreeMap<String, RepoCounters>,
    /// Mirror-caused failures, by container id.
    #[serde(default)]
    pub failures: BTreeMap<String, JobFailures>,
    #[serde(default)]
    pub store: StoreStats,
    /// Why the service is passing through, when it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub passing_through: Option<String>,
}

/// The passing-through reason the daemon's low-disk flag gives.
pub const LOW_DISK_REASON: &str =
    "host disk is below faber's admission floor plus 5 GiB: storing nothing and evicting \
     toward 80% of the cap";

/// What the service says about itself at `GET /healthz`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Health {
    /// The faber version the service was built from.
    pub version: String,
    pub status: HealthStatus,
    /// Why the service is passing through, when it is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Whether the service caches what it serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HealthStatus {
    /// Serving, and able to fill its cache.
    Ok,
    /// Serving, but every request goes straight to upstream: the cache cannot be
    /// used. Jobs still work; they are as slow as without the mirror.
    PassingThrough,
}

/// The health report for a service whose cache check came back `cache`.
pub fn health(version: &str, cache: Result<(), String>) -> Health {
    match cache {
        Ok(()) => Health {
            version: version.to_string(),
            status: HealthStatus::Ok,
            reason: None,
        },
        Err(why) => Health {
            version: version.to_string(),
            status: HealthStatus::PassingThrough,
            reason: Some(why),
        },
    }
}

/// Whether the cache directory can be written: create a probe file and remove
/// it again. An unwritable cache does not stop the service; it passes through.
pub fn check_cache(dir: &Path) -> Result<(), String> {
    let probe = dir.join(".faber-mirror-probe");
    std::fs::write(&probe, b"probe")
        .map_err(|e| format!("cache volume at {} is not writable: {e}", dir.display()))?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

// --- configuration -------------------------------------------------------------

/// The service's settings, from the environment faber starts its container with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The cache's size limit, in GiB. Greater than zero.
    pub max_size_gib: u32,
    pub cache_dir: PathBuf,
}

impl Config {
    /// Read the settings through `var` (the process environment in the service,
    /// a map in a test). [`MAX_SIZE_ENV`] is required: faber always sets it, so
    /// its absence means the container was not started by faber.
    pub fn from_vars(var: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let raw = var(MAX_SIZE_ENV).ok_or_else(|| format!("{MAX_SIZE_ENV} is not set"))?;
        let max_size_gib = raw
            .trim()
            .parse::<u32>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| format!("{MAX_SIZE_ENV} must be a whole number above 0, not {raw:?}"))?;
        Ok(Self {
            max_size_gib,
            cache_dir: PathBuf::from(CACHE_DIR),
        })
    }
}

// --- adapters --------------------------------------------------------------------

/// One ecosystem the mirror serves (LFS, npm, …).
///
/// An adapter owns the routes under `/<name>/`: what a tool asks for there, how
/// it is fetched from upstream, and how it is kept. faber redirects the tool
/// with environment it injects into the runner; the adapter is the other end of
/// that redirect.
pub trait Adapter: Send + Sync + 'static {
    /// The ecosystem's name and its path prefix: `lfs` serves `/lfs/...`.
    /// Lowercase ASCII letters, digits and `-`.
    fn name(&self) -> &'static str;

    /// The routes the adapter serves, relative to its prefix.
    fn routes(&self) -> Router;
}

/// Why an adapter could not be registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// The name is not a usable path prefix, or is one the service keeps.
    BadName(String),
    /// Two adapters claim the same name.
    Duplicate(String),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadName(n) => write!(f, "adapter name {n:?} is not a usable path prefix"),
            Self::Duplicate(n) => write!(f, "two adapters are named {n:?}"),
        }
    }
}

impl std::error::Error for RegistryError {}

/// Path prefixes the service itself uses, which no adapter may take.
const RESERVED: &[&str] = &["healthz"];

/// The adapters the service runs, by name.
#[derive(Default)]
pub struct Registry {
    adapters: Vec<Box<dyn Adapter>>,
}

impl Registry {
    /// Add `adapter`. Refused when its name is not a plain lowercase prefix, is
    /// reserved, or is already taken.
    pub fn register(&mut self, adapter: impl Adapter) -> Result<(), RegistryError> {
        let name = adapter.name();
        let plain = !name.is_empty()
            && name.len() <= 32
            && name.as_bytes()[0].is_ascii_lowercase()
            && name
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
        if !plain || RESERVED.contains(&name) {
            return Err(RegistryError::BadName(name.to_string()));
        }
        if self.adapters.iter().any(|a| a.name() == name) {
            return Err(RegistryError::Duplicate(name.to_string()));
        }
        self.adapters.push(Box::new(adapter));
        Ok(())
    }

    /// The registered adapters' names, in registration order.
    pub fn names(&self) -> Vec<&'static str> {
        self.adapters.iter().map(|a| a.name()).collect()
    }
}

/// The adapters this build of the service ships: LFS and npm. A new ecosystem
/// is one `register` call here.
pub fn builtin<L: lfs::Upstream, N: npm::Upstream>(
    lfs: Arc<lfs::Lfs<L>>,
    npm: Arc<npm::Npm<N>>,
) -> Registry {
    let mut r = Registry::default();
    r.register(lfs::Adapter(lfs))
        .expect("the built-in adapters have distinct plain names");
    r.register(npm::Adapter(npm))
        .expect("the built-in adapters have distinct plain names");
    r
}

// --- who is asking -----------------------------------------------------------------

/// The address a request arrived on: the service's own end of the connection.
/// The service has one address per job network it is connected to, so this
/// names the network, and through the sync's [`SyncEntry::addr`], the
/// container. Served with `into_make_service_with_connect_info::<Peer>()`; a
/// router driven without it (a unit test) sees no peer, and its requests are
/// [`UNATTRIBUTED`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Peer {
    pub local: Option<SocketAddr>,
}

impl
    axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, tokio::net::TcpListener>>
    for Peer
{
    fn connect_info(target: axum::serve::IncomingStream<'_, tokio::net::TcpListener>) -> Self {
        Self {
            local: target.io().local_addr().ok(),
        }
    }
}

// --- the server -------------------------------------------------------------------

/// What the health and control routes read.
#[derive(Clone)]
pub struct Service {
    version: Arc<str>,
    cache_dir: Arc<Path>,
    control: Arc<control::Control>,
    store: Arc<store::Store>,
}

impl Service {
    pub fn new(
        version: &str,
        cache_dir: &Path,
        control: Arc<control::Control>,
        store: Arc<store::Store>,
    ) -> Self {
        Self {
            version: Arc::from(version),
            cache_dir: Arc::from(cache_dir),
            control,
            store,
        }
    }

    /// Why the service cannot cache right now, if it cannot.
    fn passing_through(&self) -> Option<String> {
        if self.control.low_disk() {
            return Some(LOW_DISK_REASON.to_string());
        }
        self.store.broken().map(String::from)
    }

    /// The counters, with the store's own figures.
    pub fn stats(&self) -> Stats {
        Stats {
            store: self.store.stats(),
            passing_through: self.passing_through(),
            ..self.control.stats()
        }
    }
}

/// The service's router: `GET /healthz`, and each adapter's routes under its
/// name. This is what jobs reach.
pub fn router(registry: &Registry, service: Service) -> Router {
    let mut app = Router::new()
        .route("/healthz", get(healthz))
        .with_state(service);
    for adapter in &registry.adapters {
        app = app.nest(&format!("/{}", adapter.name()), adapter.routes());
    }
    app
}

/// The control router, served on `127.0.0.1:`[`ADMIN_PORT`] only: `POST /sync`
/// and `GET /stats`.
pub fn admin_router(service: Service) -> Router {
    Router::new()
        .route("/sync", post(sync))
        .route("/stats", get(stats))
        .with_state(service)
}

async fn healthz(State(s): State<Service>) -> Json<Health> {
    if let Some(why) = s.passing_through() {
        return Json(health(&s.version, Err(why)));
    }
    let dir = s.cache_dir.clone();
    let cache = tokio::task::spawn_blocking(move || check_cache(&dir))
        .await
        .unwrap_or_else(|e| Err(format!("cache check did not finish: {e}")));
    Json(health(&s.version, cache))
}

async fn sync(State(s): State<Service>, Json(req): Json<SyncRequest>) -> Json<Stats> {
    if let Some(bytes) = req.max_size_bytes.filter(|b| *b > 0) {
        s.store.set_max_bytes(bytes);
    }
    let low = req.low_disk;
    s.control.apply(req);
    if low {
        // ADR-0047 §8: toward 80% of the cap while the host is short of disk.
        let store = s.store.clone();
        let target = store.max_bytes() / 10 * 8;
        let _ = tokio::task::spawn_blocking(move || store.evict_to(target)).await;
    }
    Json(s.stats())
}

async fn stats(State(s): State<Service>) -> Json<Stats> {
    Json(s.stats())
}

// --- the probe ----------------------------------------------------------------------

/// The request the probe sends to the service on its own loopback.
pub const PROBE_REQUEST: &[u8] =
    b"GET /healthz HTTP/1.0\r\nHost: localhost\r\nConnection: close\r\n\r\n";

/// The most of a probe response the probe reads.
pub const PROBE_CAP: usize = 16 * 1024;

/// The most a sync request or its answer may be.
pub const SYNC_CAP: usize = 4 * 1024 * 1024;

/// The HTTP/1.0 request `faber-mirror sync` sends to the control port.
pub fn sync_request(body: &[u8]) -> Vec<u8> {
    let mut req = format!(
        "POST /sync HTTP/1.0\r\nHost: localhost\r\nContent-Type: application/json\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    req.extend_from_slice(body);
    req
}

/// The body of an HTTP/1.x response to [`PROBE_REQUEST`] or [`sync_request`],
/// when its status is 200. Anything else is an error naming what came back.
pub fn parse_probe_response(raw: &[u8]) -> Result<&str, String> {
    let text = std::str::from_utf8(raw).map_err(|_| "response is not UTF-8".to_string())?;
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| "response has no end of headers".to_string())?;
    let status = head.lines().next().unwrap_or_default();
    let code = status.split_whitespace().nth(1).unwrap_or_default();
    if !status.starts_with("HTTP/1.") || code != "200" {
        let shown: String = status.chars().take(80).collect();
        return Err(format!("the service answered {shown:?}"));
    }
    Ok(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    struct Fake(&'static str);

    impl Adapter for Fake {
        fn name(&self) -> &'static str {
            self.0
        }
        fn routes(&self) -> Router {
            Router::new().route("/ping", get(|| async { "pong" }))
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("faber-mirror-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn get_body(app: Router, path: &str) -> (StatusCode, String) {
        let res = app
            .oneshot(Request::get(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, String::from_utf8(bytes.to_vec()).unwrap())
    }

    #[test]
    fn health_is_ok_with_a_writable_cache_and_passing_through_without_one() {
        assert_eq!(
            health("1.2.3", Ok(())),
            Health {
                version: "1.2.3".into(),
                status: HealthStatus::Ok,
                reason: None
            }
        );
        let h = health("1.2.3", Err("no space".into()));
        assert_eq!(h.status, HealthStatus::PassingThrough);
        assert_eq!(h.reason.as_deref(), Some("no space"));
    }

    #[test]
    fn the_health_report_has_the_wire_shape_the_daemon_reads() {
        let ok = serde_json::to_value(health("1.2.3", Ok(()))).unwrap();
        assert_eq!(ok, serde_json::json!({"version": "1.2.3", "status": "ok"}));
        let pass = serde_json::to_value(health("1.2.3", Err("full".into()))).unwrap();
        assert_eq!(
            pass,
            serde_json::json!({"version": "1.2.3", "status": "passing_through", "reason": "full"})
        );
    }

    #[test]
    fn the_cache_check_reports_an_unwritable_directory() {
        let dir = scratch("check");
        assert_eq!(check_cache(&dir), Ok(()));
        assert!(
            !dir.join(".faber-mirror-probe").exists(),
            "probe left behind"
        );
        let missing = dir.join("absent");
        assert!(check_cache(&missing)
            .unwrap_err()
            .contains("is not writable"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_size_limit_is_required_and_above_zero() {
        let vars = |v: Option<&'static str>| {
            move |k: &str| (k == MAX_SIZE_ENV).then_some(v).flatten().map(String::from)
        };
        assert_eq!(
            Config::from_vars(vars(Some("20"))).unwrap().max_size_gib,
            20
        );
        assert_eq!(
            Config::from_vars(vars(Some(" 5 "))).unwrap().max_size_gib,
            5
        );
        for bad in [None, Some("0"), Some("-1"), Some("lots"), Some("")] {
            assert!(Config::from_vars(vars(bad)).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn an_adapter_needs_a_plain_unreserved_unique_name() {
        let mut r = Registry::default();
        assert_eq!(r.register(Fake("npm")), Ok(()));
        assert_eq!(r.register(Fake("git-lfs2")), Ok(()));
        assert_eq!(
            r.register(Fake("npm")),
            Err(RegistryError::Duplicate("npm".into()))
        );
        for bad in ["", "NPM", "-x", "a/b", "healthz", "x y", "2fa"] {
            assert_eq!(
                r.register(Fake(bad)),
                Err(RegistryError::BadName(bad.into())),
                "{bad:?}"
            );
        }
        assert_eq!(r.names(), vec!["npm", "git-lfs2"]);
    }

    #[test]
    fn the_image_exposes_and_prepares_what_the_contract_names() {
        let dockerfile = include_str!("../Dockerfile");
        assert!(
            dockerfile.contains(&format!("EXPOSE {PORT}")),
            "EXPOSE != PORT"
        );
        assert!(
            dockerfile.contains(&format!("mkdir {CACHE_DIR}")),
            "no cache dir"
        );
    }

    fn service(version: &str, cache: &Path) -> (Service, Arc<control::Control>) {
        let control = Arc::new(control::Control::new());
        let store = Arc::new(store::Store::open(&cache.join("lfs"), 1 << 20));
        (
            Service::new(version, cache, control.clone(), store),
            control,
        )
    }

    struct NoUpstream;

    impl lfs::Upstream for NoUpstream {
        async fn batch(
            &self,
            _: &str,
            _: &[lfs::ObjectSpec],
        ) -> Result<Vec<lfs::Confirmed>, lfs::UpstreamError> {
            Err(lfs::UpstreamError::Local("none".into()))
        }
        async fn download(&self, _: &lfs::Action) -> Result<lfs::ByteStream, lfs::UpstreamError> {
            Err(lfs::UpstreamError::Local("none".into()))
        }
    }

    impl npm::Upstream for NoUpstream {
        async fn send(&self, _: npm::Ask) -> Result<npm::Reply, lfs::UpstreamError> {
            Err(lfs::UpstreamError::Local("none".into()))
        }
    }

    #[test]
    fn the_shipped_registry_is_lfs_and_npm() {
        let dir = scratch("builtin");
        let (_, control) = service("1", &dir);
        let store = Arc::new(store::Store::open(&dir.join("store"), 1));
        let lfs = lfs::Lfs::new(control.clone(), store.clone(), NoUpstream);
        let npm = npm::Npm::new(control, store, &dir, NoUpstream);
        assert_eq!(builtin(lfs, npm).names(), vec!["lfs", "npm"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn healthz_answers_with_the_report() {
        let dir = scratch("healthz");
        let (s, control) = service("9.9.9", &dir);
        let (status, body) = get_body(router(&Registry::default(), s.clone()), "/healthz").await;
        assert_eq!(status, StatusCode::OK);
        let h: Health = serde_json::from_str(&body).unwrap();
        assert_eq!(h.status, HealthStatus::Ok);
        assert_eq!(h.version, "9.9.9");

        let (gone, _) = service("9.9.9", &dir.join("gone").join("x"));
        // The store made its directories; the cache check probes the volume root.
        let _ = std::fs::remove_dir_all(dir.join("gone"));
        let (_, body) = get_body(router(&Registry::default(), gone), "/healthz").await;
        let h: Health = serde_json::from_str(&body).unwrap();
        assert_eq!(h.status, HealthStatus::PassingThrough);
        assert!(h.reason.unwrap().contains("not writable"));

        // The daemon's low-disk flag passes through, with its reason.
        control.apply(SyncRequest {
            low_disk: true,
            ..SyncRequest::default()
        });
        let (_, body) = get_body(router(&Registry::default(), s), "/healthz").await;
        let h: Health = serde_json::from_str(&body).unwrap();
        assert_eq!(h.status, HealthStatus::PassingThrough);
        assert_eq!(h.reason.as_deref(), Some(LOW_DISK_REASON));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_sync_sets_the_table_and_answers_the_counters() {
        let dir = scratch("sync");
        let (s, control) = service("1", &dir);
        let body = serde_json::json!({
            "entries": [{"id": "faber-net-1", "repo": "owner/demo", "token": "t", "addr": "172.20.0.3"}],
            "lfs": "relaxed",
            "npm": "off",
            "low_disk": false,
            "max_size_bytes": 4096
        })
        .to_string();
        let res = admin_router(s)
            .oneshot(
                Request::post("/sync")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let stats: Stats = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(stats.ids, vec!["faber-net-1"]);
        assert_eq!(stats.store.max_bytes, 4096);
        assert_eq!(control.grant("t").unwrap().repo, "owner/demo");
        assert_eq!(control.lfs(), Some(Fill::Relaxed));
        assert_eq!(control.npm(), Some(Fill::Off));
        assert_eq!(
            control.by_addr("172.20.0.3".parse().unwrap()).unwrap().repo,
            "owner/demo"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_sync_request_is_framed_for_the_probe_parser() {
        let req = sync_request(b"{}");
        let text = String::from_utf8(req).unwrap();
        assert!(text.starts_with("POST /sync HTTP/1.0\r\n"));
        assert!(text.contains("Content-Length: 2\r\n"));
        assert!(text.ends_with("\r\n\r\n{}"));
    }

    #[tokio::test]
    async fn an_adapter_is_served_under_its_name_only() {
        let dir = scratch("nest");
        let mut r = Registry::default();
        r.register(Fake("fake")).unwrap();
        let (s, _) = service("1", &dir);
        let (status, body) = get_body(router(&r, s.clone()), "/fake/ping").await;
        assert_eq!((status, body.as_str()), (StatusCode::OK, "pong"));
        let (status, _) = get_body(router(&r, s), "/ping").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_probe_reads_a_200_body_and_refuses_anything_else() {
        let ok = b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\n\r\n{\"a\":1}";
        assert_eq!(parse_probe_response(ok), Ok("{\"a\":1}"));
        let err = parse_probe_response(b"HTTP/1.1 503 Service Unavailable\r\n\r\nx").unwrap_err();
        assert!(err.contains("503"), "{err}");
        assert!(parse_probe_response(b"HTTP/1.1 200 OK\r\n").is_err());
        assert!(parse_probe_response(b"SSH-2.0\r\n\r\n").is_err());
        assert!(parse_probe_response(&[0xff, 0xfe]).is_err());
    }
}
