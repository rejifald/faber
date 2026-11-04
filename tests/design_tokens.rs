//! The design-token gate: every surface colours through the one token source,
//! never through a literal.
//!
//! `docs/design/guidelines.md` § Colour is the rule, and ADR-0037 the decision:
//! a colour is a role defined once per theme in `desktop/src/tokens.css`, and
//! everything else reads it with `var()`. The React UI (ui/, ADR-0042) imports
//! that file, and `ui/scripts/theme.test.mjs` guards that side; the daemon
//! embeds it (`include_str!`) into the pages it still writes by hand. A literal
//! anywhere else does not follow the theme and drifts from the palette.
//!
//! It is a test rather than a lint for the same reason as `tests/privacy.rs`:
//! `cargo test` is what every contributor and agent already runs, and the
//! desktop frontend has no JS toolchain beyond the Tauri CLI.
//!
//! ## What is scanned
//!
//! - `desktop/src/tokens.css`, where a literal may sit only in a custom-property
//!   declaration, and where both theme blocks must define the same roles;
//! - every other `desktop/src/*.css`, where no literal may sit at all, not even
//!   in a custom property: a second token block is a second palette;
//! - every `<style>` block in the Rust sources under `src/` (the sign-in and
//!   standby pages), under the same rule;
//! - every page that has a `<style>` block reads the token source.
//!
//! ## What counts
//!
//! A hex colour (`#rgb`, `#rgba`, `#rrggbb`, `#rrggbbaa`) or a colour function
//! (`rgb()`, `hsl()`, `oklch()`, …) outside a comment. Named colours (`white`,
//! `red`) are not caught; `transparent` and `currentColor` are not colours to
//! tokenise. `color-mix()` over `var()`s is a derived colour, not a literal.
//!
//! ## A genuine one-off
//!
//! Put `design-tokens: allow (<reason>)` in a comment on the same line. The
//! reason is required: a marker without one fails, so every exception says why
//! it is not a token.

use std::fs;
use std::path::{Path, PathBuf};

const MARKER: &str = "design-tokens: allow (";

/// The one file a colour literal may live in, relative to the crate root.
const TOKEN_SOURCE: &str = "desktop/src/tokens.css";

/// What a page embedded in a Rust source writes to read the token source.
const TOKEN_INCLUDE: &str = "include_str!(\"../desktop/src/tokens.css\")";

/// Where a colour literal may sit in the text being scanned.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Scope {
    /// The token source: in a custom-property declaration, nowhere else.
    Tokens,
    /// Everything else: nowhere.
    Rules,
}

const COLOUR_FUNCTIONS: &[&str] = &[
    "rgb(", "rgba(", "hsl(", "hsla(", "hwb(", "lab(", "lch(", "oklab(", "oklch(",
];

/// Every line of `css` that uses a colour literal without a reasoned marker,
/// as `(line number, what is wrong)`.
fn violations(css: &str, scope: Scope) -> Vec<(usize, &'static str)> {
    let mut out = Vec::new();
    let mut in_comment = false;
    let mut in_custom_property = false;
    for (i, line) in css.lines().enumerate() {
        let marker = line.find(MARKER).map(|at| &line[at + MARKER.len()..]);
        let code = strip_comments(line, &mut in_comment);
        let trimmed = code.trim_start();
        if trimmed.starts_with("--") {
            in_custom_property = true;
        }
        let literal = has_colour_literal(&code) && !(scope == Scope::Tokens && in_custom_property);
        if code.contains(';') || code.contains('}') {
            in_custom_property = false;
        }
        match (literal, marker) {
            (false, _) => {}
            (true, None) => out.push((
                i + 1,
                match scope {
                    Scope::Tokens => "colour literal outside a custom property",
                    Scope::Rules => "colour literal outside the token source",
                },
            )),
            (true, Some(rest)) => {
                let reason = rest.split(')').next().unwrap_or("").trim();
                if reason.is_empty() {
                    out.push((i + 1, "allow marker without a reason"));
                }
            }
        }
    }
    out
}

/// `line` with `/* … */` comments removed, carrying an open comment across
/// lines in `in_comment`.
fn strip_comments(line: &str, in_comment: &mut bool) -> String {
    let mut out = String::new();
    let mut rest = line;
    loop {
        if *in_comment {
            match rest.find("*/") {
                Some(end) => {
                    rest = &rest[end + 2..];
                    *in_comment = false;
                }
                None => return out,
            }
        } else {
            match rest.find("/*") {
                Some(start) => {
                    out.push_str(&rest[..start]);
                    rest = &rest[start + 2..];
                    *in_comment = true;
                }
                None => {
                    out.push_str(rest);
                    return out;
                }
            }
        }
    }
}

fn has_colour_literal(code: &str) -> bool {
    let lower = code.to_ascii_lowercase();
    if COLOUR_FUNCTIONS.iter().any(|f| {
        lower.match_indices(f).any(|(at, _)| {
            // `rgb(` but not the tail of another identifier such as `--x-rgb(`.
            let before = at.checked_sub(1).map(|p| lower.as_bytes()[p]);
            before.is_none_or(|c| !c.is_ascii_alphanumeric() && c != b'-')
        })
    }) {
        return true;
    }
    let bytes = lower.as_bytes();
    bytes.iter().enumerate().any(|(at, &b)| {
        if b != b'#' {
            return false;
        }
        let digits = bytes[at + 1..]
            .iter()
            .take_while(|c| c.is_ascii_hexdigit())
            .count();
        let next = bytes.get(at + 1 + digits);
        let ends_word = next.is_none_or(|c| !c.is_ascii_alphanumeric() && *c != b'-' && *c != b'_');
        matches!(digits, 3 | 4 | 6 | 8) && ends_word
    })
}

/// The `<style>` blocks of `text`, as `(line number of the block's first line,
/// its contents)`, so a violation can be reported against the file it is in.
fn style_blocks(text: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let mut rest = text;
    let mut offset = 0;
    while let Some(open) = rest.find("<style>") {
        let body_at = open + "<style>".len();
        let Some(len) = rest[body_at..].find("</style>") else {
            break;
        };
        let line = text[..offset + body_at].lines().count().max(1);
        out.push((line, &rest[body_at..body_at + len]));
        offset += body_at + len;
        rest = &rest[body_at + len..];
    }
    out
}

/// The custom properties `block` declares, in order.
fn declared_roles(block: &str) -> Vec<&str> {
    block
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("--"))
        .filter_map(|l| l.split(':').next())
        .collect()
}

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn relative(file: &Path) -> String {
    file.strip_prefix(root())
        .unwrap_or(file)
        .display()
        .to_string()
}

/// Every file under `dir` (recursively) whose extension is `ext`.
fn files_with(dir: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in fs::read_dir(&d).expect("directory is readable") {
            let path = entry.expect("directory entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == ext) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

const HOW_TO_FIX: &str = "use a role from desktop/src/tokens.css, add one there in both theme \
     blocks, or mark a genuine one-off with `/* design-tokens: allow (<reason>) */` \
     (docs/design/guidelines.md § Colour, ADR-0037)";

#[test]
fn desktop_stylesheets_colour_through_the_token_source() {
    let dir = root().join("desktop/src");
    let sheets: Vec<_> = fs::read_dir(&dir)
        .expect("desktop/src is readable")
        .map(|e| e.expect("directory entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "css"))
        .collect();
    let mut failures = Vec::new();
    for file in &sheets {
        let css = fs::read_to_string(file).expect("stylesheet is UTF-8");
        let scope = if file.ends_with(TOKEN_SOURCE) {
            Scope::Tokens
        } else {
            Scope::Rules
        };
        for (line, what) in violations(&css, scope) {
            failures.push(format!("{}:{line}: {what}", relative(file)));
        }
    }
    assert!(
        file_is_in(&sheets, TOKEN_SOURCE),
        "expected {TOKEN_SOURCE} among the stylesheets in {}",
        dir.display()
    );
    assert!(
        failures.is_empty(),
        "{HOW_TO_FIX}:\n{}",
        failures.join("\n")
    );
}

/// Whether `relative` (to the crate root) is one of `files`.
fn file_is_in(files: &[PathBuf], relative: &str) -> bool {
    files.iter().any(|f| f.ends_with(relative))
}

#[test]
fn served_pages_colour_through_the_token_source() {
    let mut pages = Vec::new();
    let mut failures = Vec::new();
    for file in files_with(&root().join("src"), "rs") {
        let source = fs::read_to_string(&file).expect("source is UTF-8");
        let blocks = style_blocks(&source);
        if blocks.is_empty() {
            continue;
        }
        let name = relative(&file);
        if source.matches(TOKEN_INCLUDE).count() < blocks.len() {
            failures.push(format!(
                "{name}: a page with a <style> block does not embed the token source \
                 (`concat!(…, {TOKEN_INCLUDE}, …)` at the top of the block)"
            ));
        }
        for (line, block) in blocks {
            for (at, what) in violations(block, Scope::Rules) {
                failures.push(format!("{name}:{}: {what}", line + at - 1));
            }
        }
        pages.push(name);
    }
    for expected in ["src/control.rs", "src/standby.rs"] {
        assert!(
            pages.iter().any(|p| p == expected),
            "expected a <style> block in {expected}; found pages in {pages:?}"
        );
    }
    assert!(
        failures.is_empty(),
        "{HOW_TO_FIX}:\n{}",
        failures.join("\n")
    );
}

#[test]
fn both_themes_define_the_same_roles() {
    let css = fs::read_to_string(root().join(TOKEN_SOURCE)).expect("token source is readable");
    let dark_at = css
        .find("@media (prefers-color-scheme: dark)")
        .expect("the token source has a dark block");
    let light = declared_roles(&css[..dark_at]);
    let dark = declared_roles(&css[dark_at..]);
    let mut light_sorted = light.clone();
    light_sorted.sort_unstable();
    let mut dark_sorted = dark.clone();
    dark_sorted.sort_unstable();
    assert!(
        light.len() >= 20,
        "expected the palette in {TOKEN_SOURCE}, found {light:?}"
    );
    assert_eq!(
        light_sorted, dark_sorted,
        "every role in {TOKEN_SOURCE} needs a value in both the light and the dark block"
    );
}

#[test]
fn a_literal_in_a_rule_fails() {
    assert_eq!(violations(".a { color: #ff6a00; }", Scope::Rules).len(), 1);
    assert_eq!(violations(".a { color: #FFF; }", Scope::Rules).len(), 1);
    assert_eq!(
        violations(
            ".a { box-shadow: 0 1px 2px rgba(0, 0, 0, 0.1); }",
            Scope::Rules
        )
        .len(),
        1
    );
    assert_eq!(
        violations(
            ".a {\n  background: linear-gradient(\n    #000 0,\n    red 100%\n  );\n}",
            Scope::Rules
        )
        .len(),
        1
    );
}

#[test]
fn tokens_and_var_reads_pass_in_the_token_source() {
    let css = ":root {\n  --bg: #f4f5f7;\n  --shadow-float: 0 1px 2px rgba(15, 20, 25, 0.06),\n    0 16px 42px rgba(15, 20, 25, 0.2);\n}\n.a { color: var(--bg); }";
    assert!(violations(css, Scope::Tokens).is_empty());
    assert_eq!(violations(".a { color: #fff; }", Scope::Tokens).len(), 1);
}

#[test]
fn a_token_block_outside_the_token_source_fails() {
    let css = ":root {\n  --bg: #f4f5f7;\n  --radius: 12px;\n}\n.a { color: var(--bg); }";
    assert_eq!(
        violations(css, Scope::Rules),
        vec![(2, "colour literal outside the token source")]
    );
    assert!(violations(
        ".a { border-color: color-mix(in srgb, var(--red) 32%, transparent); }",
        Scope::Rules
    )
    .is_empty());
}

#[test]
fn style_blocks_report_the_line_they_start_on() {
    let page = "const P: &str = r#\"<html>\n<style>\n  a { color: #fff; }\n</style>\n<style>b{}</style>\"#;";
    let blocks = style_blocks(page);
    assert_eq!(blocks.len(), 2);
    let (line, css) = blocks[0];
    let (at, _) = violations(css, Scope::Rules)[0];
    assert_eq!(line + at - 1, 3);
    assert_eq!(blocks[1], (5, "b{}"));
}

#[test]
fn comments_and_id_selectors_are_not_colours() {
    assert!(violations(
        "/* was #ff0000, see #156 */\n.a { color: var(--x); }",
        Scope::Rules
    )
    .is_empty());
    assert!(violations("/* a long\n   comment with rgb(1, 2, 3)\n*/", Scope::Rules).is_empty());
    assert!(violations("#po-panel, #more { color: var(--x); }", Scope::Rules).is_empty());
}

#[test]
fn a_marker_needs_a_reason() {
    assert!(violations(
        ".a { color: #000; } /* design-tokens: allow (mask stop) */",
        Scope::Rules
    )
    .is_empty());
    assert_eq!(
        violations(
            ".a { color: #000; } /* design-tokens: allow () */",
            Scope::Rules
        ),
        vec![(1, "allow marker without a reason")]
    );
}

// --- contrast ----------------------------------------------------------------
//
// WCAG AA over the token pairs faber draws (docs/design/guidelines.md § Colour,
// owner 2026-10-18): 4.5:1 for text, 3:1 for large text and UI marks, and 3:1
// for --faint text, a product override. Checked in both themes, on every page
// surface a pair can sit on. --accent is a mark and a fill (the gauge arc,
// the budget bar, the primary behind --accent-ink, the logo), so it is held
// to 3:1; --accent-text draws accent-coloured text.

/// An sRGB colour with alpha, from a token value.
#[derive(Clone, Copy, Debug)]
struct Rgba([f64; 3], f64);

/// The colour a token declares, if its value is a single `#rrggbb` or `rgba()`.
fn parse_colour(value: &str) -> Option<Rgba> {
    let v = value.trim();
    if let Some(hex) = v.strip_prefix('#') {
        if hex.len() != 6 {
            return None;
        }
        let ch = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok().map(f64::from);
        return Some(Rgba([ch(0)?, ch(2)?, ch(4)?], 1.0));
    }
    let inner = v.strip_prefix("rgba(")?.strip_suffix(')')?;
    let parts: Vec<f64> = inner
        .split(',')
        .map(|p| p.trim().parse().ok())
        .collect::<Option<_>>()?;
    match parts[..] {
        [r, g, b, a] => Some(Rgba([r, g, b], a)),
        _ => None,
    }
}

/// `role → colour` for one theme block, colours only.
fn palette(block: &str) -> std::collections::HashMap<String, Rgba> {
    block
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("--"))
        .filter_map(|l| {
            let (name, rest) = l.split_once(':')?;
            let value = rest.split(';').next()?.split("/*").next()?;
            Some((name.trim().to_string(), parse_colour(value)?))
        })
        .collect()
}

/// `top` composited over the opaque `under`.
fn over(top: Rgba, under: Rgba) -> Rgba {
    let Rgba(t, a) = top;
    let Rgba(u, _) = under;
    Rgba([0, 1, 2].map(|i| t[i] * a + u[i] * (1.0 - a)), 1.0)
}

fn luminance(c: Rgba) -> f64 {
    let lin = |v: f64| {
        let s = v / 255.0;
        if s <= 0.03928 {
            s / 12.92
        } else {
            ((s + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c.0[0]) + 0.7152 * lin(c.0[1]) + 0.0722 * lin(c.0[2])
}

fn contrast(a: Rgba, b: Rgba) -> f64 {
    let (la, lb) = (luminance(a), luminance(b));
    (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
}

const TEXT: f64 = 4.5;
const LARGE_OR_MARK: f64 = 3.0;

/// The page surfaces anything can sit on.
const SURFACES: &[&str] = &["bg", "surface", "surface-2"];

/// Translucent row tints (`--row-hover`, `--row-press`), checked composited over `--bg`: the
/// popover card, where list rows sit. Every role in [`ON_SURFACES`] must hold its tier on them.
const ROW_TINTS: &[&str] = &["row-hover", "row-press"];

/// `(foreground role, minimum)` drawn on every surface in [`SURFACES`] and row tint in [`ROW_TINTS`].
const ON_SURFACES: &[(&str, f64, &str)] = &[
    ("fg", TEXT, "body text"),
    ("fg-strong", TEXT, "headings and emphasis"),
    ("muted", TEXT, "secondary text"),
    ("accent-text", TEXT, "links and accent-coloured text"),
    ("green", TEXT, "ok and running text"),
    ("red", TEXT, "errors and refused text"),
    ("amber", TEXT, "warnings and fault text"),
    // A product override, not a WCAG tier: faint text is held to 3:1
    // (docs/design/guidelines.md § Colour, owner 2026-10-18).
    (
        "faint",
        LARGE_OR_MARK,
        "hints, timestamps and other faint text",
    ),
    // UI marks: focus rings, tab underlines, checkboxes, status dots.
    (
        "accent-text",
        LARGE_OR_MARK,
        "focus rings, tab underlines and checkboxes",
    ),
    (
        "border-control",
        LARGE_OR_MARK,
        "a control's boundary: checkbox, input, select, outlined button",
    ),
    ("green", LARGE_OR_MARK, "running dots"),
    ("red", LARGE_OR_MARK, "error dots"),
    ("amber", LARGE_OR_MARK, "warning dots and note markers"),
    (
        "accent",
        LARGE_OR_MARK,
        "the gauge arc, budget bar, fills and logo",
    ),
];

/// `(foreground, background, minimum, what)` for a pair on one fill. A `-soft`
/// background is composited over `surface`, where its chips sit.
const ON_FILLS: &[(&str, &str, f64, &str)] = &[
    ("accent-ink", "accent", TEXT, "the primary button's label"),
    (
        "surface",
        "accent-text",
        LARGE_OR_MARK,
        "the tick in a checked checkbox",
    ),
    (
        "accent-text",
        "accent-soft",
        TEXT,
        "the panel's PR chip, pinned badge, and the selected dashboard nav row with its count",
    ),
    ("green", "green-soft", TEXT, "running pill"),
    ("red", "red-soft", TEXT, "offline and refused badges"),
    ("amber", "amber-soft", TEXT, "paused and fault badges"),
];

#[test]
fn token_pairs_meet_wcag_aa_in_both_themes() {
    let css = fs::read_to_string(root().join(TOKEN_SOURCE)).expect("token source is readable");
    let dark_at = css
        .find("@media (prefers-color-scheme: dark)")
        .expect("the token source has a dark block");
    let mut failures = Vec::new();
    for (theme, block) in [("light", &css[..dark_at]), ("dark", &css[dark_at..])] {
        let p = palette(block);
        let get = |role: &str| -> Rgba {
            *p.get(role).unwrap_or_else(|| {
                panic!("{TOKEN_SOURCE}: no colour --{role} in the {theme} block")
            })
        };
        let mut check_over = |fg: &str, bg: &str, ground: &str, min: f64, what: &str| {
            let back = over(get(bg), get(ground));
            let ratio = contrast(over(get(fg), back), back);
            if ratio < min {
                failures.push(format!(
                    "{theme}: --{fg} on --{bg} is {ratio:.2}:1, needs {min}:1 ({what})"
                ));
            }
        };
        for &(fg, min, what) in ON_SURFACES {
            for bg in SURFACES {
                check_over(fg, bg, "surface", min, what);
            }
            for tint in ROW_TINTS {
                check_over(fg, tint, "bg", min, what);
            }
        }
        for &(fg, bg, min, what) in ON_FILLS {
            check_over(fg, bg, "surface", min, what);
        }
    }
    assert!(
        failures.is_empty(),
        "WCAG AA (docs/design/guidelines.md § Colour): change the role's value in both \
         blocks of {TOKEN_SOURCE}, or draw it with a role that passes:\n{}",
        failures.join("\n")
    );
}

#[test]
fn contrast_matches_the_wcag_formula() {
    let white = Rgba([255.0, 255.0, 255.0], 1.0);
    let black = Rgba([0.0, 0.0, 0.0], 1.0);
    assert!((contrast(white, black) - 21.0).abs() < 1e-9);
    let mid = parse_colour("#767676").unwrap();
    assert!((contrast(mid, white) - 4.54).abs() < 0.01);
    let half = parse_colour("rgba(0, 0, 0, 0.5)").unwrap();
    assert!((over(half, white).0[0] - 127.5).abs() < 1e-9);
}
