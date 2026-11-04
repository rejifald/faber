//! A live exercise of the runner-image variant cache against the real Docker
//! daemon and a real runner base image (ADR-0008).
//!
//! `#[ignore]`d, because it needs a Docker daemon, a multi-gigabyte base image and
//! several minutes of browser downloads — none of which belong in `cargo test`.
//! Everything the unit tests can decide, they decide; what only a real build can
//! answer is whether faber's recipe actually works *on the image faber is given*:
//! whether the base has `npx`, whether `--with-deps` can reach a package manager,
//! whether the browsers land where the runner expects them, and whether the image
//! that comes out is one the drift check reads as in sync.
//!
//! Run it against the base a profile actually uses:
//!
//! ```sh
//! FABER_LIVE_BASE=ghcr.io/owner/my-runner:2.335.1 \
//! FABER_LIVE_PW=1.62.1 \
//!   cargo test --test live_variant_build -- --ignored --nocapture
//! ```
//!
//! It leaves the variant behind on purpose — it is the artifact worth inspecting,
//! and the next run finds it warm and proves the cache-hit path instead.

use std::collections::HashSet;

use faber::config::{ImageBuildMode, RunnerImagePolicy};
use faber::image::{variant_tag, EvictTier, VariantCache};

/// The base image and pinned version under test, or `None` when the environment
/// didn't name them (the test then explains itself and passes rather than failing
/// for a reason that has nothing to do with the code).
fn target() -> Option<(String, String)> {
    let base = std::env::var("FABER_LIVE_BASE").ok()?;
    let version = std::env::var("FABER_LIVE_PW").ok()?;
    (!base.trim().is_empty() && !version.trim().is_empty()).then_some((base, version))
}

/// Read one `docker inspect` field, panicking with the daemon's own words.
fn inspect(image: &str, format: &str) -> String {
    let out = std::process::Command::new("docker")
        .args(["image", "inspect", "-f", format, image])
        .output()
        .expect("running docker");
    assert!(
        out.status.success(),
        "docker inspect {image} failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Run a command *inside* the variant and return its stdout, so the assertions are
/// about what a job would actually find rather than about image metadata.
fn run_in(image: &str, script: &str) -> String {
    let out = std::process::Command::new("docker")
        .args(["run", "--rm", "--entrypoint", "sh", image, "-c", script])
        .output()
        .expect("running docker");
    assert!(
        out.status.success(),
        "`{script}` in {image} failed: {}",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[tokio::test]
#[ignore = "needs a Docker daemon and a real runner base image; see the module doc"]
async fn a_variant_built_from_a_real_base_is_one_the_drift_check_reads_as_in_sync() {
    let Some((base, version)) = target() else {
        eprintln!(
            "skipped: set FABER_LIVE_BASE and FABER_LIVE_PW to run this against a real image"
        );
        return;
    };

    // A private root, so a live exercise never writes into the running daemon's
    // ledger or overwrites the recipe it would build from.
    let root = std::env::temp_dir().join(format!("faber-live-{}", std::process::id()));
    let cache = VariantCache::rooted(
        RunnerImagePolicy {
            build: ImageBuildMode::OnDemand,
            // One warm variant, no TTL: this test is about the build, and the
            // eviction rules have their own (pure) coverage.
            keep: 1,
            idle_ttl_hours: 0,
            build_timeout_secs: 2400,
            dockerfile: None,
        },
        Some(root.clone()),
    );

    let base_id = inspect(&base, "{{ .Id }}");
    let base_user = inspect(&base, "{{ .Config.User }}");
    let expected = variant_tag(&base_id, &version);
    eprintln!("base {base} ({base_id}) user={base_user:?} → building {expected}");

    let started = std::time::Instant::now();
    let variant = cache
        .ensure(&base, &version)
        .await
        .expect("building the variant");
    eprintln!(
        "{} {} in {:?}",
        if variant.built { "built" } else { "cache hit" },
        variant.tag,
        started.elapsed()
    );
    assert_eq!(
        variant.tag, expected,
        "the tag must be derivable, not invented"
    );

    // 1. The baked truth faber asserts. This is what makes the *next* drift check
    //    read the variant as in sync instead of re-diagnosing what was just healed.
    assert_eq!(
        inspect(
            &variant.tag,
            r#"{{ index .Config.Labels "org.faber.playwright.version" }}"#
        ),
        version
    );
    // 2. Ownership — the only reason faber is ever allowed to remove this image.
    assert_eq!(
        inspect(
            &variant.tag,
            r#"{{ index .Config.Labels "org.faber.kind" }}"#
        ),
        "image"
    );
    assert_eq!(
        inspect(
            &variant.tag,
            r#"{{ index .Config.Labels "org.faber.base" }}"#
        ),
        base_id,
        "a variant must name the base it derives from, or a base rebuild can't orphan it"
    );
    // 3. The base's user, restored — asserted on the *effective* user rather than
    //    on `.Config.User`, because those are not the same question. A Dockerfile
    //    `USER` needs an argument, so a base declaring none (the empty string,
    //    which means root) can only come back as the literal `root`. Comparing
    //    metadata would call that a regression; comparing who the process actually
    //    is asks the thing worth asking — a runner image that silently starts
    //    running as somebody else is a different image in the way that matters most.
    let base_whoami = run_in(&base, "id -un");
    assert_eq!(
        run_in(&variant.tag, "id -un"),
        base_whoami,
        "the variant must run as the same user as its base (base declares {base_user:?})"
    );

    // 4. What a job would actually find. The version file the in-job consumer
    //    reads, and browsers on disk for this version — the whole point of the
    //    build, and the half `docker inspect` cannot see.
    let browsers_path = run_in(
        &variant.tag,
        r#"echo "${PLAYWRIGHT_BROWSERS_PATH:-/opt/ms-playwright}""#,
    );
    assert_eq!(
        run_in(
            &variant.tag,
            &format!("cat {browsers_path}/.playwright-version")
        ),
        version
    );
    let listing = run_in(&variant.tag, &format!("ls {browsers_path}"));
    eprintln!("{browsers_path}:\n{listing}");
    for engine in ["chromium", "firefox", "webkit"] {
        assert!(
            listing.contains(engine),
            "no {engine} build in {browsers_path} — the install didn't do what the recipe says"
        );
    }

    // 5. The shield. This variant was handed out seconds ago, so no eviction pass
    //    may take it however hard the host is squeezed — its job may still be
    //    parked in admission with no container to prove it is in use.
    for tier in [EvictTier::Cold, EvictTier::Squeezed] {
        assert!(
            !cache
                .evictable(tier, &HashSet::new())
                .await
                .contains(&variant.tag),
            "a just-resolved variant must survive an eviction pass ({tier:?})"
        );
    }

    let _ = std::fs::remove_dir_all(&root);
    eprintln!(
        "left {} in place — re-run to exercise the cache-hit path, or `docker image rm` it",
        variant.tag
    );
}

/// The property AGENTS.md turns on, checked against the daemon's real image set:
/// the listing eviction works from can only ever return images faber built, and
/// the irreplaceable base is structurally outside it.
///
/// Asserted against the daemon's *whole* image set rather than against whatever
/// happens to be evictable today: with a fresh ledger every present variant is
/// dated now and shielded by the resolve grace, so a subset check alone would pass
/// while proving nothing. What is checked instead is concrete — the base is absent
/// from the listing, the variant is in it — and holds however empty the candidate
/// list is.
#[tokio::test]
#[ignore = "needs a Docker daemon and FABER_LIVE_BASE; see the module doc"]
async fn eviction_can_never_see_an_image_faber_did_not_build() {
    let Some((base, version)) = target() else {
        eprintln!("skipped: set FABER_LIVE_BASE and FABER_LIVE_PW");
        return;
    };
    let cache = VariantCache::rooted(
        RunnerImagePolicy {
            build: ImageBuildMode::OnDemand,
            keep: 0,
            idle_ttl_hours: 0,
            build_timeout_secs: 60,
            dockerfile: None,
        },
        Some(std::env::temp_dir().join(format!("faber-live-list-{}", std::process::id()))),
    );
    // `keep = 0` and no TTL is the most permissive policy there is, so anything
    // this returns is something faber considers removable.
    let removable: HashSet<String> = cache
        .evictable(EvictTier::Cold, &HashSet::new())
        .await
        .into_iter()
        .collect();
    let labelled = tags(&["--filter", "label=org.faber.kind=image"]);
    let all = tags(&[]);
    eprintln!(
        "{} images on this host, {} faber-built, {} removable right now",
        all.len(),
        labelled.len(),
        removable.len()
    );

    // The base — irreplaceable, exists in no registry — must be invisible to the
    // listing eviction works from. This is the assertion AGENTS.md is about.
    assert!(
        all.contains(&base),
        "the base {base} must be on this host for the test to mean anything"
    );
    assert!(
        !labelled.contains(&base),
        "{base} is visible to eviction — removing it would take CI down with \
         nothing to re-pull"
    );

    // And the variant built from it must be visible, or nothing is ever reclaimed.
    let base_id = inspect(&base, "{{ .Id }}");
    let variant = variant_tag(&base_id, &version);
    if all.contains(&variant) {
        assert!(
            labelled.contains(&variant),
            "{variant} is a faber-built variant but carries no org.faber.kind=image"
        );
    } else {
        eprintln!("note: {variant} not built yet — the positive half was not exercised");
    }

    // Nothing outside the labelled set may ever be a candidate.
    for image in &removable {
        assert!(
            labelled.contains(image),
            "{image} is removable without carrying org.faber.kind=image — the one \
             property that keeps an irreplaceable base image safe"
        );
    }
}

/// Every `repo:tag` on this host, optionally filtered, as faber's own listing sees
/// them.
fn tags(filter: &[&str]) -> HashSet<String> {
    let mut args = vec!["image", "ls"];
    args.extend_from_slice(filter);
    args.extend_from_slice(&["--format", "{{ .Repository }}:{{ .Tag }}"]);
    let out = std::process::Command::new("docker")
        .args(&args)
        .output()
        .expect("running docker");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains("<none>"))
        .map(String::from)
        .collect()
}
