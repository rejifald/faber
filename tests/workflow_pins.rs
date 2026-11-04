//! What the workflows run, and on what, is pinned, allowed, kept current and
//! hardened (#141). Facts that used to be checked only in review:
//!
//! - every `uses:` of another repo's action, in a workflow or in a composite action
//!   under `.github/actions`, is pinned to a full commit SHA, with its tag as a comment;
//! - every third-party action matches the allow-list in `infra/github/main.tf`
//!   (GitHub-owned `actions/*` and `github/*` are allowed wholesale there);
//! - every action `release.yml` uses is in Dependabot's `release-workflow` group,
//!   so a pin change there is never filed under another title;
//! - Dependabot reads every lockfile (Cargo and npm) and every composite action;
//! - `release.yml` builds on exact toolchains and runner images, and
//!   `deploy/release.sh` expects exactly the assets it stages;
//! - every workflow declares its token permissions and every job a timeout, every
//!   checkout drops the token after cloning, and every cargo command that resolves
//!   dependencies is `--locked`.
//!
//! Plain line parsing, deliberately: the files are small and their shape is
//! fixed, and a YAML dependency would be a heavier thing to trust than this.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(root().join(rel)).unwrap_or_else(|e| panic!("reading {rel}: {e}"))
}

/// Leading spaces of a line.
fn indent(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// A scalar as written after `key:`: without its trailing comment or its quotes.
fn scalar(raw: &str) -> String {
    let value = raw.split_once(" #").map_or(raw, |(v, _)| v);
    value.trim().trim_matches('"').to_string()
}

/// One `uses:` line: the action (`owner/name`), the ref and its comment.
struct Use {
    file: String,
    line: usize,
    action: String,
    reference: String,
    comment: String,
}

impl Use {
    fn is_pinned(&self) -> bool {
        self.reference.len() == 40
            && self.reference.chars().all(|c| c.is_ascii_hexdigit())
            && !self.comment.is_empty()
    }
}

fn uses_in(file: &str, text: &str) -> Vec<Use> {
    let mut out = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let t = raw.trim_start().trim_start_matches("- ").trim_start();
        let Some(rest) = t.strip_prefix("uses:") else {
            continue;
        };
        let (spec, comment) = match rest.split_once(" #") {
            Some((s, c)) => (s.trim(), c.trim()),
            None => (rest.trim(), ""),
        };
        // A local composite action (`./.github/actions/...`) is this repo's own
        // code at the workflow's own commit: there is nothing to pin or allow.
        if spec.starts_with("./") {
            continue;
        }
        let (target, reference) = spec.split_once('@').unwrap_or((spec, ""));
        let action = target.split('/').take(2).collect::<Vec<_>>().join("/");
        out.push(Use {
            file: file.to_string(),
            line: i + 1,
            action,
            reference: reference.to_string(),
            comment: comment.to_string(),
        });
    }
    out
}

/// The entries of `dir` under `base`, sorted, as paths relative to `base`.
fn entries(base: &Path, dir: &str) -> Vec<String> {
    let Ok(read_dir) = std::fs::read_dir(base.join(dir)) else {
        return Vec::new();
    };
    let mut names: Vec<String> = read_dir
        .map(|e| format!("{dir}/{}", e.unwrap().file_name().to_string_lossy()))
        .collect();
    names.sort();
    names
}

/// Every workflow, then every composite action, under `base`, relative to it.
/// A composite action is a workflow's steps in another file: what it runs is as
/// much the build's as anything in `workflows/`.
fn workflow_files_in(base: &Path) -> Vec<String> {
    let mut files: Vec<String> = entries(base, ".github/workflows")
        .into_iter()
        .filter(|f| f.ends_with(".yml") || f.ends_with(".yaml"))
        .collect();
    for dir in entries(base, ".github/actions") {
        for name in ["action.yml", "action.yaml"] {
            let file = format!("{dir}/{name}");
            if base.join(&file).is_file() {
                files.push(file);
            }
        }
    }
    files
}

fn workflow_files() -> Vec<String> {
    let files = workflow_files_in(&root());
    assert!(!files.is_empty(), "no workflows found");
    files
}

fn workflows_only() -> Vec<String> {
    workflow_files()
        .into_iter()
        .filter(|f| f.starts_with(".github/workflows/"))
        .collect()
}

fn all_uses() -> Vec<Use> {
    workflow_files()
        .iter()
        .flat_map(|f| uses_in(f, &read(f)))
        .collect()
}

#[test]
fn every_action_is_pinned_to_a_full_sha_with_its_tag_as_a_comment() {
    let bad: Vec<String> = all_uses()
        .into_iter()
        .filter(|u| !u.is_pinned())
        .map(|u| format!("{}:{}: {}@{}", u.file, u.line, u.action, u.reference))
        .collect();
    assert!(
        bad.is_empty(),
        "not pinned to a full SHA with a tag comment:\n{}",
        bad.join("\n")
    );
}

#[test]
fn every_third_party_action_is_on_the_allow_list() {
    let policy = read("infra/github/main.tf");
    let bad: Vec<String> = all_uses()
        .into_iter()
        .filter(|u| !u.action.starts_with("actions/") && !u.action.starts_with("github/"))
        .filter(|u| !policy.contains(&format!("\"{}@*\"", u.action)))
        .map(|u| format!("{}:{}: {}", u.file, u.line, u.action))
        .collect();
    assert!(
        bad.is_empty(),
        "not in infra/github/main.tf's patterns_allowed:\n{}",
        bad.join("\n")
    );
}

/// A composite action once escaped both checks above, because only `workflows/`
/// was read. This builds a repository with an unpinned, unlisted action in one and
/// shows the scan now sees it.
#[test]
fn a_composite_action_is_scanned_like_a_workflow() {
    let dir = std::env::temp_dir().join(format!("faber-workflow-pins-{}", std::process::id()));
    let action = dir.join(".github/actions/tool");
    std::fs::create_dir_all(&action).unwrap();
    std::fs::create_dir_all(dir.join(".github/workflows")).unwrap();
    std::fs::write(
        action.join("action.yml"),
        "runs:\n  using: composite\n  steps:\n    - uses: owner/tool@v1\n",
    )
    .unwrap();
    std::fs::write(
        dir.join(".github/workflows/ci.yml"),
        "jobs:\n  a:\n    steps:\n      - uses: owner/other@v2\n",
    )
    .unwrap();

    let files = workflow_files_in(&dir);
    let unpinned: Vec<String> = files
        .iter()
        .flat_map(|f| uses_in(f, &std::fs::read_to_string(dir.join(f)).unwrap()))
        .filter(|u| !u.is_pinned())
        .map(|u| u.file)
        .collect();
    std::fs::remove_dir_all(&dir).unwrap();

    assert_eq!(
        unpinned,
        [
            ".github/workflows/ci.yml",
            ".github/actions/tool/action.yml"
        ]
    );
}

/// The patterns listed under Dependabot's `release-workflow` group.
fn release_group_patterns() -> Vec<String> {
    let config = read(".github/dependabot.yml");
    let mut in_group = false;
    let mut patterns = Vec::new();
    for line in config.lines() {
        let t = line.trim();
        if t.ends_with(':') && !t.starts_with('-') && t != "patterns:" {
            in_group = t == "release-workflow:";
            continue;
        }
        if in_group {
            if let Some(p) = t.strip_prefix("- ") {
                patterns.push(p.trim_matches('"').to_string());
            }
        }
    }
    assert!(
        !patterns.is_empty(),
        "no release-workflow group in .github/dependabot.yml"
    );
    patterns
}

#[test]
fn every_release_workflow_action_is_in_the_release_dependabot_group() {
    let group = release_group_patterns();
    let bad: Vec<String> = uses_in(
        ".github/workflows/release.yml",
        &read(".github/workflows/release.yml"),
    )
    .into_iter()
    .filter(|u| !group.contains(&u.action))
    .map(|u| format!("release.yml:{}: {}", u.line, u.action))
    .collect();
    assert!(
        bad.is_empty(),
        "release.yml uses actions missing from Dependabot's release-workflow group:\n{}",
        bad.join("\n")
    );
}

/// One `- package-ecosystem:` entry of `.github/dependabot.yml`, with the
/// directories it reads (`directory:` or each item of `directories:`).
struct Watch {
    ecosystem: String,
    directories: Vec<String>,
}

fn dependabot_watches() -> Vec<Watch> {
    let mut watches: Vec<Watch> = Vec::new();
    let mut in_directories = false;
    for line in read(".github/dependabot.yml").lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some(ecosystem) = t.strip_prefix("- package-ecosystem:") {
            watches.push(Watch {
                ecosystem: scalar(ecosystem),
                directories: Vec::new(),
            });
            in_directories = false;
            continue;
        }
        let Some(watch) = watches.last_mut() else {
            continue;
        };
        if let Some(directory) = t.strip_prefix("directory:") {
            watch.directories.push(scalar(directory));
            in_directories = false;
        } else if t == "directories:" {
            in_directories = true;
        } else if let Some(item) = t.strip_prefix("- ").filter(|_| in_directories) {
            watch.directories.push(scalar(item));
        } else {
            in_directories = false;
        }
    }
    watches
}

/// Whether `directory` (like `/ui`) is one of `listed`, or sits one level under
/// a `/parent/*` glob there.
fn is_watched(listed: &[String], directory: &str) -> bool {
    listed.iter().any(|l| {
        l == directory
            || l.strip_suffix("/*").is_some_and(|parent| {
                directory
                    .strip_prefix(parent)
                    .and_then(|rest| rest.strip_prefix('/'))
                    .is_some_and(|name| !name.is_empty() && !name.contains('/'))
            })
    })
}

/// Every file under `base/rel`, as a path relative to `base`, leaving out what is
/// never published: git's own directory, build output, installed packages and
/// local tooling's state.
fn walked_files(base: &Path, rel: &str) -> Vec<String> {
    let mut files = Vec::new();
    let Ok(read_dir) = std::fs::read_dir(base.join(rel)) else {
        return files;
    };
    for entry in read_dir {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        if !entry.file_type().unwrap().is_dir() {
            files.push(path);
        } else if !matches!(
            name.as_str(),
            ".git" | "target" | "node_modules" | ".claude"
        ) {
            files.extend(walked_files(base, &path));
        }
    }
    files
}

/// The directories, as Dependabot names them (`/` or `/ui`), that hold a tracked
/// file called `file_name`. It asks git, as tests/privacy.rs does, and walks the
/// tree when git cannot answer (a source tarball, a container without `.git`),
/// so the check degrades to "still runs" rather than "fails".
fn tracked_directories(file_name: &str) -> Vec<String> {
    let tracked: Vec<String> = Command::new("git")
        .arg("-C")
        .arg(root())
        .args(["ls-files", "-z"])
        .output()
        .ok()
        .filter(|out| out.status.success())
        .map(|out| {
            out.stdout
                .split(|b| *b == 0)
                .map(|s| String::from_utf8_lossy(s).into_owned())
                .collect()
        })
        .unwrap_or_else(|| walked_files(&root(), ""));
    let mut dirs: Vec<String> = tracked
        .into_iter()
        .filter_map(|path| {
            let dir = if path == file_name {
                Some("")
            } else {
                path.strip_suffix(&format!("/{file_name}"))
            };
            dir.map(|d| format!("/{d}"))
        })
        .collect();
    dirs.sort();
    dirs
}

/// A lockfile Dependabot never reads is a dependency set nobody is told about
/// when it goes stale or vulnerable: the Cargo ones were exactly that.
#[test]
fn dependabot_reads_every_lockfile_and_composite_action() {
    let watches = dependabot_watches();
    let listed = |ecosystem: &str| -> Vec<String> {
        watches
            .iter()
            .filter(|w| w.ecosystem == ecosystem)
            .flat_map(|w| w.directories.clone())
            .collect()
    };

    let mut missing = Vec::new();
    for (lockfile, ecosystem) in [("Cargo.lock", "cargo"), ("package-lock.json", "npm")] {
        let listed = listed(ecosystem);
        for dir in tracked_directories(lockfile) {
            if !is_watched(&listed, &dir) {
                missing.push(format!("{ecosystem}: {dir} ({lockfile})"));
            }
        }
    }
    let actions = listed("github-actions");
    for dir in entries(&root(), ".github/actions") {
        if !is_watched(&actions, &format!("/{dir}")) {
            missing.push(format!("github-actions: /{dir}"));
        }
    }
    assert!(
        missing.is_empty(),
        ".github/dependabot.yml has no entry for:\n{}",
        missing.join("\n")
    );
}

/// The value of every `key:` line in `text`, with its line number.
fn values_of(text: &str, key: &str) -> Vec<(usize, String)> {
    let prefix = format!("{key}:");
    text.lines()
        .enumerate()
        .filter_map(|(i, raw)| {
            let t = raw.trim_start().trim_start_matches("- ").trim_start();
            t.strip_prefix(&prefix).map(|v| (i + 1, scalar(v)))
        })
        .collect()
}

fn is_exact_version(value: &str) -> bool {
    let parts: Vec<&str> = value.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
}

/// The day's toolchain must not decide what gets signed: the release is built
/// with an exact Rust and an exact Node, on named runner images.
#[test]
fn the_release_is_built_on_exact_toolchains_and_named_runner_images() {
    let text = read(".github/workflows/release.yml");
    let lines: Vec<&str> = text.lines().collect();
    let mut bad = Vec::new();

    for (pin, key, action) in [
        ("PINNED_RUST_VERSION", "toolchain", "dtolnay/rust-toolchain"),
        ("PINNED_NODE_VERSION", "node-version", "actions/setup-node"),
    ] {
        let pinned = values_of(&text, pin);
        match pinned.as_slice() {
            [(_, value)] if is_exact_version(value) => {}
            other => bad.push(format!(
                "{pin} must be set once, to an exact x.y.z: {other:?}"
            )),
        }

        // Every step that sets a toolchain up says which, and it is the pin: a
        // step that says nothing gets the action's default, the day's.
        let want = format!("${{{{ env.{pin} }}}}");
        let mut steps = 0;
        for (i, line) in lines.iter().enumerate() {
            let t = line.trim_start().trim_start_matches("- ");
            if !t.starts_with(&format!("uses: {action}@")) {
                continue;
            }
            steps += 1;
            let sets_the_pin = step_containing(&lines, i).iter().any(|l| {
                l.trim()
                    .strip_prefix(&format!("{key}:"))
                    .is_some_and(|v| scalar(v) == want)
            });
            if !sets_the_pin {
                bad.push(format!(
                    "release.yml:{}: this {action} step does not set `{key}: {want}`",
                    i + 1
                ));
            }
        }
        assert!(
            steps > 0,
            "release.yml no longer uses {action}, so {pin} pins nothing"
        );
    }

    for key in ["runs-on", "os"] {
        for (line, value) in values_of(&text, key) {
            if value.contains("latest") {
                bad.push(format!(
                    "release.yml:{line}: `{value}` moves under the build; name the image"
                ));
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// The names `release.yml` copies into `dist/`, for each target of its matrix,
/// plus the two files its `attest` job adds.
///
/// It reads quoted paths only (a staged name holds the matrix expression, whose
/// spaces need quoting in shell anyway), and it assumes every matrix leg stages
/// every asset, as the one macOS leg does. A leg that stages fewer is where to
/// teach it about the steps' conditions.
fn staged_assets() -> BTreeSet<String> {
    let text = read(".github/workflows/release.yml");
    let targets: Vec<String> = values_of(&text, "target")
        .into_iter()
        .map(|(_, t)| t)
        .collect();
    assert!(!targets.is_empty(), "release.yml has no matrix targets");

    let mut assets: BTreeSet<String> = ["SHA256SUMS", "SHA256SUMS.sigstore.json"]
        .map(String::from)
        .into();
    for line in text.lines() {
        let Some((_, after)) = line.split_once("\"dist/") else {
            continue;
        };
        let Some((name, _)) = after.split_once('"') else {
            continue;
        };
        for target in &targets {
            assets.insert(name.replace("${{ matrix.target }}", target));
        }
    }
    assets
}

/// The names listed in `deploy/release.sh`'s `EXPECTED_ASSETS=( ... )`.
fn expected_assets() -> BTreeSet<String> {
    let text = read("deploy/release.sh");
    let mut in_list = false;
    let mut names = BTreeSet::new();
    for line in text.lines() {
        let t = line.trim();
        if t == "EXPECTED_ASSETS=(" {
            in_list = true;
        } else if in_list && t == ")" {
            return names;
        } else if in_list {
            names.insert(t.trim_matches('"').to_string());
        }
    }
    panic!("deploy/release.sh has no EXPECTED_ASSETS=( ... ) list");
}

/// The check that guards a draft drifted from the workflow twice: the launcher
/// and the guest agent were staged for releases before the check asked for them.
#[test]
fn release_sh_expects_exactly_the_assets_release_yml_stages() {
    let staged = staged_assets();
    let expected = expected_assets();
    let unlisted: Vec<_> = staged.difference(&expected).collect();
    let unstaged: Vec<_> = expected.difference(&staged).collect();
    assert!(
        unlisted.is_empty() && unstaged.is_empty(),
        "release.yml stages assets deploy/release.sh does not expect: {unlisted:?}\n\
         deploy/release.sh expects assets release.yml does not stage: {unstaged:?}"
    );
}

#[test]
fn every_workflow_declares_its_token_permissions() {
    let bad: Vec<String> = workflows_only()
        .into_iter()
        .filter(|f| !read(f).lines().any(|l| l.starts_with("permissions:")))
        .collect();
    assert!(
        bad.is_empty(),
        "no top-level `permissions:` (the repository default is not a substitute):\n{}",
        bad.join("\n")
    );
}

#[test]
fn every_job_has_a_timeout() {
    let mut bad = Vec::new();
    for file in workflows_only() {
        let text = read(&file);
        // Each job id under `jobs:` (indented two) and whether a line indented
        // four, a job's own key, sets `timeout-minutes`.
        let mut jobs: Vec<(String, bool)> = Vec::new();
        let mut in_jobs = false;
        for line in text.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with('#') {
                continue;
            }
            if indent(line) == 0 {
                in_jobs = line.starts_with("jobs:");
            } else if in_jobs && indent(line) == 2 && t.ends_with(':') {
                jobs.push((t.trim_end_matches(':').to_string(), false));
            } else if in_jobs && indent(line) == 4 && t.starts_with("timeout-minutes:") {
                if let Some(job) = jobs.last_mut() {
                    job.1 = true;
                }
            }
        }
        assert!(!jobs.is_empty(), "{file}: found no jobs");
        bad.extend(
            jobs.into_iter()
                .filter(|(_, has_timeout)| !has_timeout)
                .map(|(job, _)| format!("{file}: job `{job}`")),
        );
    }
    assert!(
        bad.is_empty(),
        "no `timeout-minutes:`, so a hung job holds its runner for six hours:\n{}",
        bad.join("\n")
    );
}

/// Lines of the step that contains line `at` (0-based): from its `- ` marker to
/// the next line indented no deeper than that marker.
fn step_containing<'a>(lines: &[&'a str], at: usize) -> Vec<&'a str> {
    let mut start = at;
    // A step's keys sit two columns right of its marker, so the marker is the
    // nearest `- ` line above that is indented less than the line asked about.
    while !lines[start].trim_start().starts_with("- ")
        || (start != at && indent(lines[start]) >= indent(lines[at]))
    {
        assert!(start > 0, "no step marker above line {}", at + 1);
        start -= 1;
    }
    let marker = indent(lines[start]);
    let mut end = start + 1;
    while end < lines.len() && (lines[end].trim().is_empty() || indent(lines[end]) > marker) {
        end += 1;
    }
    lines[start..end].to_vec()
}

#[test]
fn every_checkout_drops_the_token_after_cloning() {
    let mut bad = Vec::new();
    for file in workflow_files() {
        let text = read(&file);
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let t = line.trim_start().trim_start_matches("- ");
            if !t.starts_with("uses: actions/checkout@") {
                continue;
            }
            let drops = step_containing(&lines, i).iter().any(|l| {
                l.trim()
                    .strip_prefix("persist-credentials:")
                    .is_some_and(|v| scalar(v) == "false")
            });
            if !drops {
                bad.push(format!("{file}:{}", i + 1));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "actions/checkout without `persist-credentials: false`, so the token stays \
         readable in .git/config for every build script that follows:\n{}",
        bad.join("\n")
    );
}

/// The cargo subcommands that read `Cargo.lock` and may rewrite it. `fmt`
/// resolves nothing, and `update` exists to move the lockfile.
const RESOLVING: [&str; 13] = [
    "build", "check", "clippy", "test", "run", "doc", "bench", "metadata", "tree", "deny",
    "install", "fetch", "fix",
];

/// A cargo command that resolves dependencies reads `Cargo.lock` and may rewrite
/// it, so each says `--locked`. That includes the menu-bar app's build, whose only
/// cargo call is the one `tauri build` makes with the arguments after its own `--`.
#[test]
fn cargo_is_locked_wherever_it_resolves_dependencies() {
    let mut bad = Vec::new();
    for file in workflow_files() {
        for (i, line) in read(&file).lines().enumerate() {
            // A comment or a step's `name:` says "cargo check"; only a command runs it.
            let t = line.trim_start().trim_start_matches("- ");
            if t.starts_with('#') || t.starts_with("name:") {
                continue;
            }
            let words: Vec<&str> = line.split_whitespace().collect();
            let resolves = words
                .windows(2)
                .any(|w| w[0] == "cargo" && RESOLVING.contains(&w[1]));
            let builds_app = line.contains("tauri -- build");
            let locked = if builds_app {
                line.contains(" -- --locked")
            } else {
                words.contains(&"--locked")
            };
            if (resolves || builds_app) && !locked {
                bad.push(format!("{file}:{}: {}", i + 1, line.trim()));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "cargo without `--locked` (keep it on the same line as the subcommand, and \
         after the `--` of `tauri -- build`):\n{}",
        bad.join("\n")
    );
}
