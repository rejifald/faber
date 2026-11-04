//! A live exercise of ADR-0035's git mirror against the real Docker daemon and a
//! real runner image.
//!
//! `#[ignore]`d, because it needs a Docker daemon and a runner image. The unit
//! tests in `src/git_mirror.rs` decide the policy and run the refresh script
//! against plain git. What only a live run answers is what ADR-0035 §2 leaves to
//! "the implementation's runtime test": that a mirror faber's refresh wrote as
//! root is readable through a **read-only** volume by the runner image's
//! unprivileged uid, that git's `safe.directory` check does not refuse an
//! alternate owned by another uid, and that nothing in the job can write it.
//!
//! ```sh
//! FABER_LIVE_RUNNER_IMAGE=ghcr.io/owner/my-runner:latest \
//!   cargo test --test live_git_mirror -- --ignored --nocapture
//! ```
//!
//! `FABER_LIVE_RUNNER_UID` overrides the job uid (default `1001`, the actions
//! runner image's `runner`). Everything it touches it creates, under a namespace
//! of its own, and it removes its volume on the way out. The "GitHub" it fetches
//! from is a local bare repo mounted read-only, so no credential is involved.

use faber::git_mirror::{parse_report, refspecs, REFRESH_SCRIPT};

const NS: &str = "faber-live-adr35";

fn docker(args: &[&str]) -> (bool, String) {
    let out = std::process::Command::new("docker")
        .args(args)
        .output()
        .expect("running docker");
    (
        out.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        ),
    )
}

fn git(dir: &std::path::Path, args: &[&str]) {
    let ok = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .status()
        .expect("git runs")
        .success();
    assert!(ok, "git {args:?}");
}

/// The test's scratch directory, removed on every exit, a failed assert included.
struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Volume(String);

impl Drop for Volume {
    fn drop(&mut self) {
        let _ = docker(&["volume", "rm", "-f", &self.0]);
    }
}

#[test]
#[ignore = "needs a Docker daemon and FABER_LIVE_RUNNER_IMAGE"]
fn a_root_written_mirror_is_borrowed_read_only_by_the_job_uid() {
    let Ok(image) = std::env::var("FABER_LIVE_RUNNER_IMAGE") else {
        eprintln!("FABER_LIVE_RUNNER_IMAGE is not set — skipping");
        return;
    };
    let uid = std::env::var("FABER_LIVE_RUNNER_UID").unwrap_or_else(|_| "1001".to_string());
    assert!(
        docker(&["info", "--format", "{{ .ServerVersion }}"]).0,
        "no Docker daemon"
    );

    // The "GitHub": a local bare repo with one commit on main.
    let root = TempDir(std::env::temp_dir().join(format!("{NS}-{}", std::process::id())));
    let root = root.0.clone();
    let work = root.join("work");
    let origin = root.join("origin.git");
    std::fs::create_dir_all(&work).unwrap();
    git(&root, &["init", "-q", "--bare", origin.to_str().unwrap()]);
    git(&work, &["init", "-q", "-b", "main"]);
    for i in 0..20 {
        std::fs::write(work.join(format!("f{i}.txt")), format!("{i}\n").repeat(100)).unwrap();
    }
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "one"]);
    git(&work, &["push", "-q", origin.to_str().unwrap(), "main"]);
    let origin_mount = format!("{}:/origin:ro", origin.display());

    let vol = Volume(format!("{NS}-{}", std::process::id()));
    assert!(docker(&["volume", "create", &vol.0]).0);
    let any_dir = [
        "-e",
        "GIT_CONFIG_COUNT=1",
        "-e",
        "GIT_CONFIG_KEY_0=safe.directory",
    ];

    // The refresh, as faber runs it: root, the volume read-write.
    let mut args: Vec<String> = [
        "run",
        "--rm",
        "--pull=never",
        "--user",
        "0:0",
        "--entrypoint",
        "sh",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    args.extend(any_dir.iter().map(|s| s.to_string()));
    args.extend(
        [
            "-e".to_string(),
            "GIT_CONFIG_VALUE_0=/origin".to_string(),
            "-v".to_string(),
            format!("{}:/mirror", vol.0),
            "-v".to_string(),
            origin_mount.clone(),
            image.clone(),
            "-c".to_string(),
            REFRESH_SCRIPT.to_string(),
            "sh".to_string(),
            "/mirror".to_string(),
            "file:///origin".to_string(),
            "-".to_string(),
        ]
        .into_iter()
        .chain(refspecs(&[])),
    );
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let (ok, out) = docker(&argv);
    assert!(ok, "refresh container: {out}");
    let report = parse_report(out.trim_end()).unwrap_or_else(|| panic!("report: {out}"));
    assert!(report.fetched && report.complete, "{report:?}");

    // The job: the image's own uid, the mirror read-only, the workflow step, then
    // checkout's fetch.
    // `safe.directory` for the stand-in GitHub goes in a global config: git does
    // not pass `GIT_CONFIG_COUNT` to a `file://` upload-pack, which then refuses
    // `/origin` as owned by another uid. Real checkouts fetch over HTTPS, where
    // this does not arise; the mirror itself needs no such entry (§2).
    let job = r#"set -e
export HOME=/tmp/faber-live-home && mkdir -p "$HOME"
git config --global --add safe.directory /origin
cd /tmp && rm -rf ws && git init -q ws && cd ws
git remote add origin file:///origin
echo "$FABER_GIT_MIRROR/objects" > .git/objects/info/alternates
git -c protocol.version=2 fetch -q --no-tags --depth=1 origin +refs/heads/main:refs/remotes/origin/main
git checkout -q --force -B main refs/remotes/origin/main
git count-objects -v | grep '^in-pack: 0$'
git fsck --connectivity-only
if touch "$FABER_GIT_MIRROR/written" 2>/dev/null; then echo WRITABLE; exit 1; fi
echo borrowed-read-only
"#;
    let user = format!("{uid}:{uid}");
    let mirror_mount = format!("{}:/runner/_git:ro", vol.0);
    let (ok, out) = docker(&[
        "run",
        "--rm",
        "--pull=never",
        "--user",
        &user,
        "--entrypoint",
        "sh",
        "-e",
        "FABER_GIT_MIRROR=/runner/_git",
        "-v",
        &mirror_mount,
        "-v",
        &origin_mount,
        &image,
        "-c",
        job,
    ]);
    assert!(
        ok && out.contains("borrowed-read-only"),
        "job container: {out}"
    );
}
