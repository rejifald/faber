//! The LFS adapter against an in-house fake of GitHub's LFS batch API. Never the
//! real one.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use sha2::{Digest, Sha256};
use tower::ServiceExt;

use super::*;
use crate::{SyncEntry, SyncRequest};

/// The host the fake's download links name: one of GitHub's real LFS hosts,
/// so the adapter's link check lets them through.
const FAKE_HOST: &str = "https://github-cloud.githubusercontent.com/";

/// GitHub as the adapter sees it: which repo has which object, and what a
/// download of it sends (normally its bytes; a corrupt one sends others).
/// Like GitHub, it answers a download only for the object's true size, and
/// `422` for any other.
#[derive(Default)]
pub(crate) struct FakeGithub {
    has: Mutex<HashMap<(String, String), Vec<u8>>>,
    corrupt: Mutex<HashMap<String, Vec<u8>>>,
    pub batches: Mutex<Vec<String>>,
    pub downloads: AtomicUsize,
    pub local_fault: Mutex<bool>,
    /// A misbehaving upstream: the download link it offers instead.
    pub href: Mutex<Option<String>>,
    /// A misbehaving upstream: the size it claims instead of the true one.
    pub claims: Mutex<Option<u64>>,
}

impl FakeGithub {
    pub(crate) fn put(&self, repo: &str, bytes: &[u8]) -> String {
        let oid = oid_of(bytes);
        self.has
            .lock()
            .unwrap()
            .insert((repo.into(), oid.clone()), bytes.to_vec());
        oid
    }
}

pub(crate) fn oid_of(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

impl Upstream for Arc<FakeGithub> {
    async fn batch(
        &self,
        repo: &str,
        objects: &[ObjectSpec],
    ) -> Result<Vec<Confirmed>, UpstreamError> {
        if *self.local_fault.lock().unwrap() {
            return Err(UpstreamError::Local("credential unreadable".into()));
        }
        self.batches.lock().unwrap().push(repo.to_string());
        let has = self.has.lock().unwrap();
        Ok(objects
            .iter()
            .map(|o| match has.get(&(repo.to_string(), o.oid.clone())) {
                Some(bytes) if bytes.len() as u64 != o.size => Confirmed {
                    oid: o.oid.clone(),
                    size: o.size,
                    answer: Answer::Error {
                        code: 422,
                        message: format!("Object {} is not {} bytes", o.oid, o.size),
                    },
                },
                Some(bytes) => Confirmed {
                    oid: o.oid.clone(),
                    size: self.claims.lock().unwrap().unwrap_or(bytes.len() as u64),
                    answer: Answer::Download(Action {
                        href: self
                            .href
                            .lock()
                            .unwrap()
                            .clone()
                            .unwrap_or_else(|| format!("{FAKE_HOST}{repo}/{}", o.oid)),
                        header: [("Authorization".to_string(), "RemoteAuth x".to_string())].into(),
                    }),
                },
                None => Confirmed {
                    oid: o.oid.clone(),
                    size: o.size,
                    answer: Answer::Error {
                        code: 404,
                        message: "Object does not exist".into(),
                    },
                },
            })
            .collect())
    }

    async fn download(&self, action: &Action) -> Result<ByteStream, UpstreamError> {
        self.downloads.fetch_add(1, Ordering::Relaxed);
        let rest = action.href.strip_prefix(FAKE_HOST).unwrap();
        let (repo, oid) = rest.rsplit_once('/').unwrap();
        let bytes = match self.corrupt.lock().unwrap().get(oid) {
            Some(b) => b.clone(),
            None => self
                .has
                .lock()
                .unwrap()
                .get(&(repo.to_string(), oid.to_string()))
                .cloned()
                .ok_or(UpstreamError::Upstream {
                    status: 404,
                    message: "gone".into(),
                })?,
        };
        // Two chunks, so the fill really streams.
        let mid = bytes.len() / 2;
        let chunks = vec![
            Ok(Bytes::from(bytes[..mid].to_vec())),
            Ok(Bytes::from(bytes[mid..].to_vec())),
        ];
        Ok(Box::pin(futures_util::stream::iter(chunks)))
    }
}

pub(crate) struct Rig {
    pub dir: PathBuf,
    pub gh: Arc<FakeGithub>,
    pub control: Arc<Control>,
    pub store: Arc<Store>,
    pub app: Router,
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

pub(crate) fn rig(tag: &str, max_bytes: u64) -> Rig {
    let dir = std::env::temp_dir().join(format!("faber-mirror-lfs-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let gh = Arc::new(FakeGithub::default());
    let control = Arc::new(Control::new());
    let store = Arc::new(Store::open(&dir.join("store"), max_bytes));
    let lfs = Lfs::new(control.clone(), store.clone(), gh.clone());
    let npm = crate::npm::Npm::new(
        control.clone(),
        store.clone(),
        &dir,
        crate::npm::tests::FakeRegistry::default().shared(),
    );
    let registry = crate::builtin(lfs, npm);
    let service = crate::Service::new("t", &dir, control.clone(), store.clone());
    let app = crate::router(&registry, service);
    control.apply(SyncRequest {
        entries: vec![
            SyncEntry {
                id: "faber-net-a".into(),
                repo: "owner/demo".into(),
                token: Some("tok-a".into()),
                addr: None,
            },
            SyncEntry {
                id: "faber-net-b".into(),
                repo: "owner/other".into(),
                token: Some("tok-b".into()),
                addr: None,
            },
        ],
        lfs: Some(Fill::Relaxed),
        ..SyncRequest::default()
    });
    Rig {
        dir,
        gh,
        control,
        store,
        app,
    }
}

impl Rig {
    /// The size a well-behaved git-lfs sends: the object's true size, from its
    /// pointer file.
    fn true_size(&self, oid: &str) -> u64 {
        self.gh
            .has
            .lock()
            .unwrap()
            .iter()
            .find(|((_, o), _)| o == oid)
            .map_or(1, |(_, b)| b.len() as u64)
    }

    pub(crate) async fn batch(
        &self,
        token: &str,
        repo: &str,
        op: &str,
        oid: &str,
    ) -> (StatusCode, serde_json::Value) {
        let size = self.true_size(oid);
        self.batch_sized(token, repo, op, oid, size).await
    }

    pub(crate) async fn batch_sized(
        &self,
        token: &str,
        repo: &str,
        op: &str,
        oid: &str,
        size: u64,
    ) -> (StatusCode, serde_json::Value) {
        let body = serde_json::json!({"operation": op, "objects": [{"oid": oid, "size": size}]});
        let res = self
            .app
            .clone()
            .oneshot(
                Request::post(format!("/lfs/{token}/{repo}/info/lfs/objects/batch"))
                    .header("host", "faber-mirror:7470")
                    .header("content-type", LFS_JSON)
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    pub(crate) async fn get(&self, token: &str, repo: &str, oid: &str) -> (StatusCode, Vec<u8>) {
        let res = self
            .app
            .clone()
            .oneshot(
                Request::get(format!("/lfs/{token}/{repo}/info/lfs/objects/{oid}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 24)
            .await
            .unwrap();
        (status, bytes.to_vec())
    }

    /// What git-lfs does: ask the batch API, then `GET` the link.
    pub(crate) async fn fetch(&self, token: &str, repo: &str, oid: &str) -> (StatusCode, Vec<u8>) {
        self.batch(token, repo, "download", oid).await;
        self.get(token, repo, oid).await
    }

    fn counters(&self, repo: &str) -> crate::RepoCounters {
        self.control
            .stats()
            .repos
            .get(repo)
            .cloned()
            .unwrap_or_default()
    }

    fn tmp_is_empty(&self) -> bool {
        std::fs::read_dir(self.dir.join("store").join("tmp"))
            .unwrap()
            .next()
            .is_none()
    }
}

#[tokio::test]
async fn a_miss_is_filled_once_and_then_served_from_the_store() {
    let r = rig("hit", 1 << 20);
    let bytes = b"baseline png bytes".repeat(100);
    let oid = r.gh.put("owner/demo", &bytes);

    let (status, answer) = r.batch("tok-a", "owner/demo", "download", &oid).await;
    assert_eq!(status, StatusCode::OK);
    let href = answer["objects"][0]["actions"]["download"]["href"]
        .as_str()
        .unwrap();
    assert_eq!(
        href,
        format!("http://faber-mirror:7470/lfs/tok-a/owner/demo/info/lfs/objects/{oid}")
    );
    assert!(
        !answer.to_string().contains("RemoteAuth"),
        "upstream's headers never reach the job"
    );

    let (status, got) = r.get("tok-a", "owner/demo", &oid).await;
    assert_eq!((status, got.as_slice()), (StatusCode::OK, bytes.as_slice()));
    assert_eq!(r.store.has(&oid), Some(bytes.len() as u64));
    let c = r.counters("owner/demo");
    assert_eq!(
        (c.misses, c.hits, c.bytes_from_upstream),
        (1, 0, bytes.len() as u64)
    );

    // A hit: no upstream batch, no download.
    let batches = r.gh.batches.lock().unwrap().len();
    let (status, answer) = r.batch("tok-a", "owner/demo", "download", &oid).await;
    assert_eq!(status, StatusCode::OK);
    assert!(answer["objects"][0]["actions"]["download"]["href"].is_string());
    let (status, got) = r.get("tok-a", "owner/demo", &oid).await;
    assert_eq!((status, got.as_slice()), (StatusCode::OK, bytes.as_slice()));
    assert_eq!(r.gh.batches.lock().unwrap().len(), batches);
    assert_eq!(r.gh.downloads.load(Ordering::Relaxed), 1);
    let c = r.counters("owner/demo");
    assert_eq!((c.hits, c.bytes_from_cache), (1, bytes.len() as u64));
    assert!(r.tmp_is_empty());
}

#[tokio::test]
async fn an_object_upstream_lacks_is_its_error_not_the_mirrors() {
    let r = rig("absent", 1 << 20);
    let oid = oid_of(b"never pushed");
    let (status, answer) = r.batch("tok-a", "owner/demo", "download", &oid).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(answer["objects"][0]["error"]["code"], 404);
    let (status, _) = r.get("tok-a", "owner/demo", &oid).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(
        r.control.stats().failures.is_empty(),
        "not a mirror failure"
    );
}

#[tokio::test]
async fn bytes_that_do_not_hash_to_the_oid_are_refused_and_not_stored() {
    let r = rig("sha", 1 << 20);
    let oid = r.gh.put("owner/demo", b"the real object");
    r.gh.corrupt
        .lock()
        .unwrap()
        .insert(oid.clone(), b"something else!".to_vec());
    r.batch("tok-a", "owner/demo", "download", &oid).await;
    let (status, body) = r.get("tok-a", "owner/demo", &oid).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(String::from_utf8_lossy(&body).contains("not stored"));
    assert_eq!(r.store.has(&oid), None);
    assert!(!r.store.indexed("owner/demo", &oid));
    assert!(r.tmp_is_empty(), "the temp file is gone");

    // A longer body than the batch promised is cut off, and refused too.
    r.gh.corrupt
        .lock()
        .unwrap()
        .insert(oid.clone(), b"the real object, and then some".to_vec());
    let (status, _) = r.get("tok-a", "owner/demo", &oid).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(r.store.has(&oid), None);
}

#[tokio::test]
async fn an_object_stored_for_another_repo_is_served_only_after_upstream_confirms_it() {
    let r = rig("cross", 1 << 20);
    let bytes = b"shared baseline";
    let oid = r.gh.put("owner/demo", bytes);
    r.batch("tok-a", "owner/demo", "download", &oid).await;
    assert_eq!(r.get("tok-a", "owner/demo", &oid).await.0, StatusCode::OK);
    assert!(r.store.has(&oid).is_some());

    // owner/other guesses the hash. GitHub says it does not have it: refused,
    // though the bytes are right here.
    let (_, answer) = r.batch("tok-b", "owner/other", "download", &oid).await;
    assert_eq!(answer["objects"][0]["error"]["code"], 404);
    let (status, body) = r.get("tok-b", "owner/other", &oid).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_ne!(body, bytes);
    assert_eq!(
        r.gh.batches.lock().unwrap().last().map(String::as_str),
        Some("owner/other"),
        "checked upstream as the asking repo"
    );

    // Once GitHub confirms owner/other has it, the stored bytes serve it with no
    // second download.
    r.gh.put("owner/other", bytes);
    let downloads = r.gh.downloads.load(Ordering::Relaxed);
    let (status, got) = r.get("tok-b", "owner/other", &oid).await;
    assert_eq!((status, got.as_slice()), (StatusCode::OK, &bytes[..]));
    assert_eq!(r.gh.downloads.load(Ordering::Relaxed), downloads);
    assert!(r.store.indexed("owner/other", &oid));
}

#[tokio::test]
async fn a_token_speaks_for_its_own_repo_only() {
    let r = rig("token", 1 << 20);
    let oid = r.gh.put("owner/other", b"theirs");
    let (status, answer) = r.batch("tok-a", "owner/other", "download", &oid).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(answer["message"]
        .as_str()
        .unwrap()
        .contains("not for this repository"));
    assert_eq!(
        r.get("tok-a", "owner/other", &oid).await.0,
        StatusCode::FORBIDDEN
    );
    let (status, _) = r.batch("forged", "owner/demo", "download", &oid).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(
        r.gh.batches.lock().unwrap().is_empty(),
        "nothing reached upstream"
    );

    // Revoked at teardown: the next sync leaves it out.
    r.control.apply(SyncRequest {
        lfs: Some(Fill::Relaxed),
        ..SyncRequest::default()
    });
    assert_eq!(
        r.batch("tok-b", "owner/other", "download", &oid).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn an_upload_is_refused_with_where_pushes_go() {
    let r = rig("upload", 1 << 20);
    let (status, answer) = r
        .batch("tok-a", "owner/demo", "upload", &oid_of(b"new"))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(answer["message"].as_str().unwrap().contains("lfs.pushurl"));
}

#[tokio::test]
async fn lfs_is_not_served_until_the_daemon_turns_it_on() {
    let r = rig("off", 1 << 20);
    r.control.apply(SyncRequest::default());
    let (status, _) = r
        .batch("tok-a", "owner/demo", "download", &oid_of(b"x"))
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_cap_evicts_and_an_evicted_object_is_filled_again() {
    let r = rig("evict", 25);
    let a = r.gh.put("owner/demo", b"0123456789");
    let b = r.gh.put("owner/demo", b"abcdefghij");
    let c = r.gh.put("owner/demo", b"ABCDEFGHIJ");
    for oid in [&a, &b, &c] {
        assert_eq!(r.fetch("tok-a", "owner/demo", oid).await.0, StatusCode::OK);
    }
    assert_eq!(r.store.has(&a), None, "the least recently used went");
    assert!(r.store.stats().bytes <= 25);
    let (status, got) = r.fetch("tok-a", "owner/demo", &a).await;
    assert_eq!(
        (status, got.as_slice()),
        (StatusCode::OK, &b"0123456789"[..])
    );
    assert_eq!(r.counters("owner/demo").misses, 4);

    // Larger than the whole cap: verified and served, never stored.
    let big = r.gh.put("owner/demo", &[7u8; 100]);
    let (status, got) = r.fetch("tok-a", "owner/demo", &big).await;
    assert_eq!((status, got.len()), (StatusCode::OK, 100));
    assert_eq!(r.store.has(&big), None);
    assert!(r.tmp_is_empty());
}

#[tokio::test]
async fn passing_through_serves_a_miss_without_storing_it() {
    let r = rig("pass", 1 << 20);
    let kept = r.gh.put("owner/demo", b"kept before the disk ran low");
    assert_eq!(
        r.fetch("tok-a", "owner/demo", &kept).await.0,
        StatusCode::OK
    );

    let mut low = SyncRequest {
        entries: vec![SyncEntry {
            id: "faber-net-a".into(),
            repo: "owner/demo".into(),
            token: None,
            addr: None,
        }],
        lfs: Some(Fill::Relaxed),
        npm: None,
        low_disk: true,
        max_size_bytes: None,
        adopt: false,
    };
    r.control.apply(low.clone());
    let oid = r.gh.put("owner/demo", b"fetched while low");
    let (status, got) = r.fetch("tok-a", "owner/demo", &oid).await;
    assert_eq!(
        (status, got.as_slice()),
        (StatusCode::OK, &b"fetched while low"[..])
    );
    assert_eq!(r.store.has(&oid), None, "not stored");
    let c = r.counters("owner/demo");
    assert_eq!(c.passed_through, 1);
    assert_eq!(c.bytes_from_upstream, 28 + 17);
    // Hits still come from the store.
    assert_eq!(r.get("tok-a", "owner/demo", &kept).await.0, StatusCode::OK);
    assert_eq!(r.counters("owner/demo").hits, 1);

    // fill = "off" passes through the same way.
    low.low_disk = false;
    low.lfs = Some(Fill::Off);
    r.control.apply(low);
    let off = r.gh.put("owner/demo", b"fetched with fill off");
    assert_eq!(r.fetch("tok-a", "owner/demo", &off).await.0, StatusCode::OK);
    assert_eq!(r.store.has(&off), None);
}

#[tokio::test]
async fn a_fault_of_the_mirrors_own_is_recorded_against_the_container() {
    let r = rig("fault", 1 << 20);
    *r.gh.local_fault.lock().unwrap() = true;
    let (status, _) = r
        .batch("tok-a", "owner/demo", "download", &oid_of(b"x"))
        .await;
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    let f = r.control.stats().failures;
    assert_eq!(f["faber-net-a"].count, 1);
    assert!(f["faber-net-a"].last.contains("credential"));
}

#[test]
fn an_upstream_batch_answer_is_parsed_into_downloads_and_errors() {
    let body = br#"{"objects":[
        {"oid":"a","size":3,"actions":{"download":{"href":"https://x/a","header":{"Authorization":"RemoteAuth t"}}}},
        {"oid":"b","size":4,"error":{"code":404,"message":"Object does not exist"}},
        {"oid":"c","size":5}
    ]}"#;
    let got = parse_batch(body).unwrap();
    assert!(matches!(&got[0].answer, Answer::Download(a) if a.href == "https://x/a"));
    assert!(matches!(&got[1].answer, Answer::Error { code: 404, .. }));
    assert!(matches!(&got[2].answer, Answer::Error { code: 404, .. }));
    assert!(parse_batch(b"<html>").is_err());
}

// --- what the mirror will follow, and what size it trusts ---------------------------

#[tokio::test]
async fn a_download_link_off_githubs_lfs_hosts_is_never_followed() {
    for href in [
        "http://github-cloud.githubusercontent.com/owner/demo/x",
        "https://evil.example/owner/demo/x",
        "https://github-cloud.githubusercontent.com:8443/owner/demo/x",
        "https://10.0.0.5/owner/demo/x",
        "https://169.254.169.254/latest/meta-data",
        "https://[::1]/x",
        "http://faber-mirror:7470/healthz",
        "http://172.18.0.3:5432/",
    ] {
        let r = rig("egress", 1 << 20);
        let oid = r.gh.put("owner/demo", b"an object");
        *r.gh.href.lock().unwrap() = Some(href.to_string());
        let (status, body) = r.fetch("tok-a", "owner/demo", &oid).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY, "{href}");
        assert!(
            String::from_utf8_lossy(&body).contains("not following"),
            "{href}: {}",
            String::from_utf8_lossy(&body)
        );
        assert_eq!(
            r.gh.downloads.load(Ordering::Relaxed),
            0,
            "{href}: fetched anyway"
        );
        assert_eq!(r.store.has(&oid), None);
    }
}

#[tokio::test]
async fn a_job_supplied_size_never_reaches_the_store_or_another_job() {
    let r = rig("size", 1 << 20);
    let bytes = b"the object, 24 bytes...!";
    let oid = r.gh.put("owner/demo", bytes);
    // A job lies about the size: GitHub refuses it, and nothing is remembered.
    let (_, answer) = r
        .batch_sized("tok-a", "owner/demo", "download", &oid, 1 << 40)
        .await;
    assert_eq!(answer["objects"][0]["error"]["code"], 422);
    let (_, answer) = r
        .batch_sized("tok-a", "owner/demo", "download", &oid, 3)
        .await;
    assert_eq!(answer["objects"][0]["error"]["code"], 422);
    assert_eq!(
        r.get("tok-a", "owner/demo", &oid).await.0,
        StatusCode::NOT_FOUND,
        "a GET with no batch answer behind it does not use the job's size"
    );
    // The next job asks with the true size and is served in full.
    let (status, got) = r.fetch("tok-a", "owner/demo", &oid).await;
    assert_eq!((status, got.as_slice()), (StatusCode::OK, &bytes[..]));
    assert_eq!(r.store.has(&oid), Some(bytes.len() as u64));
}

#[tokio::test]
async fn an_upstream_size_of_zero_or_a_wrong_one_is_refused() {
    let r = rig("zero", 1 << 20);
    let oid = r.gh.put("owner/demo", b"some bytes");
    *r.gh.claims.lock().unwrap() = Some(0);
    let (_, answer) = r.batch("tok-a", "owner/demo", "download", &oid).await;
    assert_eq!(answer["objects"][0]["error"]["code"], 502);
    assert!(answer["objects"][0]["actions"].is_null());

    // A size smaller than the bytes: the download is cut off there, nothing
    // is kept, and the temp file goes.
    *r.gh.claims.lock().unwrap() = Some(4);
    let (status, _) = r.fetch("tok-a", "owner/demo", &oid).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert_eq!(r.store.has(&oid), None);
    assert!(r.tmp_is_empty());
    assert_eq!(r.store.stats().in_flight, 0);
}
