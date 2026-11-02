//! The privacy gate: nothing tracked in this repo may name a private project,
//! carry a credential, or point at somebody's home directory.
//!
//! faber is public. Everything tracked here is published the moment it is
//! pushed, and git history keeps it published long after a later commit removes
//! it — so this has to fail *before* the commit, not after the leak. It is a
//! test rather than a CI-only script for one reason: `cargo test` is what every
//! contributor and every coding agent already runs, and a gate nobody runs is
//! not a gate. See `AGENTS.md`.
//!
//! ## How the denylist stays secret
//!
//! The digests and the key that makes publishing them safe are described in
//! `src/privacy.rs` and ADR-0058. In short: names are checked only where
//! `FABER_PRIVACY_KEY` is set. Without it this test prints a loud skip notice
//! and passes, except in CI, which sets `FABER_PRIVACY_KEY_REQUIRED=true` so
//! that a missing key fails it.
//!
//! A failure reports the file and line and **never the token that matched**,
//! and any part of a file name that matched prints as `***`: printing it into a
//! public CI log would leak exactly what the gate is for. Open the file and look.
//!
//! ## Adding a name
//!
//! See *Keeping the list current* in `src/privacy.rs`. `examples/privacy_denylist.rs`
//! derives the list; the `print_digest` helper below adds one name no source
//! shows. Both need the key.

use std::path::{Path, PathBuf};
use std::process::Command;

use faber::privacy::{
    announce_skip, carries_personal_path, carries_secret, denylist_from_env, key_from_env,
    name_token, words, MAX_JOIN, MIN_TOKEN, SKIP_ENTRIES,
};

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every file that would be published — which is exactly what git tracks, so
/// ask git. Falls back to a manual walk when there is no git (a source tarball,
/// a vendored checkout) so the gate degrades to "still runs" rather than
/// "silently passes".
fn published_files(root: &Path) -> Vec<PathBuf> {
    if let Ok(out) = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "-z"])
        .output()
    {
        if out.status.success() {
            let mut files: Vec<PathBuf> = out
                .stdout
                .split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| root.join(git_path(s)))
                .collect();
            if !files.is_empty() {
                files.sort();
                return files;
            }
        }
    }

    let mut files = Vec::new();
    walk(root, &mut files);
    files.sort();
    files
}

/// A path as `git ls-files -z` prints it: raw bytes, which on Unix need not be
/// UTF-8. Decoded lossily, such a name would point at no file, and the scan
/// would skip its content without a word.
#[cfg(unix)]
fn git_path(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    std::ffi::OsStr::from_bytes(bytes).into()
}

/// Elsewhere git prints paths as UTF-8.
#[cfg(not(unix))]
fn git_path(bytes: &[u8]) -> PathBuf {
    String::from_utf8_lossy(bytes).as_ref().into()
}

/// The fallback enumeration, used only when git cannot answer.
///
/// It cannot reproduce `.gitignore`, so in a live working checkout it also sees
/// ignored files — local tooling state, scratch files — that would never be
/// published and may legitimately contain absolute paths. That is the right
/// trade for the case this exists to serve (a source tarball, where everything
/// present *is* the published content) but it means a failure reported here and
/// nowhere else is worth checking against `git check-ignore` before believing.
fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        // By name, before asking whether it is a directory: a worktree's `.git`
        // is a file, and must be skipped just the same.
        if SKIP_ENTRIES.contains(&name.to_string_lossy().as_ref()) {
            continue;
        }
        let path = entry.path();
        // The entry's own type, not `path.is_dir()`, which follows a link: a
        // link is published as the path it holds, and one to a directory above
        // it would walk that directory again, over and over.
        if entry.file_type().is_ok_and(|t| t.is_dir()) {
            walk(&path, out);
        } else {
            out.push(path);
        }
    }
}

/// What publishing `path` publishes, as text.
///
/// For a symbolic link that is the path it holds: git stores the link, not
/// what it points at. Reading through the link would scan a file that is not
/// published, or, where the target is absent (as in CI), nothing at all.
///
/// Otherwise, text is anything without a NUL byte near the front — which keeps
/// icons, `.icns` bundles and lockfile binaries out of the scan without
/// maintaining an extension list. Bytes that are not UTF-8 are replaced, as
/// `examples/privacy_scan.rs` does, so one stray Latin-1 byte cannot hide a
/// whole file.
fn read_text(path: &Path) -> Option<String> {
    if let Ok(target) = std::fs::read_link(path) {
        return Some(target.to_string_lossy().into_owned());
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

#[test]
fn a_file_that_is_not_utf8_is_still_scanned() {
    // Assembled at run time: spelled out, these lines would trip the gate.
    let credential = format!("token = {}{}", "ghp_", "0123456789abcdefghijklmnopqrstuv");
    let home = format!("see /Users/{}/notes", "adalovelace");
    // One stray byte: "café" as a Latin-1 editor saves it.
    let mut bytes = b"caf\xe9\n".to_vec();
    bytes.extend_from_slice(format!("{credential}\n{home}\n").as_bytes());
    let path = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("privacy-not-utf8-{}.txt", std::process::id()));
    std::fs::write(&path, &bytes).expect("write the fixture");
    let text = read_text(&path);
    std::fs::remove_file(&path).expect("remove the fixture");

    let text = text.expect("a stray byte does not make a file binary");
    assert!(
        text.lines().any(carries_secret),
        "the credential is scanned"
    );
    assert!(
        text.lines().any(carries_personal_path),
        "the home path is scanned"
    );
}

#[cfg(unix)]
#[test]
fn a_tracked_path_keeps_the_bytes_git_printed() {
    use std::os::unix::ffi::OsStrExt;
    let raw = b"docs/caf\xe9.md";
    assert_eq!(git_path(raw).as_os_str().as_bytes(), raw);
}

/// An empty directory of its own under cargo's scratch space.
#[cfg(unix)]
fn scratch_dir(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("privacy-{name}-{}", std::process::id()));
    // Left over from a run that failed before cleaning up, if anything.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create a scratch directory");
    dir
}

#[cfg(unix)]
#[test]
fn a_link_is_scanned_as_the_path_it_holds() {
    use std::os::unix::fs::symlink;
    let dir = scratch_dir("links");
    // Assembled at run time: spelled out, these lines would trip the gate.
    let home = format!("/Users/{}/notes", "adalovelace");
    let credential = format!("token = {}{}", "ghp_", "0123456789abcdefghijklmnopqrstuv");
    // A link into someone's home points at nothing on any other machine, CI
    // included, and git publishes the path all the same.
    symlink(&home, dir.join("dangling")).expect("link to a home path");
    // A link to a file: the file is not what the link publishes.
    std::fs::write(dir.join("target.txt"), &credential).expect("write the target");
    symlink("target.txt", dir.join("link")).expect("link to the target");
    let dangling = read_text(&dir.join("dangling"));
    let link = read_text(&dir.join("link"));
    std::fs::remove_dir_all(&dir).expect("remove the scratch directory");

    let dangling = dangling.expect("a link to nothing is still read");
    assert!(carries_personal_path(&dangling), "the home path is scanned");
    assert_eq!(link.as_deref(), Some("target.txt"));
}

#[cfg(unix)]
#[test]
fn the_fallback_walk_lists_a_link_and_does_not_follow_it() {
    use std::os::unix::fs::symlink;
    let dir = scratch_dir("walk");
    std::fs::write(dir.join("file.txt"), "text").expect("write a file");
    // A link to the directory it sits in: followed, the walk would go round.
    symlink(".", dir.join("loop")).expect("link to the directory");
    let mut found = Vec::new();
    walk(&dir, &mut found);
    found.sort();
    std::fs::remove_dir_all(&dir).expect("remove the scratch directory");

    assert_eq!(found, [dir.join("file.txt"), dir.join("loop")]);
}

#[test]
fn tree_names_no_private_project() {
    let mut names = match denylist_from_env() {
        Ok(Some(names)) => names,
        Ok(None) => {
            announce_skip();
            return;
        }
        Err(e) => panic!("{}", e.describe()),
    };
    let root = repo_root();
    let mut findings: Vec<String> = Vec::new();

    for path in published_files(&root) {
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();

        // The path itself is published too — a file *named* after a private
        // project leaks just as much as one that mentions it.
        if names.names_any(&rel) {
            findings.push(format!("{} (in the file name)", names.mask(&rel)));
        }

        let Some(text) = read_text(&path) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            if names.names_any(line) {
                findings.push(format!("{}:{}", names.mask(&rel), n + 1));
            }
        }
    }

    assert!(
        findings.is_empty(),
        "a private project name is present in {} place(s):\n  {}\n\n\
         The matched token is deliberately not printed, and any part of a file \
         name that matched shows as `***` — this output reaches a public CI log. \
         Open the file and replace the name with a placeholder (`owner/demo`, \
         `ghcr.io/owner/my-runner`). See AGENTS.md.",
        findings.len(),
        findings.join("\n  "),
    );
}

#[test]
fn tree_carries_no_credentials_or_personal_paths() {
    let root = repo_root();
    // Paths print masked wherever the key allows: a file named after a private
    // project would otherwise leak through this test's output.
    let mut names = denylist_from_env().ok().flatten();
    let mut findings: Vec<String> = Vec::new();

    for path in published_files(&root) {
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(&path)
            .to_string_lossy()
            .into_owned();
        let shown = names.as_mut().map_or_else(|| rel.clone(), |n| n.mask(&rel));
        let Some(text) = read_text(&path) else {
            continue;
        };
        for (n, line) in text.lines().enumerate() {
            if carries_secret(line) {
                findings.push(format!("{shown}:{} — looks like a credential", n + 1));
            }
            if carries_personal_path(line) {
                findings.push(format!(
                    "{shown}:{} — absolute path into someone's home",
                    n + 1
                ));
            }
        }
    }

    assert!(
        findings.is_empty(),
        "{} finding(s):\n  {}\n\n\
         A credential in the tree is compromised the moment it is pushed — rotate \
         it, do not merely delete it. A home path should be derived at run time \
         (`$HOME`, `dirs`) or written as the role it describes (`/Users/ci`).",
        findings.len(),
        findings.join("\n  "),
    );
}

/// Print the keyed digest of `$FABER_PRIVACY_NAME` so it can be added to
/// `DENIED`. Ignored: it is a tool, not an assertion. It never prints the name.
#[test]
#[ignore = "helper: see `Keeping the list current` in src/privacy.rs"]
fn print_digest() {
    let key = key_from_env()
        .unwrap_or_else(|e| panic!("{}", e.describe()))
        .expect("export FABER_PRIVACY_KEY first: a digest made without the key matches nothing");
    let name = std::env::var("FABER_PRIVACY_NAME")
        .expect("set FABER_PRIVACY_NAME to the name you want a digest for");
    let token = name_token(&name);
    assert!(
        token.len() >= MIN_TOKEN,
        "a name shorter than {MIN_TOKEN} characters cannot be caught by this scan"
    );
    let count = words(&name).len();
    if count > MAX_JOIN {
        eprintln!(
            "warning: this name has {count} words and the scan joins at most {MAX_JOIN}, so it \
             is caught only where it is written with nothing between its words. Consider \
             listing its most distinctive part as a name of its own too."
        );
    }
    println!("    \"{}\",", key.digest(&token));
}
