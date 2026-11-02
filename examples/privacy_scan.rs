//! Text privacy gate: scan files of free text (commit messages, PR title/body,
//! issue and comment text) with the same scanner as `tests/privacy.rs`.
//!
//! `cargo run --example privacy_scan -- <file>...` exits 1 on any hit. A hit
//! prints the file, line and kind, **never the matched text**, and a file name
//! is printed with any private name in it masked: this output lands in a public
//! CI log. Private names are checked only with `FABER_PRIVACY_KEY` set (see
//! `src/privacy.rs`); without it the run says so loudly and still checks
//! credentials and home paths. Not shipped; examples are not part of the
//! release build.

use faber::privacy::{announce_skip, denylist_from_env, scan_text};

fn main() {
    let mut names = match denylist_from_env() {
        Ok(names) => names,
        Err(e) => {
            eprintln!("::error::{}", e.describe());
            std::process::exit(1);
        }
    };
    if names.is_none() {
        announce_skip();
    }
    let mut failed = false;
    for path in std::env::args().skip(1) {
        let shown = names
            .as_mut()
            .map_or_else(|| path.clone(), |n| n.mask(&path));
        let text = match std::fs::read(&path) {
            Ok(b) => String::from_utf8_lossy(&b).into_owned(),
            Err(e) => {
                eprintln!("::error::{shown}: cannot read ({})", e.kind());
                failed = true;
                continue;
            }
        };
        for (line, kind) in scan_text(&text, names.as_mut()) {
            eprintln!("::error::{shown}:{line}: {}", kind.describe());
            failed = true;
        }
    }
    if failed {
        eprintln!(
            "The matched text is deliberately not printed (public log). \
             Edit the flagged text; see AGENTS.md."
        );
        std::process::exit(1);
    }
}
