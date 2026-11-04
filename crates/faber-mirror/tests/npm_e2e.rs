//! The real `npm` client against the service on localhost, with an in-house
//! fake of the registry behind it. Never the real registry.
//!
//! The client is configured exactly as faber configures a runner: through
//! `NPM_CONFIG_REGISTRY`. Every other way out is closed: npm's proxies point at
//! a port nothing listens on, so a request the mirror did not get (a lockfile
//! URL npm failed to rewrite, say) fails instead of reaching the internet.
//!
//! It needs `npm` on `PATH`. Under `CI` a missing one fails the test. Run
//! locally without it, it says so on stderr and passes without running; the
//! adapter's own unit tests still cover the behaviour.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::http::{header, HeaderMap, Method, StatusCode};
use base64::Engine as _;
use faber_mirror::lfs::{Action, ByteStream, Confirmed, ObjectSpec, UpstreamError};
use faber_mirror::npm::{Ask, Reply, Target};
use faber_mirror::{Fill, SyncEntry, SyncRequest};
use sha2::{Digest, Sha512};

/// The packument lifetime the service runs with here: short, so the test sees
/// one expire.
const TTL: Duration = Duration::from_secs(1);

/// name → (the tarball its packument names, the bytes a download sends).
type Packages = HashMap<String, (Vec<u8>, Vec<u8>)>;

#[derive(Default)]
struct FakeRegistry {
    packages: Mutex<Packages>,
    packuments: AtomicUsize,
    tarballs: AtomicUsize,
    /// The `Authorization` of every request that carried one.
    auth: Mutex<Vec<String>>,
}

struct Shared(Arc<FakeRegistry>);

fn reply(status: StatusCode, content_type: &str, body: Vec<u8>) -> Reply {
    let mut headers = HeaderMap::new();
    headers.insert(header::CONTENT_TYPE, content_type.parse().unwrap());
    headers.insert(header::CONTENT_LENGTH, body.len().into());
    Reply {
        status,
        headers,
        body: Box::pin(futures_util::stream::iter(vec![Ok(Bytes::from(body))])),
    }
}

impl faber_mirror::npm::Upstream for Shared {
    async fn send(&self, ask: Ask) -> Result<Reply, UpstreamError> {
        if let Some(a) = ask.headers.get(header::AUTHORIZATION) {
            self.0.auth.lock().unwrap().push(a.to_str().unwrap().into());
        }
        if ask.method != Method::GET {
            return Ok(reply(StatusCode::OK, "application/json", b"{}".to_vec()));
        }
        let packages = self.0.packages.lock().unwrap();
        match faber_mirror::npm::classify(&ask.path) {
            Target::Packument { name } => {
                self.0.packuments.fetch_add(1, Ordering::Relaxed);
                let Some((tgz, _)) = packages.get(&name) else {
                    return Ok(reply(
                        StatusCode::NOT_FOUND,
                        "application/json",
                        b"{}".to_vec(),
                    ));
                };
                let integrity = format!(
                    "sha512-{}",
                    base64::engine::general_purpose::STANDARD.encode(Sha512::digest(tgz))
                );
                let doc = serde_json::json!({
                    "name": name,
                    "dist-tags": {"latest": "1.0.0"},
                    "versions": {"1.0.0": {
                        "name": name,
                        "version": "1.0.0",
                        "dist": {
                            "tarball": format!("{}/{name}/-/{name}-1.0.0.tgz", faber_mirror::NPMJS),
                            "integrity": integrity,
                        },
                    }},
                });
                Ok(reply(
                    StatusCode::OK,
                    faber_mirror::npm::ABBREVIATED,
                    doc.to_string().into_bytes(),
                ))
            }
            Target::Tarball { name, .. } => {
                self.0.tarballs.fetch_add(1, Ordering::Relaxed);
                match packages.get(&name) {
                    Some((_, served)) => Ok(reply(
                        StatusCode::OK,
                        "application/octet-stream",
                        served.clone(),
                    )),
                    None => Ok(reply(
                        StatusCode::NOT_FOUND,
                        "application/json",
                        b"{}".to_vec(),
                    )),
                }
            }
            Target::Other => Ok(reply(
                StatusCode::NOT_FOUND,
                "application/json",
                b"{}".to_vec(),
            )),
        }
    }
}

/// LFS is not under test here, and is never reached.
struct NoGithub;

impl faber_mirror::lfs::Upstream for NoGithub {
    async fn batch(&self, _: &str, _: &[ObjectSpec]) -> Result<Vec<Confirmed>, UpstreamError> {
        Err(UpstreamError::Local("LFS is not under test".into()))
    }
    async fn download(&self, _: &Action) -> Result<ByteStream, UpstreamError> {
        Err(UpstreamError::Local("LFS is not under test".into()))
    }
}

fn have_npm() -> bool {
    Command::new("npm")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

struct Npm {
    root: PathBuf,
    registry: String,
    runs: AtomicUsize,
}

impl Npm {
    /// Run npm in `dir` with a fresh cache of its own, so every run fetches
    /// through the mirror rather than from npm's cache.
    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        let n = self.runs.fetch_add(1, Ordering::Relaxed);
        let cache = self.root.join(format!("npm-cache-{n}"));
        Command::new("npm")
            .args(args)
            .current_dir(dir)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", self.root.join("home"))
            .env("npm_config_userconfig", self.root.join("home/.npmrc"))
            .env("npm_config_cache", &cache)
            // As faber injects it.
            .env("NPM_CONFIG_REGISTRY", &self.registry)
            // Nothing else gets out.
            .env("npm_config_proxy", "http://127.0.0.1:9")
            .env("npm_config_https_proxy", "http://127.0.0.1:9")
            .env("npm_config_noproxy", "127.0.0.1")
            .env("npm_config_audit", "false")
            .env("npm_config_fund", "false")
            .env("npm_config_update_notifier", "false")
            .env("npm_config_fetch_retries", "0")
            .output()
            .expect("running npm")
    }

    fn ok(&self, dir: &Path, args: &[&str]) {
        let out = self.run(dir, args);
        assert!(
            out.status.success(),
            "npm {args:?} failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("faber-mirror-npm-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    for d in ["home", "src", "app", "bad", "authed", "expiry"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    std::fs::write(dir.join("home/.npmrc"), "").unwrap();
    dir
}

/// A real package tarball, as `npm pack` makes it.
fn pack(npm: &Npm, name: &str, body: &str) -> Vec<u8> {
    let src = npm.root.join("src").join(name);
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(
        src.join("package.json"),
        format!(r#"{{"name":"{name}","version":"1.0.0","main":"index.js"}}"#),
    )
    .unwrap();
    std::fs::write(src.join("index.js"), body).unwrap();
    npm.ok(&src, &["pack", "--pack-destination", "."]);
    std::fs::read(src.join(format!("{name}-1.0.0.tgz"))).unwrap()
}

fn depend_on(dir: &Path, name: &str) {
    std::fs::write(
        dir.join("package.json"),
        format!(r#"{{"name":"app","version":"0.0.0","dependencies":{{"{name}":"1.0.0"}}}}"#),
    )
    .unwrap();
}

fn resolved(dir: &Path, name: &str) -> String {
    let lock: serde_json::Value =
        serde_json::from_slice(&std::fs::read(dir.join("package-lock.json")).unwrap()).unwrap();
    lock["packages"][format!("node_modules/{name}")]["resolved"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_npm_client_installs_through_the_mirror() {
    if !have_npm() {
        // In CI a missing npm is a broken runner setup, not a reason to pass:
        // every ci.yml job that runs this test sets up Node first.
        assert!(
            std::env::var_os("CI").is_none(),
            "npm must be on PATH in CI for the end-to-end npm test"
        );
        eprintln!("SKIPPED: npm is not on PATH, so the end-to-end npm test did not run");
        return;
    }
    let root = scratch();

    // The service, on a loopback port, in front of the fake. A connection to
    // it arrives on 127.0.0.1, which the sync names as owner/demo's network.
    let reg = Arc::new(FakeRegistry::default());
    let control = Arc::new(faber_mirror::control::Control::new());
    control.apply(SyncRequest {
        entries: vec![SyncEntry {
            id: "faber-net-e2e".into(),
            repo: "owner/demo".into(),
            token: None,
            addr: Some("127.0.0.1".into()),
        }],
        npm: Some(Fill::Relaxed),
        ..SyncRequest::default()
    });
    let cache = root.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    let store = Arc::new(faber_mirror::store::Store::open(
        &cache.join("store"),
        1 << 30,
    ));
    let lfs = faber_mirror::lfs::Lfs::new(control.clone(), store.clone(), NoGithub);
    let npm_adapter = faber_mirror::npm::Npm::with_ttl(
        control.clone(),
        store.clone(),
        &cache,
        Shared(reg.clone()),
        TTL,
    );
    let registry = faber_mirror::builtin(lfs, npm_adapter);
    let service = faber_mirror::Service::new("e2e", &cache, control.clone(), store.clone());
    let app = faber_mirror::router(&registry, service)
        .into_make_service_with_connect_info::<faber_mirror::Peer>();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let npm = Npm {
        root: root.clone(),
        registry: format!("http://127.0.0.1:{port}/npm/"),
        runs: AtomicUsize::new(0),
    };
    let demo = pack(&npm, "faber-e2e-demo", "module.exports = 'demo';\n");
    let bad = pack(&npm, "faber-e2e-bad", "module.exports = 'genuine';\n");
    {
        let mut p = reg.packages.lock().unwrap();
        p.insert("faber-e2e-demo".into(), (demo.clone(), demo.clone()));
        // Its packument names the genuine tarball; a download sends another.
        p.insert("faber-e2e-bad".into(), (bad.clone(), demo.clone()));
    }
    let demo_key: String = Sha512::digest(&demo)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let bad_key: String = Sha512::digest(&bad)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let counters = || {
        control
            .stats()
            .npm
            .get("owner/demo")
            .cloned()
            .unwrap_or_default()
    };

    // 1. A miss: the packument and the tarball are fetched upstream once, the
    //    tarball verified and kept.
    let app = root.join("app");
    depend_on(&app, "faber-e2e-demo");
    npm.ok(&app, &["install"]);
    assert!(app.join("node_modules/faber-e2e-demo/index.js").exists());
    assert_eq!(reg.tarballs.load(Ordering::Relaxed), 1);
    assert_eq!(store.has(&demo_key), Some(demo.len() as u64));
    let after_miss = counters();
    assert!(after_miss.misses >= 2, "{after_miss:?}");
    assert!(after_miss.bytes_from_upstream >= demo.len() as u64);

    // 2. The lockfile npm wrote names the mirror: npm records the registry it
    //    fetched from (docs/mirrors.md says so, for a job that commits one).
    let written = resolved(&app, "faber-e2e-demo");
    assert_eq!(
        written,
        format!("{}faber-e2e-demo/-/faber-e2e-demo-1.0.0.tgz", npm.registry)
    );

    // 3. A hit: `npm ci` from that lockfile, nothing cached in npm itself.
    std::fs::remove_dir_all(app.join("node_modules")).unwrap();
    npm.ok(&app, &["ci"]);
    assert!(app.join("node_modules/faber-e2e-demo/index.js").exists());
    assert_eq!(
        reg.tarballs.load(Ordering::Relaxed),
        1,
        "served from the store"
    );
    assert!(counters().hits > after_miss.hits);
    assert!(counters().bytes_from_cache >= demo.len() as u64);

    // 4. A lockfile naming the public registry (as one committed by a developer
    //    does): npm's default replace-registry-host=npmjs sends it to the
    //    mirror. Were it not rewritten, the dead proxy would fail the install.
    let lock = std::fs::read_to_string(app.join("package-lock.json")).unwrap();
    let public = lock.replace(
        &written,
        &format!(
            "{}/faber-e2e-demo/-/faber-e2e-demo-1.0.0.tgz",
            faber_mirror::NPMJS
        ),
    );
    assert!(public.contains("https://registry.npmjs.org/faber-e2e-demo/-/"));
    std::fs::write(app.join("package-lock.json"), public).unwrap();
    std::fs::remove_dir_all(app.join("node_modules")).unwrap();
    npm.ok(&app, &["ci"]);
    assert!(app.join("node_modules/faber-e2e-demo/index.js").exists());
    assert_eq!(
        reg.tarballs.load(Ordering::Relaxed),
        1,
        "through the mirror's store"
    );

    // 5. A tarball that does not match its packument's integrity: refused, the
    //    install fails, and nothing is kept for it.
    let bad_dir = root.join("bad");
    depend_on(&bad_dir, "faber-e2e-bad");
    let out = npm.run(&bad_dir, &["install"]);
    assert!(!out.status.success(), "npm installed a mismatched tarball");
    assert!(!bad_dir.join("node_modules/faber-e2e-bad/index.js").exists());
    assert_eq!(store.has(&bad_key), None);
    assert_eq!(
        store.stats().usage["npm"].objects,
        1,
        "only the demo tarball"
    );

    // 6. Authenticated: a token written for the mirror's own URL is sent, and
    //    the mirror forwards every such request as is, uncached, though it holds
    //    the tarball. (A token written for //registry.npmjs.org/ is not sent to
    //    the mirror at all: docs/mirrors.md tells those jobs to opt out.)
    let authed = root.join("authed");
    depend_on(&authed, "faber-e2e-demo");
    std::fs::write(
        authed.join(".npmrc"),
        format!("//127.0.0.1:{port}/npm/:_authToken=e2e-secret\n"),
    )
    .unwrap();
    let before = (
        reg.tarballs.load(Ordering::Relaxed),
        counters().passed_through,
    );
    npm.ok(&authed, &["install"]);
    assert!(authed.join("node_modules/faber-e2e-demo/index.js").exists());
    assert!(
        reg.auth
            .lock()
            .unwrap()
            .iter()
            .any(|a| a == "Bearer e2e-secret"),
        "the token reached upstream as sent"
    );
    assert_eq!(
        reg.tarballs.load(Ordering::Relaxed),
        before.0 + 1,
        "not served from the store"
    );
    assert!(
        counters().passed_through >= before.1 + 2,
        "packument and tarball passed through"
    );

    // 7. A packument past its lifetime is fetched again.
    let expiry = root.join("expiry");
    depend_on(&expiry, "faber-e2e-demo");
    tokio::time::sleep(TTL + Duration::from_millis(200)).await;
    let before = reg.packuments.load(Ordering::Relaxed);
    npm.ok(&expiry, &["install"]);
    assert_eq!(
        reg.packuments.load(Ordering::Relaxed),
        before + 1,
        "fetched once more"
    );
    let cached = reg.packuments.load(Ordering::Relaxed);
    std::fs::remove_dir_all(expiry.join("node_modules")).unwrap();
    std::fs::remove_file(expiry.join("package-lock.json")).unwrap();
    npm.ok(&expiry, &["install"]);
    if reg.packuments.load(Ordering::Relaxed) == cached {
        eprintln!("the second install ran within the TTL and was served the cached packument");
    }

    let _ = std::fs::remove_dir_all(&root);
}
