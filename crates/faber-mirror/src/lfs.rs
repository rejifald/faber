//! The Git LFS adapter: the download side of the LFS Batch API, filled on a
//! miss (ADR-0047 §4, without warming).
//!
//! A job's git-lfs reaches it at `lfs.url`, which faber injects as
//! `http://faber-mirror:7470/lfs/<token>/<owner>/<repo>/info/lfs`, so git-lfs asks
//!
//! - `POST …/info/lfs/objects/batch` for which objects it can download, and
//! - `GET …/info/lfs/objects/<oid>` for each one.
//!
//! The token resolves to exactly one repo ([`crate::control::Grant`]). A token
//! the service does not know, or one used for another repo's path, is `403`.
//! Uploads are refused: pushes go to GitHub through `lfs.pushurl`.
//!
//! **A miss is confirmed upstream as the job's repo.** An object is served from
//! the store only when the repo's index names it. Otherwise faber's read-only
//! token asks GitHub's batch API, *as that repo*, for it, so GitHub decides
//! whether the repo has it. Bytes already stored for another repo are then
//! indexed for this one and served without a download; bytes not yet stored are
//! downloaded to a temp file, hashed, and stored only when sha256 is the oid. A
//! job never chooses what is stored: it can only cause a fill of what GitHub
//! serves for its own repo.
//!
//! **Passing through.** With `fill = "off"`, while the daemon reports the host
//! short of disk, or when the store cannot be written, a miss is still
//! confirmed and fetched upstream, but streamed to the job without being kept.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use futures_util::{Stream, StreamExt};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

use crate::control::{Control, Grant};
use crate::egress::Egress;
use crate::store::{valid_oid, NotStored, Store};
use crate::Fill;

/// The media type of every LFS API request and response.
pub const LFS_JSON: &str = "application/vnd.git-lfs+json";

/// The most objects one batch may name. git-lfs sends at most 100.
const BATCH_MAX: usize = 1000;

/// How long an upstream download action is reused for a `GET` that follows its
/// batch, at most. GitHub's actions last longer; a shorter reuse only costs one
/// more upstream batch.
const PENDING_TTL: Duration = Duration::from_secs(300);

/// How many upstream actions are remembered between a batch and its `GET`s.
const PENDING_CAP: usize = 10_000;

/// How long the download hrefs the service hands out are good for, in seconds:
/// as long as the upstream action behind them is remembered, so git-lfs asks
/// the batch API again rather than `GET` a link the service has forgotten.
const HREF_EXPIRES_IN: u64 = PENDING_TTL.as_secs();

/// The largest object the mirror stores, whatever GitHub says: GitHub's own LFS
/// limit is 5 GB per file on its largest plan. A bigger one is passed through,
/// and every download, kept or not, is cut off at the size GitHub stated.
pub const MAX_OBJECT_BYTES: u64 = 5 << 30;

/// The read size for a stored object.
const CHUNK: usize = 64 * 1024;

// --- upstream -----------------------------------------------------------------------

/// One object as a client or upstream names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectSpec {
    pub oid: String,
    pub size: u64,
}

/// Where and how to download one object. `header` may carry upstream's
/// credentials, so it has no `Debug` and never leaves the service.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Action {
    pub href: String,
    #[serde(default)]
    pub header: BTreeMap<String, String>,
}

/// Upstream's answer for one object.
#[derive(Clone, PartialEq, Eq)]
pub enum Answer {
    /// The repo has it; this downloads it.
    Download(Action),
    /// It does not, or upstream could not say (LFS error codes: 404, 410, 422…).
    Error { code: u16, message: String },
}

/// One object of an upstream batch answer.
#[derive(Clone, PartialEq, Eq)]
pub struct Confirmed {
    pub oid: String,
    pub size: u64,
    pub answer: Answer,
}

/// Why upstream gave no answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UpstreamError {
    /// Upstream failed or refused: GitHub's fault or the repo's, not the
    /// mirror's.
    Upstream { status: u16, message: String },
    /// The mirror's own fault: its credential is not readable, say.
    Local(String),
}

/// A download's bytes.
pub type ByteStream = Pin<Box<dyn Stream<Item = Result<Bytes, String>> + Send>>;

/// GitHub's LFS service, as the adapter uses it. Behind a trait so tests use an
/// in-house fake and never reach the real one.
pub trait Upstream: Send + Sync + 'static {
    /// Ask, as `repo`, for download actions for `objects`.
    fn batch(
        &self,
        repo: &str,
        objects: &[ObjectSpec],
    ) -> impl Future<Output = Result<Vec<Confirmed>, UpstreamError>> + Send;

    /// Start one download.
    fn download(
        &self,
        action: &Action,
    ) -> impl Future<Output = Result<ByteStream, UpstreamError>> + Send;
}

/// The real upstream: `https://github.com/<owner>/<repo>.git/info/lfs`, with
/// faber's read-only token from the credential file faber mounts read-only
/// ([`crate::CREDENTIAL_DIR`]). The file is read on every batch, so a token
/// faber rotates is picked up without a restart.
///
/// Downloads follow [`Egress`]: https to GitHub's LFS hosts only, at most
/// [`crate::egress::MAX_REDIRECTS`] redirects, each re-checked, and only to
/// public addresses (the client resolves through [`Egress::resolver`]).
pub struct GithubUpstream {
    base: String,
    credential: PathBuf,
    client: reqwest::Client,
    egress: Egress,
}

impl GithubUpstream {
    pub fn new(base: &str, credential: PathBuf, egress: Egress) -> Result<Self, String> {
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
            credential,
            client,
            egress,
        })
    }

    /// faber's token, from its git-credential file (`password=` line). `None`
    /// when the file is absent: faber runs anonymously, and so does the mirror.
    fn token(&self) -> Result<Option<String>, UpstreamError> {
        match std::fs::read_to_string(&self.credential) {
            Ok(text) => Ok(text
                .lines()
                .find_map(|l| l.strip_prefix("password="))
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(UpstreamError::Local(format!(
                "faber's GitHub credential is not readable: {e}"
            ))),
        }
    }
}

#[derive(Deserialize)]
struct WireBatch {
    #[serde(default)]
    objects: Vec<WireObject>,
}

#[derive(Deserialize)]
struct WireObject {
    oid: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    actions: Option<WireActions>,
    #[serde(default)]
    error: Option<WireError>,
}

#[derive(Deserialize)]
struct WireActions {
    #[serde(default)]
    download: Option<Action>,
}

#[derive(Serialize, Deserialize)]
struct WireError {
    #[serde(default)]
    code: u16,
    #[serde(default)]
    message: String,
}

/// An upstream batch body into [`Confirmed`] objects.
fn parse_batch(body: &[u8]) -> Result<Vec<Confirmed>, String> {
    let wire: WireBatch =
        serde_json::from_slice(body).map_err(|e| format!("not an LFS batch answer: {e}"))?;
    Ok(wire
        .objects
        .into_iter()
        .map(|o| {
            let answer = match (o.actions.and_then(|a| a.download), o.error) {
                (Some(action), _) => Answer::Download(action),
                (None, Some(e)) => Answer::Error {
                    code: e.code,
                    message: e.message,
                },
                (None, None) => Answer::Error {
                    code: 404,
                    message: "upstream offered no download".into(),
                },
            };
            Confirmed {
                oid: o.oid,
                size: o.size,
                answer,
            }
        })
        .collect())
}

impl Upstream for GithubUpstream {
    async fn batch(
        &self,
        repo: &str,
        objects: &[ObjectSpec],
    ) -> Result<Vec<Confirmed>, UpstreamError> {
        let token = self.token()?;
        let url = format!("{}/{repo}.git/info/lfs/objects/batch", self.base);
        let body = serde_json::json!({
            "operation": "download",
            "transfers": ["basic"],
            "objects": objects,
            "hash_algo": "sha256",
        });
        let mut req = self
            .client
            .post(url)
            .header(header::ACCEPT, LFS_JSON)
            .header(header::CONTENT_TYPE, LFS_JSON)
            .body(body.to_string())
            .timeout(Duration::from_secs(60));
        if let Some(t) = token {
            req = req.basic_auth("x-access-token", Some(t));
        }
        let res = req.send().await.map_err(|e| UpstreamError::Upstream {
            status: 502,
            message: format!("GitHub's LFS batch API did not answer: {}", e.without_url()),
        })?;
        let status = res.status().as_u16();
        let bytes = res.bytes().await.map_err(|e| UpstreamError::Upstream {
            status: 502,
            message: format!("reading GitHub's batch answer: {}", e.without_url()),
        })?;
        if status != 200 {
            return Err(UpstreamError::Upstream {
                status,
                message: format!("GitHub's LFS batch API answered {status} for {repo}"),
            });
        }
        parse_batch(&bytes).map_err(|message| UpstreamError::Upstream {
            status: 502,
            message,
        })
    }

    async fn download(&self, action: &Action) -> Result<ByteStream, UpstreamError> {
        let refuse = |why: String| UpstreamError::Upstream {
            status: 502,
            message: format!("not following GitHub's download link: {why}"),
        };
        let first = self.egress.check(&action.href).map_err(refuse)?;
        let mut url = first.clone();
        let mut hops = 0;
        let res = loop {
            let mut req = self.client.get(url.clone());
            // Upstream's headers may carry its credentials: they go to the host
            // that gave them, never to one a redirect names.
            if url.origin() == first.origin() {
                for (k, v) in &action.header {
                    req = req.header(k.as_str(), v.as_str());
                }
            }
            let res = req.send().await.map_err(|e| UpstreamError::Upstream {
                status: 502,
                message: format!("downloading from GitHub: {}", e.without_url()),
            })?;
            if !res.status().is_redirection() {
                break res;
            }
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
        };
        let status = res.status().as_u16();
        if status != 200 {
            return Err(UpstreamError::Upstream {
                status: if status == 404 { 404 } else { 502 },
                message: format!("GitHub's object download answered {status}"),
            });
        }
        Ok(Box::pin(futures_util::stream::unfold(
            res,
            |mut res| async {
                match res.chunk().await {
                    Ok(Some(b)) => Some((Ok(b), res)),
                    Ok(None) => None,
                    Err(e) => Some((
                        Err(format!("reading from GitHub: {}", e.without_url())),
                        res,
                    )),
                }
            },
        )))
    }
}

// --- the adapter ----------------------------------------------------------------------

/// Upstream actions remembered between a batch and its `GET`s: `(repo, oid)` to
/// the action, upstream's size, and when it was given.
type Pending = HashMap<(String, String), (Action, u64, Instant)>;

/// The LFS adapter's state.
pub struct Lfs<U> {
    control: Arc<Control>,
    store: Arc<Store>,
    upstream: U,
    /// Upstream actions from a batch, for the `GET`s that follow it, by
    /// `(repo, oid)`. Never shared across repos.
    pending: Mutex<Pending>,
    /// One fill per oid at a time.
    fills: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Which download links are followed at all ([`crate::egress`]).
    egress: Egress,
}

impl<U: Upstream> Lfs<U> {
    /// The adapter, following only GitHub's LFS hosts ([`Egress::github`]).
    pub fn new(control: Arc<Control>, store: Arc<Store>, upstream: U) -> Arc<Self> {
        Self::with_egress(control, store, upstream, Egress::github())
    }

    pub fn with_egress(
        control: Arc<Control>,
        store: Arc<Store>,
        upstream: U,
        egress: Egress,
    ) -> Arc<Self> {
        Arc::new(Self {
            control,
            store,
            upstream,
            pending: Mutex::new(HashMap::new()),
            fills: Mutex::new(HashMap::new()),
            egress,
        })
    }

    /// Start a download upstream offered, after checking its link: whatever
    /// the upstream, the mirror follows only [`Egress`]'s links.
    async fn download(&self, action: &Action) -> Result<ByteStream, UpstreamError> {
        self.egress
            .check(&action.href)
            .map_err(|why| UpstreamError::Upstream {
                status: 502,
                message: format!("not following upstream's download link: {why}"),
            })?;
        self.upstream.download(action).await
    }

    /// Why a miss would not be kept right now, if it would not.
    fn passing_through(&self) -> Option<&'static str> {
        if self.control.lfs() == Some(Fill::Off) {
            Some("fill is off")
        } else if self.control.low_disk() {
            Some("host disk low")
        } else if self.store.broken().is_some() {
            Some("store not writable")
        } else {
            None
        }
    }

    fn remember(&self, repo: &str, oid: &str, size: u64, action: Action) {
        let mut p = self.pending.lock().unwrap();
        if p.len() >= PENDING_CAP {
            p.retain(|_, (_, _, at)| at.elapsed() < PENDING_TTL);
            if p.len() >= PENDING_CAP {
                p.clear();
            }
        }
        p.insert(
            (repo.to_string(), oid.to_string()),
            (action, size, Instant::now()),
        );
    }

    fn recall(&self, repo: &str, oid: &str) -> Option<(Action, u64)> {
        let mut p = self.pending.lock().unwrap();
        let key = (repo.to_string(), oid.to_string());
        match p.get(&key) {
            Some((a, size, at)) if at.elapsed() < PENDING_TTL => Some((a.clone(), *size)),
            Some(_) => {
                p.remove(&key);
                None
            }
            None => None,
        }
    }

    fn fill_lock(&self, oid: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut f = self.fills.lock().unwrap();
        f.retain(|_, l| Arc::strong_count(l) > 1);
        f.entry(oid.to_string()).or_default().clone()
    }
}

/// The adapter's name and routes.
pub struct Adapter<U>(pub Arc<Lfs<U>>);

impl<U: Upstream> crate::Adapter for Adapter<U> {
    fn name(&self) -> &'static str {
        "lfs"
    }

    fn routes(&self) -> Router {
        Router::new()
            .route(
                "/{token}/{owner}/{repo}/info/lfs/objects/batch",
                post(batch::<U>),
            )
            .route(
                "/{token}/{owner}/{repo}/info/lfs/objects/{oid}",
                get(object::<U>),
            )
            .with_state(self.0.clone())
    }
}

/// An LFS API error: its JSON body and status.
fn lfs_error(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({ "message": message }).to_string();
    (status, [(header::CONTENT_TYPE, LFS_JSON)], body).into_response()
}

/// Resolve the request's token for `owner/repo`. An error is the response.
fn authorize<U: Upstream>(
    lfs: &Lfs<U>,
    token: &str,
    owner: &str,
    repo: &str,
) -> Result<Grant, Box<Response>> {
    if lfs.control.lfs().is_none() {
        return Err(Box::new(lfs_error(
            StatusCode::NOT_FOUND,
            "LFS is not served by this mirror",
        )));
    }
    let Some(grant) = lfs.control.grant(token) else {
        return Err(Box::new(lfs_error(
            StatusCode::FORBIDDEN,
            "this mirror token is not valid (it is minted per runner container and revoked \
             at teardown)",
        )));
    };
    if grant.repo != format!("{owner}/{repo}") {
        return Err(Box::new(lfs_error(
            StatusCode::FORBIDDEN,
            "this mirror token is not for this repository",
        )));
    }
    Ok(grant)
}

/// The base of the hrefs the service hands out: the host the client used.
pub(crate) fn href_base(headers: &HeaderMap) -> String {
    let host = headers
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .filter(|h| {
            !h.is_empty()
                && h.len() <= 255
                && h.bytes().all(|b| {
                    b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b':' | b'[' | b']')
                })
        })
        .map(String::from)
        .unwrap_or_else(|| format!("{}:{}", crate::ALIAS, crate::PORT));
    format!("http://{host}")
}

#[derive(Deserialize)]
struct BatchRequest {
    operation: String,
    #[serde(default)]
    objects: Vec<ObjectSpec>,
}

#[derive(Serialize)]
struct OutObject {
    oid: String,
    size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    authenticated: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    actions: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<WireError>,
}

async fn batch<U: Upstream>(
    State(lfs): State<Arc<Lfs<U>>>,
    Path((token, owner, repo)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let grant = match authorize(&lfs, &token, &owner, &repo) {
        Ok(g) => g,
        Err(r) => return *r,
    };
    let Ok(req) = serde_json::from_slice::<BatchRequest>(&body) else {
        return lfs_error(StatusCode::UNPROCESSABLE_ENTITY, "not an LFS batch request");
    };
    if req.operation != "download" {
        return lfs_error(
            StatusCode::FORBIDDEN,
            "faber's mirror only serves downloads; pushes go to GitHub through lfs.pushurl, \
             which faber sets to the repository's own LFS endpoint",
        );
    }
    if req.objects.len() > BATCH_MAX {
        return lfs_error(
            StatusCode::UNPROCESSABLE_ENTITY,
            "too many objects in one batch",
        );
    }
    let base = href_base(&headers);
    let href = |oid: &str| {
        serde_json::json!({ "download": {
            "href": format!("{base}/lfs/{token}/{owner}/{repo}/info/lfs/objects/{oid}"),
            "expires_in": HREF_EXPIRES_IN,
        }})
    };
    let ok = |oid: &str, size: u64| OutObject {
        oid: oid.to_string(),
        size,
        authenticated: Some(true),
        actions: Some(href(oid)),
        error: None,
    };
    let err = |oid: &str, size: u64, code: u16, message: String| OutObject {
        oid: oid.to_string(),
        size,
        authenticated: None,
        actions: None,
        error: Some(WireError { code, message }),
    };

    let mut out: Vec<Option<OutObject>> = Vec::with_capacity(req.objects.len());
    let mut misses: Vec<(usize, ObjectSpec)> = Vec::new();
    for (i, o) in req.objects.iter().enumerate() {
        if !valid_oid(&o.oid) {
            out.push(Some(err(&o.oid, o.size, 422, "not a sha256 oid".into())));
        } else if lfs.store.indexed(&grant.repo, &o.oid) {
            let size = lfs.store.has(&o.oid).unwrap_or(o.size);
            out.push(Some(ok(&o.oid, size)));
        } else {
            out.push(None);
            misses.push((i, o.clone()));
        }
    }
    if !misses.is_empty() {
        let specs: Vec<ObjectSpec> = misses.iter().map(|(_, o)| o.clone()).collect();
        match lfs.upstream.batch(&grant.repo, &specs).await {
            Ok(confirmed) => {
                let by_oid: HashMap<&str, &Confirmed> =
                    confirmed.iter().map(|c| (c.oid.as_str(), c)).collect();
                for (i, o) in &misses {
                    let found = by_oid.get(o.oid.as_str());
                    // Upstream's size, never the client's: GitHub answers a
                    // download only for the object's true size, and the fill
                    // and every pass-through are held to it. A download with no
                    // size is refused rather than trusted.
                    let size = found.map(|c| c.size).unwrap_or(0);
                    out[*i] = Some(match found.map(|c| &c.answer) {
                        Some(Answer::Download(_)) if size == 0 => err(
                            &o.oid,
                            o.size,
                            502,
                            "upstream offered a download with no size; refused".into(),
                        ),
                        Some(Answer::Download(action)) => {
                            if lfs.store.has(&o.oid).is_some() {
                                // Stored for another repo, now confirmed for this
                                // one: index it, no download needed.
                                if let Err(e) = lfs.store.grant(&grant.repo, &o.oid) {
                                    tracing::warn!("lfs: {e}");
                                }
                            } else {
                                lfs.remember(&grant.repo, &o.oid, size, action.clone());
                            }
                            ok(&o.oid, size)
                        }
                        Some(Answer::Error { code, message }) => {
                            err(&o.oid, o.size, *code, message.clone())
                        }
                        None => err(&o.oid, o.size, 404, "upstream did not answer for it".into()),
                    });
                }
            }
            Err(UpstreamError::Upstream { status, message }) => {
                let code = if status == 404 || status == 403 || status == 401 {
                    StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY)
                } else {
                    StatusCode::BAD_GATEWAY
                };
                return lfs_error(code, &message);
            }
            Err(UpstreamError::Local(message)) => {
                lfs.control.fail(&grant.id, &message);
                return lfs_error(StatusCode::INTERNAL_SERVER_ERROR, &message);
            }
        }
    }
    let objects: Vec<OutObject> = out.into_iter().flatten().collect();
    let body =
        serde_json::json!({ "transfer": "basic", "objects": objects, "hash_algo": "sha256" });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, LFS_JSON)],
        body.to_string(),
    )
        .into_response()
}

/// A response streaming the open file `f` of `size` bytes, counting `size` to
/// `repo` as served from the cache.
pub(crate) fn serve_file(f: tokio::fs::File, size: u64) -> Response {
    let stream = futures_util::stream::unfold(f, |mut f| async move {
        use tokio::io::AsyncReadExt;
        let mut buf = vec![0u8; CHUNK];
        match f.read(&mut buf).await {
            Ok(0) => None,
            Ok(n) => {
                buf.truncate(n);
                Some((Ok::<Bytes, std::io::Error>(Bytes::from(buf)), f))
            }
            Err(e) => Some((Err(e), f)),
        }
    });
    let mut res = Body::from_stream(stream).into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    res
}

/// Serve `oid` from the store, when it is there and `repo` may read it,
/// counting it a hit when `hit`.
async fn from_store<U: Upstream>(
    lfs: &Lfs<U>,
    grant: &Grant,
    oid: &str,
    hit: bool,
) -> Option<Response> {
    if !lfs.store.indexed(&grant.repo, oid) {
        return None;
    }
    let path = lfs.store.object_path(oid);
    match tokio::fs::File::open(&path).await {
        Ok(f) => {
            let size = match f.metadata().await {
                Ok(m) => m.len(),
                Err(e) => {
                    lfs.control
                        .fail(&grant.id, &format!("reading a stored object: {e}"));
                    return None;
                }
            };
            lfs.store.touch(oid);
            // A fill just counted its download as a miss; serving what it stored
            // is not also a hit.
            if hit {
                lfs.control.count(&grant.repo, |c| {
                    c.hits += 1;
                    c.bytes_from_cache += size;
                });
            }
            Some(serve_file(f, size))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // Evicted between the check and the open: a miss.
            lfs.store.forget(oid);
            None
        }
        Err(e) => {
            lfs.control
                .fail(&grant.id, &format!("opening a stored object: {e}"));
            None
        }
    }
}

async fn object<U: Upstream>(
    State(lfs): State<Arc<Lfs<U>>>,
    Path((token, owner, repo, oid)): Path<(String, String, String, String)>,
) -> Response {
    let grant = match authorize(&lfs, &token, &owner, &repo) {
        Ok(g) => g,
        Err(r) => return *r,
    };
    if !valid_oid(&oid) {
        return lfs_error(StatusCode::UNPROCESSABLE_ENTITY, "not a sha256 oid");
    }
    if let Some(r) = from_store(&lfs, &grant, &oid, true).await {
        return r;
    }

    // A miss. The batch that just confirmed it upstream as this repo left its
    // action and GitHub's size. Without one, the object can be confirmed only
    // when its bytes are already stored for another repo, whose verified size
    // is known; otherwise the job's git-lfs must ask the batch API again (its
    // href expires when the action is forgotten). The job's own idea of the
    // size is never used.
    let (action, size) = match lfs.recall(&grant.repo, &oid) {
        Some(found) => found,
        None => {
            let Some(stored) = lfs.store.has(&oid) else {
                return lfs_error(
                    StatusCode::NOT_FOUND,
                    "no batch answer for this object is remembered; ask the batch API again",
                );
            };
            let spec = ObjectSpec {
                oid: oid.clone(),
                size: stored,
            };
            match lfs.upstream.batch(&grant.repo, &[spec]).await {
                Ok(c) => match c
                    .into_iter()
                    .find(|c| c.oid == oid)
                    .map(|c| (c.answer, c.size))
                {
                    Some((Answer::Download(a), size)) if size == stored => (a, size),
                    Some((Answer::Download(_), _)) => {
                        return lfs_error(
                            StatusCode::BAD_GATEWAY,
                            "upstream's size for it is not the stored object's",
                        )
                    }
                    Some((Answer::Error { code, message }, _)) => {
                        let s = StatusCode::from_u16(code).unwrap_or(StatusCode::NOT_FOUND);
                        return lfs_error(s, &message);
                    }
                    None => return lfs_error(StatusCode::NOT_FOUND, "upstream does not have it"),
                },
                Err(UpstreamError::Upstream { message, .. }) => {
                    return lfs_error(StatusCode::BAD_GATEWAY, &message)
                }
                Err(UpstreamError::Local(message)) => {
                    lfs.control.fail(&grant.id, &message);
                    return lfs_error(StatusCode::INTERNAL_SERVER_ERROR, &message);
                }
            }
        }
    };
    // Confirmed for this repo. Bytes another repo's fill stored are indexed
    // and served without a download.
    if lfs.store.has(&oid).is_some() {
        if let Err(e) = lfs.store.grant(&grant.repo, &oid) {
            tracing::warn!("lfs: {e}");
        }
        if let Some(r) = from_store(&lfs, &grant, &oid, true).await {
            return r;
        }
    }
    if lfs.passing_through().is_some() || size > MAX_OBJECT_BYTES {
        return pass_through(&lfs, &grant, &action, size).await;
    }

    // Fill: one per oid at a time; a second request waits and finds it stored.
    let lock = lfs.fill_lock(&oid);
    let _held = lock.lock().await;
    if lfs.store.has(&oid).is_some() {
        if let Err(e) = lfs.store.grant(&grant.repo, &oid) {
            tracing::warn!("lfs: {e}");
        }
        if let Some(r) = from_store(&lfs, &grant, &oid, true).await {
            return r;
        }
    }
    // The fill's temp file counts toward the cap from the start: room is made
    // for it, or it is not written at all.
    let Some(room) = lfs.store.reserve(size) else {
        return pass_through(&lfs, &grant, &action, size).await;
    };
    let filled = fill(&lfs, &grant, &oid, size, &action).await;
    drop(room);
    match filled {
        Filled::Stored => match from_store(&lfs, &grant, &oid, false).await {
            Some(r) => r,
            None => pass_through(&lfs, &grant, &action, size).await,
        },
        Filled::Kept(path, f, n) => {
            // Verified but not storable (larger than the cap, or a store
            // error): serve the temp file, which is unlinked under the open
            // handle.
            let _ = std::fs::remove_file(&path);
            serve_file(f, n)
        }
        Filled::Mismatch(why) => lfs_error(StatusCode::BAD_GATEWAY, &why),
        Filled::Upstream(status, why) => lfs_error(status, &why),
        Filled::LocalFault => pass_through(&lfs, &grant, &action, size).await,
    }
}

enum Filled {
    Stored,
    /// Verified, not stored; served from this temp file.
    Kept(PathBuf, tokio::fs::File, u64),
    /// Upstream's bytes are not the object asked for. Nothing is stored.
    Mismatch(String),
    Upstream(StatusCode, String),
    /// The store failed; pass through instead (ADR-0047 §7: local faults
    /// degrade).
    LocalFault,
}

/// Download `oid` to a temp file, at most `size` bytes (GitHub's size, never
/// the job's), hash it, and store it when sha256 is the oid. A host that runs
/// short of disk mid-fill stops it, and the request passes through instead.
async fn fill<U: Upstream>(
    lfs: &Lfs<U>,
    grant: &Grant,
    oid: &str,
    size: u64,
    action: &Action,
) -> Filled {
    let tmp = lfs.store.tmp_path(oid);
    let mut file = match tokio::fs::File::create(&tmp).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!("lfs: creating a temp file: {e}");
            return Filled::LocalFault;
        }
    };
    let mut body = match lfs.download(action).await {
        Ok(b) => b,
        Err(e) => {
            let _ = tokio::fs::remove_file(&tmp).await;
            return match e {
                UpstreamError::Upstream { status, message } => Filled::Upstream(
                    StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                    message,
                ),
                UpstreamError::Local(message) => {
                    lfs.control.fail(&grant.id, &message);
                    Filled::Upstream(StatusCode::INTERNAL_SERVER_ERROR, message)
                }
            };
        }
    };
    let mut hasher = Sha256::new();
    let mut n: u64 = 0;
    let mut fault: Option<Filled> = None;
    while let Some(chunk) = body.next().await {
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                fault = Some(Filled::Upstream(StatusCode::BAD_GATEWAY, e));
                break;
            }
        };
        n += chunk.len() as u64;
        if n > size {
            fault = Some(Filled::Mismatch(format!(
                "upstream sent more than the {size} bytes of {oid}; not stored"
            )));
            break;
        }
        if lfs.control.low_disk() {
            fault = Some(Filled::LocalFault);
            break;
        }
        hasher.update(&chunk);
        if let Err(e) = file.write_all(&chunk).await {
            tracing::warn!("lfs: writing a fill: {e}");
            fault = Some(Filled::LocalFault);
            break;
        }
    }
    if fault.is_none() {
        if let Err(e) = file.flush().await {
            tracing::warn!("lfs: flushing a fill: {e}");
            fault = Some(Filled::LocalFault);
        }
    }
    drop(file);
    if let Some(f) = fault {
        let _ = tokio::fs::remove_file(&tmp).await;
        return f;
    }
    let got: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if got != oid || n != size {
        let _ = tokio::fs::remove_file(&tmp).await;
        tracing::warn!(
            "lfs: {}: upstream bytes do not hash to the oid; not stored",
            grant.repo
        );
        return Filled::Mismatch(format!(
            "upstream's bytes for {oid} hash to {got} ({n} bytes); not stored"
        ));
    }
    lfs.control.count(&grant.repo, |c| {
        c.misses += 1;
        c.bytes_from_upstream += n;
    });
    let store = lfs.store.clone();
    let (t, o, r) = (tmp.clone(), oid.to_string(), grant.repo.clone());
    let stored = tokio::task::spawn_blocking(move || store.commit(&t, &o, n, &r))
        .await
        .unwrap_or_else(|e| Err(NotStored::Io(e.to_string())));
    match stored {
        Ok(()) => Filled::Stored,
        Err(why) => {
            if let NotStored::Io(e) = &why {
                tracing::warn!("lfs: storing {oid}: {e}");
            }
            match tokio::fs::File::open(&tmp).await {
                Ok(f) => Filled::Kept(tmp, f, n),
                Err(_) => Filled::LocalFault,
            }
        }
    }
}

/// Fetch upstream and stream it to the job without keeping it, cut off past
/// `size` (GitHub's). git-lfs checks the hash itself.
async fn pass_through<U: Upstream>(
    lfs: &Lfs<U>,
    grant: &Grant,
    action: &Action,
    size: u64,
) -> Response {
    let body = match lfs.download(action).await {
        Ok(b) => b,
        Err(UpstreamError::Upstream { status, message }) => {
            return lfs_error(
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
                &message,
            )
        }
        Err(UpstreamError::Local(message)) => {
            lfs.control.fail(&grant.id, &message);
            return lfs_error(StatusCode::INTERNAL_SERVER_ERROR, &message);
        }
    };
    lfs.control.count(&grant.repo, |c| {
        c.misses += 1;
        c.passed_through += 1;
    });
    let control = lfs.control.clone();
    let repo = grant.repo.clone();
    let mut sent: u64 = 0;
    let counted = body.map(move |chunk| {
        let b = chunk.map_err(std::io::Error::other)?;
        sent += b.len() as u64;
        if sent > size {
            return Err(std::io::Error::other(
                "upstream sent more than the object's size",
            ));
        }
        let n = b.len() as u64;
        control.count(&repo, |c| c.bytes_from_upstream += n);
        Ok(b)
    });
    let mut res = Body::from_stream(counted).into_response();
    res.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    res
}

#[cfg(test)]
mod tests;
