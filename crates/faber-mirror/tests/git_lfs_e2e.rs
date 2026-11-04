//! The real `git lfs` client against the service on localhost, with an in-house
//! fake of GitHub's LFS API behind it. Never the real GitHub.
//!
//! The client is configured exactly as faber configures a runner: through
//! `GIT_CONFIG_COUNT` / `GIT_CONFIG_KEY_n` / `GIT_CONFIG_VALUE_n`, with
//! `lfs.url` naming the mirror and `lfs.pushurl` naming GitHub.
//!
//! It needs `git` and `git-lfs` on `PATH`. Under `CI` a missing one fails the
//! test. Run locally without them, it says so on stderr and passes without
//! running; the adapter's own unit tests still cover the behaviour.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::body::Bytes;
use faber_mirror::lfs::{
    Action, Answer, ByteStream, Confirmed, ObjectSpec, Upstream, UpstreamError,
};
use faber_mirror::{Fill, SyncEntry, SyncRequest};
use sha2::{Digest, Sha256};

#[derive(Default)]
struct FakeGithub {
    has: Mutex<HashMap<(String, String), Vec<u8>>>,
    downloads: AtomicUsize,
}

/// The fake, shared with the test that inspects it.
struct Shared(Arc<FakeGithub>);

impl Upstream for Shared {
    async fn batch(
        &self,
        repo: &str,
        objects: &[ObjectSpec],
    ) -> Result<Vec<Confirmed>, UpstreamError> {
        let has = self.0.has.lock().unwrap();
        Ok(objects
            .iter()
            .map(|o| Confirmed {
                oid: o.oid.clone(),
                size: has
                    .get(&(repo.to_string(), o.oid.clone()))
                    .map_or(o.size, |b| b.len() as u64),
                answer: if has.contains_key(&(repo.to_string(), o.oid.clone())) {
                    Answer::Download(Action {
                        href: format!(
                            "https://github-cloud.githubusercontent.com/{repo}/{}",
                            o.oid
                        ),
                        header: Default::default(),
                    })
                } else {
                    Answer::Error {
                        code: 404,
                        message: "Object does not exist".into(),
                    }
                },
            })
            .collect())
    }

    async fn download(&self, action: &Action) -> Result<ByteStream, UpstreamError> {
        self.0.downloads.fetch_add(1, Ordering::Relaxed);
        let path = action
            .href
            .trim_start_matches("https://github-cloud.githubusercontent.com/");
        let (repo, oid) = path.rsplit_once('/').unwrap();
        let bytes = self.0.has.lock().unwrap()[&(repo.to_string(), oid.to_string())].clone();
        Ok(Box::pin(futures_util::stream::iter(vec![Ok(Bytes::from(
            bytes,
        ))])))
    }
}

/// npm is not under test here, and is never reached.
struct NoRegistry;

impl faber_mirror::npm::Upstream for NoRegistry {
    async fn send(
        &self,
        _: faber_mirror::npm::Ask,
    ) -> Result<faber_mirror::npm::Reply, UpstreamError> {
        Err(UpstreamError::Local("npm is not under test".into()))
    }
}

fn have(tool: &[&str]) -> bool {
    Command::new(tool[0])
        .args(&tool[1..])
        .output()
        .is_ok_and(|o| o.status.success())
}

struct Git {
    dir: PathBuf,
    home: PathBuf,
    lfs_url: String,
}

impl Git {
    fn run(&self, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.dir)
            .env_clear()
            .env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("HOME", &self.home)
            .env("XDG_CONFIG_HOME", &self.home)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_COUNT", "2")
            .env("GIT_CONFIG_KEY_0", "lfs.url")
            .env("GIT_CONFIG_VALUE_0", &self.lfs_url)
            .env("GIT_CONFIG_KEY_1", "lfs.pushurl")
            .env(
                "GIT_CONFIG_VALUE_1",
                "https://github.com/owner/demo.git/info/lfs",
            )
            .output()
            .expect("running git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }
}

fn scratch() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("faber-mirror-e2e-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("home")).unwrap();
    std::fs::create_dir_all(dir.join("repo")).unwrap();
    dir
}

fn write(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_git_lfs_client_pulls_through_the_mirror() {
    if !have(&["git", "--version"]) || !have(&["git", "lfs", "version"]) {
        // In CI a missing git-lfs is a broken runner setup, not a reason to
        // pass: the runner image has it, and ci.yml's setup checks for it.
        assert!(
            std::env::var_os("CI").is_none(),
            "git and git-lfs must both be on PATH in CI for the end-to-end LFS test"
        );
        eprintln!(
            "SKIPPED: git and git-lfs are not both on PATH, so the end-to-end test did not run"
        );
        return;
    }
    let root = scratch();

    // The service, on a loopback port, in front of the fake.
    let gh = Arc::new(FakeGithub::default());
    let object = b"a visual baseline, stored in LFS\n".repeat(500);
    let oid: String = Sha256::digest(&object)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    gh.has
        .lock()
        .unwrap()
        .insert(("owner/demo".into(), oid.clone()), object.clone());
    let control = Arc::new(faber_mirror::control::Control::new());
    control.apply(SyncRequest {
        entries: vec![SyncEntry {
            id: "faber-net-e2e".into(),
            repo: "owner/demo".into(),
            token: Some("e2etoken".into()),
            addr: None,
        }],
        lfs: Some(Fill::Relaxed),
        ..SyncRequest::default()
    });
    let cache = root.join("cache");
    std::fs::create_dir_all(&cache).unwrap();
    let store = Arc::new(faber_mirror::store::Store::open(
        &cache.join("store"),
        1 << 30,
    ));
    let lfs = faber_mirror::lfs::Lfs::new(control.clone(), store.clone(), Shared(gh.clone()));
    let npm = faber_mirror::npm::Npm::new(control.clone(), store.clone(), &cache, NoRegistry);
    let registry = faber_mirror::builtin(lfs, npm);
    let service = faber_mirror::Service::new("e2e", &cache, control.clone(), store.clone());
    let app = faber_mirror::router(&registry, service.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // A repo whose only commit holds an LFS pointer, as a checkout without LFS
    // leaves it.
    let git = Git {
        dir: root.join("repo"),
        home: root.join("home"),
        lfs_url: format!("http://127.0.0.1:{port}/lfs/e2etoken/owner/demo/info/lfs"),
    };
    git.run(&["init", "-q"]);
    write(
        &git.dir.join(".gitattributes"),
        b"*.png filter=lfs diff=lfs merge=lfs -text\n",
    );
    let pointer = format!(
        "version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
        object.len()
    );
    write(&git.dir.join("baseline.png"), pointer.as_bytes());
    git.run(&["add", "."]);
    git.run(&[
        "-c",
        "user.name=t",
        "-c",
        "user.email=t@example.com",
        "commit",
        "-q",
        "-m",
        "pointer",
    ]);
    git.run(&[
        "remote",
        "add",
        "origin",
        "https://github.com/owner/demo.git",
    ]);
    git.run(&["lfs", "install", "--local"]);

    // The env-injected config is what git-lfs uses.
    let env = git.run(&["lfs", "env"]);
    assert!(env.contains(&git.lfs_url), "{env}");

    // First pull: a miss, filled from the fake and verified.
    let git2 = &git;
    let pull = || {
        let dir = git2.dir.clone();
        let g = Git {
            dir,
            home: git2.home.clone(),
            lfs_url: git2.lfs_url.clone(),
        };
        tokio::task::spawn_blocking(move || g.run(&["lfs", "pull"]))
    };
    pull().await.unwrap();
    assert_eq!(std::fs::read(git.dir.join("baseline.png")).unwrap(), object);
    assert_eq!(gh.downloads.load(Ordering::Relaxed), 1);
    assert_eq!(store.has(&oid), Some(object.len() as u64));

    // Second pull, from an empty local LFS store: a hit, no upstream download.
    std::fs::remove_dir_all(git.dir.join(".git").join("lfs").join("objects")).unwrap();
    write(&git.dir.join("baseline.png"), pointer.as_bytes());
    pull().await.unwrap();
    assert_eq!(std::fs::read(git.dir.join("baseline.png")).unwrap(), object);
    assert_eq!(
        gh.downloads.load(Ordering::Relaxed),
        1,
        "served from the store"
    );

    let c = service.stats().repos["owner/demo"].clone();
    assert_eq!((c.misses, c.hits), (1, 1));
    assert_eq!(c.bytes_from_upstream, object.len() as u64);
    assert_eq!(c.bytes_from_cache, object.len() as u64);
    let _ = std::fs::remove_dir_all(&root);
}
