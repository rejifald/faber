//! No message string carries a run of spaces (ADR-0050, section 8).
//!
//! A string literal split across source lines needs a `\` at the end of each
//! line, or the next line's indentation becomes part of the text. The compiler
//! accepts both, so this lost-continuation bug ("the              supervisor")
//! only shows on an operator's screen. This test scans every literal under
//! `src/` for three or more spaces between two non-space characters on one
//! line of its value.
//!
//! Alignment is the renderer's job (`src/out.rs`), so a deliberate run is
//! allowed only where it is listed below, with why. Each entry names a file and
//! the run's text, so the rest of that file is still checked. An item under
//! `#[cfg(test)]` (a test module, a test-only helper) is a fixture and not
//! scanned; the code around it is.

use std::path::{Path, PathBuf};

/// Deliberate runs: the file, and a fragment of the literal that holds the
/// run. Column labels go once their command renders through `out.rs`; file
/// formats (a plist, cloud-init YAML) stay for good.
const ALLOWED: &[(&str, &str)] = &[
    // Setup's closing hand-off (`setup::handoff`), which ADR-0050 leaves as
    // it is.
    ("src/setup/mod.rs", "  dashboard  faber dashboard"),
    ("src/setup/uninstall.rs", "drain       "),
    ("src/setup/uninstall.rs", "stop        "),
    ("src/setup/uninstall.rs", "remove      "),
    ("src/setup/uninstall.rs", "copy        "),
    ("src/setup/uninstall.rs", "flush       "),
    ("src/setup/uninstall.rs", "delete      "),
    ("src/setup/uninstall.rs", "leave       "),
    ("src/setup/uninstall.rs", "log out     "),
    ("src/setup/uninstall.rs", "turn off    "),
    ("src/setup/uninstall.rs", "restore     "),
    ("src/setup/uninstall.rs", "update      "),
    ("src/setup/uninstall.rs", "verify      "),
    ("src/setup/uninstall.rs", "  record    "),
    ("src/setup/golden_prepare.rs", "  Xcode      "),
    ("src/setup/golden_prepare.rs", "    total          "),
    ("src/setup/golden_prepare.rs", "  Homebrew   "),
    ("src/setup/golden_prepare.rs", "  Disk       "),
    ("src/setup/golden_prepare.rs", "  now:      "),
    ("src/status.rs", "wall     "),
    ("src/status.rs", "image    "),
    ("src/status.rs", "token    "),
    ("src/status.rs", "window   "),
    ("src/status.rs", "daemon   "),
    ("src/status.rs", "runs     "),
    ("src/status.rs", "gate     "),
    ("src/status.rs", "also     "),
    ("src/status.rs", "disk     "),
    // Until `faber auth` renders through `out.rs`.
    ("src/auth.rs", "    name         "),
    ("src/auth.rs", "    owner        "),
    ("src/auth.rs", "    expires in   "),
    // A file format, not a message: the plist's indentation.
    ("src/platform/service.rs", "{args}    </array>"),
];

/// A `\n` escape in a literal's value: a line break the author meant. A raw
/// newline stays `\n`, since a newline then indentation is the same lost
/// continuation as a run of spaces.
const ESCAPED_NEWLINE: char = '\u{0}';

/// One string literal: where it starts, and its value with `\`-newline
/// continuations applied.
struct Literal {
    line: usize,
    value: String,
}

/// The string literals in `src`, up to the first `#[cfg(test)]` line. A small
/// lexer: it skips comments and char literals, and understands escapes and raw
/// strings, which is all that decides where a literal starts and ends.
fn literals(src: &str) -> Vec<Literal> {
    let b = src.as_bytes();
    let line_at = |i: usize| src[..i].matches('\n').count() + 1;
    let mut out = Vec::new();
    // Brace depth, and the depth a `#[cfg(test)]` item opened at: literals
    // inside it are test fixtures. `pending` is set between the attribute and
    // the item's `{` (or its `;`, for an item with no body).
    let mut depth = 0usize;
    let mut pending = false;
    let mut skip_at: Option<usize> = None;
    let mut i = 0;
    while i < b.len() {
        let keep = skip_at.is_none();
        match b[i] {
            b'#' if b[i..].starts_with(b"#[cfg(test)]") => {
                pending = true;
                i += b"#[cfg(test)]".len();
            }
            b'{' => {
                if pending && skip_at.is_none() {
                    skip_at = Some(depth);
                }
                pending = false;
                depth += 1;
                i += 1;
            }
            b'}' => {
                depth = depth.saturating_sub(1);
                if skip_at == Some(depth) {
                    skip_at = None;
                } else if skip_at.is_none() {
                    pending = false;
                }
                i += 1;
            }
            // The end of an item with no body, or of a gated field, variant
            // or match arm: the attribute gated it, and nothing after.
            b';' | b',' => {
                pending = false;
                i += 1;
            }
            b'/' if b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let mut depth = 0;
                while i < b.len() {
                    if b[i..].starts_with(b"/*") {
                        depth += 1;
                        i += 2;
                    } else if b[i..].starts_with(b"*/") {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
            }
            b'\'' => {
                // A char literal (`'a'`, `'\n'`, `'"'`), or a lifetime.
                if b.get(i + 1) == Some(&b'\\') {
                    i += 2;
                    while i < b.len() && b[i] != b'\'' {
                        i += 1;
                    }
                    i += 1;
                } else if b.get(i + 2) == Some(&b'\'') {
                    i += 3;
                } else {
                    // Lifetimes, and multi-byte chars: skip one UTF-8 char.
                    let c = src[i + 1..].chars().next().map_or(1, char::len_utf8);
                    if b.get(i + 1 + c) == Some(&b'\'') {
                        i += 2 + c;
                    } else {
                        i += 1;
                    }
                }
            }
            b'r' if (!ident_before(b, i) || raw_prefix(b, i))
                && (b.get(i + 1) == Some(&b'"') || b.get(i + 1) == Some(&b'#')) =>
            {
                let hashes = b[i + 1..].iter().take_while(|&&c| c == b'#').count();
                if b.get(i + 1 + hashes) != Some(&b'"') {
                    i += 1;
                    continue;
                }
                let start = i + 2 + hashes;
                let close: Vec<u8> = std::iter::once(b'"')
                    .chain(std::iter::repeat_n(b'#', hashes))
                    .collect();
                let mut j = start;
                while j < b.len() && !b[j..].starts_with(&close) {
                    j += 1;
                }
                // A raw string cannot continue a line with `\`, so its line
                // breaks are the author's: a script, a stylesheet.
                if keep {
                    out.push(Literal {
                        line: line_at(i),
                        value: src[start..j.min(b.len())]
                            .replace('\n', &ESCAPED_NEWLINE.to_string()),
                    });
                }
                i = j + close.len();
            }
            b'"' => {
                let mut value = String::new();
                let mut j = i + 1;
                while j < b.len() && b[j] != b'"' {
                    if b[j] == b'\\' {
                        match b.get(j + 1) {
                            Some(b'\n') => {
                                // A continuation: the newline and the next
                                // line's leading whitespace are not text.
                                j += 2;
                                while j < b.len() && b[j].is_ascii_whitespace() {
                                    j += 1;
                                }
                            }
                            Some(b'n') => {
                                value.push(ESCAPED_NEWLINE);
                                j += 2;
                            }
                            Some(_) => {
                                value.push('x');
                                j += 2;
                            }
                            None => j += 1,
                        }
                    } else {
                        let c = src[j..].chars().next().unwrap_or(' ');
                        value.push(c);
                        j += c.len_utf8();
                    }
                }
                if keep {
                    out.push(Literal {
                        line: line_at(i),
                        value,
                    });
                }
                i = j + 1;
            }
            _ => i += 1,
        }
    }
    out
}

/// Whether the byte before `i` continues an identifier, so `r` there is not a
/// raw-string prefix (`for"` never occurs, but `bar"` would be `ba` + `r"`).
fn ident_before(b: &[u8], i: usize) -> bool {
    i > 0 && (b[i - 1].is_ascii_alphanumeric() || b[i - 1] == b'_')
}

/// Whether the `r` at `i` follows a `b` or `c` prefix that starts a token
/// (`br"…"`, `cr#"…"#`): a raw byte or C string.
fn raw_prefix(b: &[u8], i: usize) -> bool {
    i > 0 && matches!(b[i - 1], b'b' | b'c') && !ident_before(b, i - 1)
}

/// Whether a literal's value has three or more spaces between two non-space
/// characters on one of its intended lines.
fn has_gap(value: &str) -> bool {
    value
        .split(ESCAPED_NEWLINE)
        .any(|line| line.trim_matches(' ').contains("   "))
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn no_string_literal_carries_a_run_of_spaces() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut files = Vec::new();
    rust_files(&root.join("src"), &mut files);
    files.sort();
    let mut hits = Vec::new();
    let mut used = vec![false; ALLOWED.len()];
    for path in &files {
        let rel = path
            .strip_prefix(root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        let src = std::fs::read_to_string(path).unwrap();
        for lit in literals(&src) {
            if !has_gap(&lit.value) {
                continue;
            }
            let allowed = ALLOWED
                .iter()
                .position(|(file, frag)| *file == rel && lit.value.contains(frag));
            if let Some(n) = allowed {
                used[n] = true;
            } else {
                hits.push(format!("{rel}:{} {:?}", lit.line, lit.value));
            }
        }
    }
    assert!(
        hits.is_empty(),
        "string literals with a run of three or more spaces, probably a lost `\\` \
         continuation (ADR-0050 section 8); end the line with `\\`, or let src/out.rs \
         align it:\n  {}",
        hits.join("\n  ")
    );
    // An entry that matches nothing is stale: the run it allowed is gone, so
    // the entry would only hide the next one.
    let stale: Vec<String> = ALLOWED
        .iter()
        .zip(&used)
        .filter(|(_, used)| !**used)
        .map(|((file, frag), _)| format!("{file} {frag:?}"))
        .collect();
    assert!(
        stale.is_empty(),
        "allowlist entries that match nothing:\n  {}",
        stale.join("\n  ")
    );
}

#[test]
fn the_lexer_finds_the_lost_continuation() {
    let src = "fn f() {\n    let a = \"one \\\n             two\";\n    let b = \"one\n             two\";\n    let c = 'x'; let d: &'static str = r#\"a   \"b\"#;\n}\n#[cfg(test)]\nmod t { const X: &str = \"a     b\"; }\n";
    let lits = literals(src);
    let values: Vec<&str> = lits.iter().map(|l| l.value.as_str()).collect();
    assert_eq!(values, ["one two", "one\n             two", "a   \"b"]);
    assert!(!has_gap(&lits[0].value));
    // A raw newline and the next line's indentation: the same bug.
    assert!(has_gap(&lits[1].value));
    assert!(has_gap(&lits[2].value));
    assert_eq!(lits[2].line, 6);
}

#[test]
fn a_gap_is_between_two_non_space_characters() {
    assert!(has_gap("the              supervisor"));
    assert!(!has_gap("   indented"));
    assert!(!has_gap("trailing   "));
    assert!(!has_gap("one  two"));
    assert!(!has_gap("one\u{0}   indented"));
}

#[test]
fn only_the_item_under_cfg_test_is_skipped() {
    let src = "#[cfg(test)]\nfn helper() -> &'static str { \"x     y\" }\n\
               #[cfg(test)]\nuse std::fmt;\n\
               struct S { #[cfg(test)]\n trail: Vec<u8>, kept: u8 }\n\
               struct T { a: u8 }\n fn t() { \"after   field\" }\n\
               enum E { #[cfg(test)]\n V }\n fn u() { \"after   variant\" }\n\
               const B: &[u8] = br#\"a\"b\"#;\n\
               fn f() { let c = '{'; \"after   gap\" }\n\
               #[cfg(test)]\nmod tests { fn g() { if true { \"in   test\"; } } }\n\
               const Z: &str = \"last   one\";\n";
    let values: Vec<String> = literals(src).into_iter().map(|l| l.value).collect();
    assert_eq!(
        values,
        [
            "after   field",
            "after   variant",
            "a\"b",
            "after   gap",
            "last   one"
        ]
    );
}
