//! The npm adapter against an in-house fake of the registry. Never the real
//! one.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::extract::connect_info::MockConnectInfo;
use axum::http::{Request, StatusCode};
use sha2::{Digest, Sha512};
use tower::ServiceExt;

use super::*;
use crate::{SyncEntry, SyncRequest};

/// The registry as the adapter sees it: packages and their tarballs, what a
/// download sends (normally the tarball; a corrupt one sends other bytes), and
/// what it was asked.
/// name → its versions and their tarball bytes.
type Packages = HashMap<String, Vec<(String, Vec<u8>)>>;

/// One request the fake got: method, path, `Authorization`, body.
type Asked = (String, String, Option<String>, Vec<u8>);

#[derive(Default)]
pub(crate) struct FakeRegistry {
    packages: Mutex<Packages>,
    corrupt: Mutex<HashMap<String, Vec<u8>>>,
    /// A package published before `integrity`: its versions carry `shasum` only.
    legacy: Mutex<Vec<String>>,
    pub packuments: AtomicUsize,
    pub tarballs: AtomicUsize,
    /// Every request.
    pub asked: Mutex<Vec<Asked>>,
    pub down: AtomicBool,
    pub local_fault: AtomicBool,
}

pub(crate) fn tarball_path(name: &str, version: &str) -> String {
    let file = name.rsplit('/').next().unwrap();
    format!("/{name}/-/{file}-{version}.tgz")
}

fn sri(bytes: &[u8]) -> String {
    format!(
        "sha512-{}",
        base64::engine::general_purpose::STANDARD.encode(Sha512::digest(bytes))
    )
}

fn sha1_hex(bytes: &[u8]) -> String {
    hex(&sha1::Sha1::digest(bytes))
}

impl FakeRegistry {
    pub(crate) fn shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    pub(crate) fn publish(&self, name: &str, version: &str, bytes: &[u8]) {
        self.packages
            .lock()
            .unwrap()
            .entry(name.into())
            .or_default()
            .push((version.into(), bytes.to_vec()));
    }

    fn packument(&self, name: &str, form: Form) -> Option<serde_json::Value> {
        let packages = self.packages.lock().unwrap();
        let versions = packages.get(name)?;
        let legacy = self.legacy.lock().unwrap().contains(&name.to_string());
        let mut out = serde_json::Map::new();
        for (v, bytes) in versions {
            let mut dist = serde_json::json!({
                "tarball": format!("{}{}", crate::NPMJS, tarball_path(name, v)),
                "shasum": sha1_hex(bytes),
            });
            if !legacy {
                dist["integrity"] = sri(bytes).into();
            }
            let mut doc = serde_json::json!({ "name": name, "version": v, "dist": dist });
            if form == Form::Full {
                doc["readme"] = "a full packument carries more".into();
            }
            out.insert(v.clone(), doc);
        }
        Some(serde_json::json!({ "name": name, "versions": out }))
    }

    fn tarball(&self, path: &str) -> Option<Vec<u8>> {
        if let Some(b) = self.corrupt.lock().unwrap().get(path) {
            return Some(b.clone());
        }
        let packages = self.packages.lock().unwrap();
        packages.iter().find_map(|(name, versions)| {
            versions
                .iter()
                .find(|(v, _)| tarball_path(name, v) == path)
                .map(|(_, b)| b.clone())
        })
    }
}

fn reply(status: StatusCode, content_type: &str, body: Vec<u8>) -> Reply {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    headers.insert(header::CONTENT_LENGTH, body.len().into());
    // Two chunks, so the adapter really streams.
    let mid = body.len() / 2;
    let chunks = vec![
        Ok(Bytes::from(body[..mid].to_vec())),
        Ok(Bytes::from(body[mid..].to_vec())),
    ];
    Reply {
        status,
        headers,
        body: Box::pin(futures_util::stream::iter(chunks)),
    }
}

impl Upstream for Arc<FakeRegistry> {
    async fn send(&self, ask: Ask) -> Result<Reply, UpstreamError> {
        if self.local_fault.load(Ordering::Relaxed) {
            return Err(UpstreamError::Local("the mirror's own fault".into()));
        }
        let body = axum::body::to_bytes(ask.body, 1 << 20)
            .await
            .unwrap()
            .to_vec();
        let auth = ask
            .headers
            .get(header::AUTHORIZATION)
            .map(|v| v.to_str().unwrap().to_string());
        self.asked.lock().unwrap().push((
            ask.method.to_string(),
            ask.path.clone(),
            auth.clone(),
            body.clone(),
        ));
        if self.down.load(Ordering::Relaxed) {
            return Err(UpstreamError::Upstream {
                status: 502,
                message: "the registry is down".into(),
            });
        }
        if ask.method != Method::GET || auth.is_some() {
            let echo = serde_json::json!({"method": ask.method.to_string(), "path": ask.path, "auth": auth});
            return Ok(reply(
                StatusCode::CREATED,
                "application/json",
                echo.to_string().into_bytes(),
            ));
        }
        match classify(&ask.path) {
            Target::Packument { name } => {
                self.packuments.fetch_add(1, Ordering::Relaxed);
                assert_eq!(ask.path, packument_path(&name), "a scoped name is encoded");
                let form = Form::of(&ask.headers);
                match self.packument(&name, form) {
                    Some(p) => {
                        let ct = if form == Form::Abbreviated {
                            ABBREVIATED
                        } else {
                            "application/json"
                        };
                        Ok(reply(StatusCode::OK, ct, p.to_string().into_bytes()))
                    }
                    None => Ok(reply(
                        StatusCode::NOT_FOUND,
                        "application/json",
                        b"{\"error\":\"Not found\"}".to_vec(),
                    )),
                }
            }
            Target::Tarball { path, .. } => {
                self.tarballs.fetch_add(1, Ordering::Relaxed);
                match self.tarball(&path) {
                    Some(b) => Ok(reply(StatusCode::OK, "application/octet-stream", b)),
                    None => Ok(reply(
                        StatusCode::NOT_FOUND,
                        "application/json",
                        b"{}".to_vec(),
                    )),
                }
            }
            Target::Other => {
                let echo = serde_json::json!({"path": ask.path});
                Ok(reply(
                    StatusCode::OK,
                    "application/json",
                    echo.to_string().into_bytes(),
                ))
            }
        }
    }
}

/// The service's address on owner/demo's job network, as the daemon's sync names it.
const DEMO_ADDR: &str = "172.20.0.3";

struct Rig {
    dir: PathBuf,
    reg: Arc<FakeRegistry>,
    control: Arc<Control>,
    store: Arc<Store>,
    app: Router,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn rig(tag: &str, max_bytes: u64, ttl: Duration) -> Rig {
    let dir = std::env::temp_dir().join(format!("faber-mirror-npm-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    rig_in(dir, Arc::new(FakeRegistry::default()), max_bytes, ttl)
}

/// A rig over `dir` as a previous one left it: a restarted service.
fn rig_in(dir: PathBuf, reg: Arc<FakeRegistry>, max_bytes: u64, ttl: Duration) -> Rig {
    let control = Arc::new(Control::new());
    let store = Arc::new(Store::open(&dir.join("store"), max_bytes));
    let npm = Npm::with_ttl(control.clone(), store.clone(), &dir, reg.clone(), ttl);
    let mut registry = crate::Registry::default();
    registry.register(Adapter(npm)).unwrap();
    let service = crate::Service::new("t", &dir, control.clone(), store.clone());
    let app = crate::router(&registry, service).layer(MockConnectInfo(Peer {
        local: Some(format!("{DEMO_ADDR}:7470").parse().unwrap()),
    }));
    control.apply(SyncRequest {
        entries: vec![SyncEntry {
            id: "faber-net-a".into(),
            repo: "owner/demo".into(),
            token: None,
            addr: Some(DEMO_ADDR.into()),
        }],
        npm: Some(Fill::Relaxed),
        ..SyncRequest::default()
    });
    Rig {
        dir,
        reg,
        control,
        store,
        app,
    }
}

const TTL: Duration = PACKUMENT_TTL;

impl Rig {
    async fn send(&self, req: Request<Body>) -> (StatusCode, HeaderMap, Result<Vec<u8>, String>) {
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let body = axum::body::to_bytes(res.into_body(), 1 << 24)
            .await
            .map(|b| b.to_vec())
            .map_err(|e| format!("{e:?}"));
        (status, headers, body)
    }

    async fn get(&self, path: &str, accept: Option<&str>) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut req = Request::get(format!("/npm{path}")).header("host", "faber-mirror:7470");
        if let Some(a) = accept {
            req = req.header("accept", a);
        }
        let (s, h, b) = self.send(req.body(Body::empty()).unwrap()).await;
        (s, h, b.expect("a whole body"))
    }

    fn sync(&self, fill: Option<Fill>, low_disk: bool) {
        self.control.apply(SyncRequest {
            entries: vec![SyncEntry {
                id: "faber-net-a".into(),
                repo: "owner/demo".into(),
                token: None,
                addr: Some(DEMO_ADDR.into()),
            }],
            npm: fill,
            low_disk,
            ..SyncRequest::default()
        });
    }

    fn counters(&self) -> crate::RepoCounters {
        self.control
            .stats()
            .npm
            .get("owner/demo")
            .cloned()
            .unwrap_or_default()
    }
}

fn key_of(bytes: &[u8]) -> String {
    hex(&Sha512::digest(bytes))
}

// --- pure ----------------------------------------------------------------------------

#[test]
fn requests_are_classified_by_their_path() {
    let pk = |n: &str| Target::Packument { name: n.into() };
    let tb = |n: &str, p: &str| Target::Tarball {
        name: n.into(),
        path: p.into(),
    };
    for (raw, want) in [
        ("/react", pk("react")),
        ("/JSONStream", pk("JSONStream")),
        ("/@types%2fnode", pk("@types/node")),
        ("/@types%2Fnode", pk("@types/node")),
        ("/%40types%2fnode", pk("@types/node")),
        ("/@types/node", pk("@types/node")),
        (
            "/react/-/react-18.2.0.tgz",
            tb("react", "/react/-/react-18.2.0.tgz"),
        ),
        (
            "/@types/node/-/node-20.1.0+build.1.tgz",
            tb("@types/node", "/@types/node/-/node-20.1.0+build.1.tgz"),
        ),
        (
            "/@types%2fnode/-/node-20.1.0.tgz",
            tb("@types/node", "/@types/node/-/node-20.1.0.tgz"),
        ),
    ] {
        assert_eq!(classify(raw), want, "{raw}");
    }
    for other in [
        "/",
        "/-/whoami",
        "/-/v1/search",
        "/-/npm/v1/security/advisories/bulk",
        "/react/18.2.0",
        "/react/latest",
        "/react/-/react-18.2.0.tar",
        "/../etc/passwd",
        "/.bin",
        "/a%20b",
        "/@/x",
        "/a/b/c",
        "/react/-/../../x.tgz",
    ] {
        assert_eq!(classify(other), Target::Other, "{other}");
    }
}

#[test]
fn integrity_prefers_sha512_and_falls_back_to_sha1() {
    let bytes = b"a tarball";
    let i = Integrity::from_dist(Some(&sri(bytes)), Some(&sha1_hex(bytes))).unwrap();
    assert_eq!(
        i,
        Integrity {
            sha512: true,
            hex: key_of(bytes)
        }
    );
    // Several hashes, sha512 not first, an option suffix.
    let several = format!("sha1-AAAA {}?opt", sri(bytes));
    assert!(Integrity::from_dist(Some(&several), None).unwrap().sha512);
    // The oldest packages: shasum only, in either case.
    let sha1 = Integrity::from_dist(None, Some(&sha1_hex(bytes).to_uppercase())).unwrap();
    assert_eq!(
        sha1,
        Integrity {
            sha512: false,
            hex: sha1_hex(bytes)
        }
    );
    let b64 = base64::engine::general_purpose::STANDARD.encode(sha1::Sha1::digest(bytes));
    assert_eq!(
        Integrity::from_dist(Some(&format!("sha1-{b64}")), None)
            .unwrap()
            .hex,
        sha1_hex(bytes)
    );
    for (i, s) in [
        (None, None),
        (Some("sha512-notbase64!"), None),
        (Some("sha512-AAAA"), None),
        (Some("sha256-AAAA"), Some("zz")),
        (None, Some("abc")),
    ] {
        assert!(Integrity::from_dist(i, s).is_none(), "{i:?} {s:?}");
    }
}

#[test]
fn only_tarball_values_on_the_registry_are_rewritten() {
    let body = br#"{"name":"x","description":"\"tarball\": \"https://registry.npmjs.org/x\"",
        "versions":{"1.0.0":{"dist":{ "tarball" :  "https://registry.npmjs.org/x/-/x-1.0.0.tgz"}},
        "2.0.0":{"dist":{"tarball":"https://elsewhere.example/x-2.0.0.tgz"}},
        "3.0.0":{"homepage":"https://registry.npmjs.org/x","dist":{"tarball":"https://registry.npmjs.org/@s/x/-/x-3.0.0.tgz"}}}}"#;
    let out = String::from_utf8(rewrite_tarballs(body, "http://faber-mirror:7470/npm")).unwrap();
    assert!(
        out.contains(r#""tarball" :  "http://faber-mirror:7470/npm/x/-/x-1.0.0.tgz""#),
        "{out}"
    );
    assert!(out.contains(r#""tarball":"http://faber-mirror:7470/npm/@s/x/-/x-3.0.0.tgz""#));
    assert!(
        out.contains("https://elsewhere.example/x-2.0.0.tgz"),
        "another host is left"
    );
    assert!(
        out.contains(r#""homepage":"https://registry.npmjs.org/x""#),
        "not a tarball"
    );
    assert!(
        out.contains(r#"\"tarball\": \"https://registry.npmjs.org/x\""#),
        "inside a string"
    );
    serde_json::from_str::<serde_json::Value>(&out).expect("still JSON");
}

#[test]
fn a_tarballs_hash_is_the_one_its_packument_names_for_its_path() {
    let reg = FakeRegistry::default();
    reg.publish("@s/x", "1.0.0", b"one");
    reg.publish("@s/x", "2.0.0", b"two");
    let body = reg
        .packument("@s/x", Form::Abbreviated)
        .unwrap()
        .to_string();
    let i = integrity_for(body.as_bytes(), &tarball_path("@s/x", "2.0.0")).unwrap();
    assert_eq!(i.hex, key_of(b"two"));
    assert!(integrity_for(body.as_bytes(), &tarball_path("@s/x", "3.0.0")).is_none());
    assert!(integrity_for(b"not json", "/x/-/x-1.tgz").is_none());
}

// --- packuments ----------------------------------------------------------------------

#[tokio::test]
async fn a_packument_is_rewritten_cached_per_form_and_fetched_again_after_its_ttl() {
    let r = rig("packument", 1 << 20, Duration::from_millis(200));
    r.reg.publish("@s/demo", "1.0.0", b"tgz");
    let (s, h, body) = r.get("/@s%2fdemo", Some(ABBREVIATED)).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(h[header::CONTENT_TYPE], ABBREVIATED);
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(
        v["versions"]["1.0.0"]["dist"]["tarball"],
        "http://faber-mirror:7470/npm/@s/demo/-/demo-1.0.0.tgz"
    );
    // Within the TTL: the cache, either spelling of the name.
    r.get("/@s/demo", Some(ABBREVIATED)).await;
    assert_eq!(r.reg.packuments.load(Ordering::Relaxed), 1);
    // The full form is its own entry.
    let (_, _, full) = r.get("/@s%2fdemo", None).await;
    assert!(String::from_utf8(full).unwrap().contains("readme"));
    assert_eq!(r.reg.packuments.load(Ordering::Relaxed), 2);
    let c = r.counters();
    assert_eq!((c.hits, c.misses), (1, 2));
    assert!(c.bytes_from_cache > 0 && c.bytes_from_upstream > 0);
    // Past the TTL: fetched again.
    tokio::time::sleep(Duration::from_millis(250)).await;
    r.get("/@s/demo", Some(ABBREVIATED)).await;
    assert_eq!(r.reg.packuments.load(Ordering::Relaxed), 3);
}

#[tokio::test]
async fn a_stale_packument_is_served_when_the_registry_fails_and_a_404_is_not_kept() {
    let r = rig("stale", 1 << 20, Duration::ZERO);
    r.reg.publish("demo", "1.0.0", b"tgz");
    assert_eq!(r.get("/demo", Some(ABBREVIATED)).await.0, StatusCode::OK);
    r.reg.down.store(true, Ordering::Relaxed);
    let (s, _, body) = r.get("/demo", Some(ABBREVIATED)).await;
    assert_eq!(s, StatusCode::OK, "stale beats an error");
    assert!(String::from_utf8(body)
        .unwrap()
        .contains("faber-mirror:7470/npm/demo/-/"));
    // Nothing cached to fall back on: the registry's error.
    assert_eq!(r.get("/other", None).await.0, StatusCode::BAD_GATEWAY);
    r.reg.down.store(false, Ordering::Relaxed);
    assert_eq!(r.get("/missing", None).await.0, StatusCode::NOT_FOUND);
    assert_eq!(r.get("/missing", None).await.0, StatusCode::NOT_FOUND);
    // The fake counts the packuments it answered: /demo once, /missing twice.
    assert_eq!(
        r.reg.packuments.load(Ordering::Relaxed),
        3,
        "a 404 is asked again"
    );
}

// --- tarballs ------------------------------------------------------------------------

#[tokio::test]
async fn a_tarball_is_kept_only_by_its_integrity_and_hit_afterwards() {
    let r = rig("fill", 1 << 20, TTL);
    let tgz = b"a tarball's bytes".repeat(100);
    r.reg.publish("demo", "1.0.0", &tgz);
    r.get("/demo", Some(ABBREVIATED)).await;
    let (s, h, body) = r.get(&tarball_path("demo", "1.0.0"), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(body, tgz);
    assert_eq!(h[header::CONTENT_LENGTH], tgz.len().to_string());
    assert_eq!(r.store.has(&key_of(&tgz)), Some(tgz.len() as u64));
    let (_, _, again) = r.get(&tarball_path("demo", "1.0.0"), None).await;
    assert_eq!(again, tgz);
    assert_eq!(
        r.reg.tarballs.load(Ordering::Relaxed),
        1,
        "the second was a hit"
    );
    assert_eq!(
        r.reg.packuments.load(Ordering::Relaxed),
        1,
        "the install's own packument served the lookup"
    );
    let c = r.counters();
    // packument miss, tarball miss, tarball hit.
    assert_eq!((c.hits, c.misses, c.passed_through), (1, 2, 0));
    assert_eq!(c.bytes_from_cache, tgz.len() as u64);
    assert_eq!(r.store.stats().usage["npm"].objects, 1);
}

#[tokio::test]
async fn a_lockfile_install_asks_only_for_tarballs_and_a_restart_needs_no_packument() {
    let r = rig("lockfile", 1 << 20, TTL);
    let tgz = b"locked".to_vec();
    r.reg.publish("@s/locked", "2.0.0", &tgz);
    // `npm ci`: no packument request; the mirror asks for one itself.
    let (s, _, body) = r.get("/@s%2flocked/-/locked-2.0.0.tgz", None).await;
    assert_eq!((s, body), (StatusCode::OK, tgz.clone()));
    assert_eq!(r.reg.packuments.load(Ordering::Relaxed), 1);
    // A restarted service: the recorded hash finds the stored tarball, with no
    // packument and no download.
    let dir = r.dir.clone();
    let reg = r.reg.clone();
    std::mem::forget(r);
    let r = rig_in(dir, reg, 1 << 20, TTL);
    let (s, _, body) = r.get("/@s/locked/-/locked-2.0.0.tgz", None).await;
    assert_eq!((s, body), (StatusCode::OK, tgz));
    assert_eq!(r.reg.packuments.load(Ordering::Relaxed), 1);
    assert_eq!(r.reg.tarballs.load(Ordering::Relaxed), 1);
    assert_eq!(r.counters().hits, 1);
}

#[tokio::test]
async fn a_tarball_that_does_not_match_its_integrity_is_neither_served_nor_kept() {
    let r = rig("mismatch", 1 << 20, TTL);
    r.reg.publish("demo", "1.0.0", b"the real one");
    let path = tarball_path("demo", "1.0.0");
    r.reg
        .corrupt
        .lock()
        .unwrap()
        .insert(path.clone(), b"a swapped one".to_vec());
    let (s, _, body) = r.get(&path, None).await;
    assert_eq!(s, StatusCode::BAD_GATEWAY);
    assert!(String::from_utf8(body)
        .unwrap()
        .contains("not the packument's"));
    assert_eq!(r.store.stats().objects, 0);
    assert!(
        std::fs::read_dir(r.dir.join("store/tmp"))
            .unwrap()
            .next()
            .is_none(),
        "no temp left"
    );
    // Streamed uncached (host low on disk): cut off before its end. Read as
    // hyper writes it, chunk by chunk: fewer bytes than the Content-Length it
    // declared, then an error, so a client sees a truncated download.
    r.sync(Some(Fill::Relaxed), true);
    let req = Request::get(format!("/npm{path}"))
        .body(Body::empty())
        .unwrap();
    let res = r.app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        res.status(),
        StatusCode::OK,
        "the status went before the bytes"
    );
    let declared: usize = res.headers()[header::CONTENT_LENGTH]
        .to_str()
        .unwrap()
        .parse()
        .unwrap();
    let mut chunks = res.into_body().into_data_stream();
    let (mut sent, mut failed) = (0, false);
    while let Some(c) = chunks.next().await {
        match c {
            Ok(b) => sent += b.len(),
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    assert!(failed, "the stream ends in an error");
    assert!(
        sent < declared,
        "{sent} of {declared}: the last chunk was held back"
    );
    assert_eq!(r.store.stats().objects, 0);
}

#[tokio::test]
async fn records_of_tarballs_no_longer_stored_are_pruned() {
    let r = rig("prune", 1 << 20, TTL);
    r.reg.publish("kept", "1.0.0", b"kept");
    r.reg.publish("gone", "1.0.0", b"gone");
    r.get(&tarball_path("gone", "1.0.0"), None).await;
    r.get(&tarball_path("kept", "1.0.0"), None).await;
    let records = || {
        std::fs::read_dir(r.dir.join("npm/tarballs"))
            .unwrap()
            .count()
    };
    assert_eq!(records(), 2);
    // The least recently used goes: "gone".
    r.store.evict_to(4);
    assert_eq!(r.store.stats().objects, 1);
    // A restarted adapter prunes at start, as it does every so many records.
    let npm = Npm::new(r.control.clone(), r.store.clone(), &r.dir, r.reg.clone());
    assert_eq!(records(), 1);
    assert_eq!(npm.prune(), 0, "nothing more to prune");
}

#[tokio::test]
async fn a_legacy_package_is_verified_by_its_sha1() {
    let r = rig("legacy", 1 << 20, TTL);
    r.reg.publish("old", "0.1.0", b"from 2012");
    r.reg.legacy.lock().unwrap().push("old".into());
    let (s, _, body) = r.get(&tarball_path("old", "0.1.0"), None).await;
    assert_eq!((s, body.as_slice()), (StatusCode::OK, &b"from 2012"[..]));
    assert_eq!(r.store.has(&sha1_hex(b"from 2012")), Some(9));
}

#[tokio::test]
async fn fill_off_serves_what_is_kept_and_keeps_nothing_new() {
    let r = rig("off", 1 << 20, TTL);
    r.reg.publish("kept", "1.0.0", b"kept");
    r.reg.publish("new", "1.0.0", b"new");
    r.get(&tarball_path("kept", "1.0.0"), None).await;
    r.sync(Some(Fill::Off), false);
    let before = r.reg.tarballs.load(Ordering::Relaxed);
    assert_eq!(r.get(&tarball_path("kept", "1.0.0"), None).await.2, b"kept");
    assert_eq!(r.reg.tarballs.load(Ordering::Relaxed), before, "a hit");
    assert_eq!(r.get(&tarball_path("new", "1.0.0"), None).await.2, b"new");
    assert!(r.store.has(&key_of(b"new")).is_none(), "passed through");
    assert_eq!(r.counters().passed_through, 1);
    // Packuments are fetched every time.
    let p = r.reg.packuments.load(Ordering::Relaxed);
    r.get("/new", Some(ABBREVIATED)).await;
    r.get("/new", Some(ABBREVIATED)).await;
    assert_eq!(r.reg.packuments.load(Ordering::Relaxed), p + 2);
}

#[tokio::test]
async fn a_tarball_larger_than_the_cap_is_streamed_not_kept() {
    let r = rig("cap", 10, TTL);
    r.reg.publish("big", "1.0.0", &[7u8; 64]);
    let (s, _, body) = r.get(&tarball_path("big", "1.0.0"), None).await;
    assert_eq!((s, body.len()), (StatusCode::OK, 64));
    assert_eq!(r.store.stats().objects, 0);
    assert_eq!(r.counters().passed_through, 1);
}

// --- pass-through -------------------------------------------------------------------

#[tokio::test]
async fn authorized_requests_publishes_and_other_endpoints_pass_through_uncached() {
    let r = rig("forward", 1 << 20, TTL);
    r.reg.publish("demo", "1.0.0", b"tgz");
    let path = tarball_path("demo", "1.0.0");
    for _ in 0..2 {
        let req = Request::get(format!("/npm{path}"))
            .header("authorization", "Bearer s3cret")
            .body(Body::empty())
            .unwrap();
        let (s, _, body) = r.send(req).await;
        assert_eq!(s, StatusCode::CREATED, "the registry's answer, as it came");
        assert!(String::from_utf8(body.unwrap())
            .unwrap()
            .contains("Bearer s3cret"));
    }
    assert_eq!(r.store.stats().objects, 0, "never kept");
    let put = Request::put("/npm/demo")
        .header("content-type", "application/json")
        .header("connection", "keep-alive")
        .body(Body::from("{\"_attachments\":{}}"))
        .unwrap();
    assert_eq!(r.send(put).await.0, StatusCode::CREATED);
    let (_, _, whoami) = r.get("/-/whoami", None).await;
    assert!(String::from_utf8(whoami).unwrap().contains("/-/whoami"));
    let search = Request::get("/npm/-/v1/search?text=demo")
        .body(Body::empty())
        .unwrap();
    assert_eq!(r.send(search).await.0, StatusCode::OK);
    let asked = r.reg.asked.lock().unwrap();
    let paths: Vec<(&str, &str)> = asked
        .iter()
        .map(|(m, p, _, _)| (m.as_str(), p.as_str()))
        .collect();
    assert_eq!(
        paths,
        vec![
            ("GET", path.as_str()),
            ("GET", path.as_str()),
            ("PUT", "/demo"),
            ("GET", "/-/whoami"),
            ("GET", "/-/v1/search?text=demo"),
        ]
    );
    assert_eq!(
        asked[2].3, b"{\"_attachments\":{}}",
        "the publish body, as sent"
    );
    assert_eq!(r.counters().passed_through, 5);
}

#[tokio::test]
async fn npm_off_is_not_served() {
    let r = rig("unserved", 1 << 20, TTL);
    r.sync(None, false);
    let (s, _, body) = r.get("/demo", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(String::from_utf8(body).unwrap().contains("not served"));
    assert!(r.reg.asked.lock().unwrap().is_empty());
}

// --- attribution ---------------------------------------------------------------------

#[tokio::test]
async fn requests_count_against_the_container_whose_network_they_came_on() {
    let r = rig("attribution", 1 << 20, TTL);
    r.reg.publish("demo", "1.0.0", b"tgz");
    r.get("/demo", None).await;
    assert_eq!(r.counters().misses, 1);
    // An address no sync named: counted, but against nobody.
    r.control.apply(SyncRequest {
        npm: Some(Fill::Relaxed),
        ..SyncRequest::default()
    });
    r.get("/demo", None).await;
    let s = r.control.stats();
    assert_eq!(s.npm[crate::UNATTRIBUTED].hits, 1);
    // A fault of the mirror's own is the container's.
    r.sync(Some(Fill::Relaxed), false);
    r.reg.local_fault.store(true, Ordering::Relaxed);
    assert_eq!(
        r.get("/-/whoami", None).await.0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    assert_eq!(r.control.stats().failures["faber-net-a"].count, 1);
    // A registry fault is not.
    r.reg.local_fault.store(false, Ordering::Relaxed);
    r.reg.down.store(true, Ordering::Relaxed);
    r.get("/-/whoami", None).await;
    assert_eq!(r.control.stats().failures["faber-net-a"].count, 1);
}
