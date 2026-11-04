//! Text faber did not write, made safe to print to a terminal.
//!
//! The CLI prints text from less trusted places: the daemon's API replies, the
//! audit log (job, workflow and repo names come from GitHub, and anyone who can
//! push a workflow chooses them), GitHub's error bodies, and files `_faber`
//! owns. Under ADR-0025 some of it reaches root: `sudo faber setup` and
//! `uninstall` drain the daemon, so the daemon's reply lands in root's sudo
//! session. A terminal acts on control characters in what it prints. An escape
//! sequence can rewrite the screen, move the cursor to disguise a prompt, retitle
//! the window, or write the clipboard (OSC 52).
//!
//! [`sanitize`] replaces every character a terminal may act on with a visible
//! escape and keeps everything else, newlines, tabs and non-ASCII letters
//! included:
//!
//! - C0 controls except `\n` and `\t`: ESC is printed as `\x1b`, CR as `\x0d`
//! - DEL, as `\x7f`
//! - C1 controls, U+0080 to U+009F: the 8-bit CSI is printed as `\x9b`
//! - the invisible Unicode controls that reorder or break a line (bidi
//!   embeddings, overrides and isolates, LRM, RLM, ALM, U+2028, U+2029), which
//!   make printed text read differently from what it says: `\u{202e}`
//!
//! [`sanitize_bounded`] also cuts text from an unbounded source, and
//! [`sanitize_json`] does the same job for JSON faber serialized itself, where
//! a `\x1b` would not be valid.
//!
//! Every CLI path that prints such text goes through one of these. Output from
//! a less trusted child process in `sudo faber setup` has a stricter filter of
//! its own (`setup::tools::sanitize_child_output`), which keeps printable ASCII
//! only.

use std::borrow::Cow;
use std::fmt::Write;

/// Appended where [`sanitize_bounded`] cut the text.
pub const TRUNCATED: &str = "…(truncated)";

/// Whether a terminal may act on `c`, rather than print it.
fn is_unsafe(c: char) -> bool {
    matches!(
        c,
        '\u{0}'..='\u{8}'
            | '\u{b}'..='\u{1f}'
            | '\u{7f}'..='\u{9f}'
            | '\u{61c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

/// `text` with every character a terminal may act on replaced by a visible
/// escape: `\xHH` up to U+00FF, `\u{H}` above. Borrowed when there is nothing
/// to replace, which is the common case.
pub fn sanitize(text: &str) -> Cow<'_, str> {
    if !text.chars().any(is_unsafe) {
        return Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(text.len() + 16);
    for c in text.chars() {
        if is_unsafe(c) {
            let n = u32::from(c);
            // Writing to a String cannot fail.
            let _ = if n <= 0xff {
                write!(out, "\\x{n:02x}")
            } else {
                write!(out, "\\u{{{n:x}}}")
            };
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

/// [`sanitize`], after cutting `text` to at most `max_bytes` bytes (on a
/// character boundary) and marking the cut with [`TRUNCATED`]. For a source
/// with no bound of its own, such as a reply body.
pub fn sanitize_bounded(text: &str, max_bytes: usize) -> Cow<'_, str> {
    if text.len() <= max_bytes {
        return sanitize(text);
    }
    let mut cut = max_bytes;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = sanitize(&text[..cut]).into_owned();
    out.push_str(TRUNCATED);
    Cow::Owned(out)
}

/// [`sanitize`] for JSON faber serialized itself: every character a terminal
/// may act on becomes a JSON `\uXXXX` escape, so the output stays valid JSON
/// and decodes to the same value.
///
/// `serde_json` already escapes C0 controls inside strings, and between tokens
/// writes only spaces and newlines. What it leaves raw, and a terminal can act
/// on, is DEL, the C1 controls and the Unicode controls above, all of them
/// inside strings, where the `\u` escape is valid.
pub fn sanitize_json(json: &str) -> Cow<'_, str> {
    if !json.chars().any(is_unsafe) {
        return Cow::Borrowed(json);
    }
    let mut out = String::with_capacity(json.len() + 16);
    for c in json.chars() {
        if is_unsafe(c) {
            // Every unsafe character is in the Basic Multilingual Plane, so
            // one `\u` escape of four digits spells it.
            let _ = write!(out, "\\u{:04x}", u32::from(c));
        } else {
            out.push(c);
        }
    }
    Cow::Owned(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a terminal would act on comes out as text; what it would print
    /// comes out unchanged.
    #[test]
    fn sanitize_table() {
        let rows: &[(&str, &str, &str)] = &[
            ("plain ASCII", "draining 0", "draining 0"),
            ("newline and tab kept", "a\n\tb", "a\n\tb"),
            (
                "non-ASCII letters kept",
                "résumé · Київ — 東京",
                "résumé · Київ — 東京",
            ),
            ("ESC", "\u{1b}", "\\x1b"),
            ("BEL", "ding\u{7}", "ding\\x07"),
            ("NUL", "a\u{0}b", "a\\x00b"),
            ("CR, which overwrites the line", "ok\rFAIL", "ok\\x0dFAIL"),
            ("backspace", "yes\u{8}\u{8}\u{8}no", "yes\\x08\\x08\\x08no"),
            (
                "CSI: clear the screen, then colour",
                "\u{1b}[2J\u{1b}[31mred",
                "\\x1b[2J\\x1b[31mred",
            ),
            (
                "OSC 52: write the clipboard",
                "\u{1b}]52;c;cm0gLXJmIH4=\u{7}",
                "\\x1b]52;c;cm0gLXJmIH4=\\x07",
            ),
            (
                "OSC 0 with ST: retitle the window",
                "\u{1b}]0;sudo\u{1b}\\",
                "\\x1b]0;sudo\\x1b\\",
            ),
            ("DEL", "a\u{7f}b", "a\\x7fb"),
            ("C1 CSI, the 8-bit escape", "\u{9b}31m", "\\x9b31m"),
            ("C1 bounds", "\u{80}\u{9f}", "\\x80\\x9f"),
            (
                "first character past C1 kept",
                "\u{a0}\u{a9}",
                "\u{a0}\u{a9}",
            ),
            (
                "right-to-left override",
                "abc\u{202e}fed",
                "abc\\u{202e}fed",
            ),
            ("bidi isolate", "\u{2066}x\u{2069}", "\\u{2066}x\\u{2069}"),
            ("line separator", "a\u{2028}b", "a\\u{2028}b"),
            (
                "zero-width joiner in a word kept",
                "a\u{200d}b",
                "a\u{200d}b",
            ),
        ];
        for &(name, input, want) in rows {
            assert_eq!(sanitize(input), want, "{name}");
        }
    }

    /// Nothing to replace borrows; the output of `sanitize` needs nothing
    /// more, so applying it twice is applying it once.
    #[test]
    fn sanitize_borrows_clean_text_and_is_idempotent() {
        assert!(matches!(sanitize("clean · text"), Cow::Borrowed(_)));
        let once = sanitize("\u{1b}]52;c;x\u{7}\u{9b}\u{202e}").into_owned();
        assert_eq!(sanitize(&once), once);
        assert!(once.is_ascii());
    }

    #[test]
    fn a_bounded_sanitize_cuts_on_a_character_and_says_so() {
        assert_eq!(sanitize_bounded("short", 2048), "short");
        assert_eq!(sanitize_bounded("abcdef", 3), format!("abc{TRUNCATED}"));
        // "é" is two bytes: a cut in its middle backs off to before it.
        assert_eq!(sanitize_bounded("aébc", 2), format!("a{TRUNCATED}"));
        // The cut is taken before escaping, so an escape is never split.
        assert_eq!(
            sanitize_bounded("\u{1b}[2Jxyz", 4),
            format!("\\x1b[2J{TRUNCATED}")
        );
        let long = "\u{7}".repeat(10_000);
        let out = sanitize_bounded(&long, 2048);
        assert!(out.ends_with(TRUNCATED));
        assert_eq!(out.matches("\\x07").count(), 2048);
    }

    /// JSON stays JSON: the escapes decode back to the original value.
    #[test]
    fn sanitized_json_is_valid_and_decodes_to_the_same_value() {
        let value = serde_json::json!({
            "cause": "\u{1b}]52;c;cm0=\u{7} \u{9b}31m del\u{7f} \u{202e}rtl",
            "name": "résumé",
        });
        let raw = serde_json::to_string_pretty(&value).unwrap();
        let safe = sanitize_json(&raw);
        assert!(!safe.chars().any(is_unsafe), "{safe}");
        assert!(
            safe.contains("\\u009b") && safe.contains("\\u007f"),
            "{safe}"
        );
        assert!(
            safe.contains("\\u202e") && safe.contains("résumé"),
            "{safe}"
        );
        let back: serde_json::Value = serde_json::from_str(&safe).unwrap();
        assert_eq!(back, value);
        // Clean JSON is untouched.
        let clean = serde_json::to_string_pretty(&serde_json::json!({"a": 1})).unwrap();
        assert!(matches!(sanitize_json(&clean), Cow::Borrowed(_)));
    }
}
