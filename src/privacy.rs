//! The privacy scanner shared by the tree gate (`tests/privacy.rs`) and the text
//! gate (`examples/privacy_scan.rs`, run from `.github/workflows/privacy-text.yml`
//! and `release.yml`).
//!
//! One implementation, deliberately: a second copy of the digest scheme would
//! drift, and the text gate would silently stop agreeing with the tree gate.
//!
//! ## How the denylist stays secret
//!
//! A denylist of private names cannot be committed in the clear — publishing the
//! list would publish the very names it exists to protect. So [`DENIED`] holds
//! only keyed digests: HMAC-SHA256 under a key that is never committed
//! (ADR-0058). A scan splits text into tokens, digests each distinct token under
//! the same key, and compares digests.
//!
//! The key is what makes publishing the digests safe. An unkeyed digest, salted
//! or not, of any name of eight letters and digits falls to exhaustive search in
//! minutes on one GPU, with no dictionary and no prior suspicion needed. A keyed
//! digest cannot be checked against even one guess without the key.
//!
//! The key is held in two places only: each repository's `FABER_PRIVACY_KEY`
//! Actions secret, and the owner's environment. Where it is absent the name
//! check cannot run, so a gate says so loudly ([`announce_skip`]) instead of
//! passing in silence. Where CI sets `FABER_PRIVACY_KEY_REQUIRED=true`, an absent
//! key fails the gate instead.
//!
//! A hit reports a location and **never the token**, and a gate passes every
//! path it prints through [`Denylist::mask`] first: output lands in public CI
//! logs.
//!
//! ## Keeping the list current
//!
//! Nobody keeps the list by hand. `examples/privacy_denylist.rs` derives the
//! names from what the owner's machine can see (the `gh` account's private
//! repositories and packages, its organisations, the machine's host names) and
//! adds every missing digest to [`DENIED`]. With the key exported (ADR-0058):
//!
//! ```sh
//! cargo run --example privacy_denylist -- --write
//! ```
//!
//! For one name no source shows, read it without echo, so it reaches neither
//! the screen nor shell history, and paste the digest into [`DENIED`]:
//!
//! ```sh
//! printf 'name: ' && read -rs FABER_PRIVACY_NAME && echo &&
//!   FABER_PRIVACY_NAME=$FABER_PRIVACY_NAME \
//!   cargo test --test privacy -- --ignored --nocapture print_digest
//! unset FABER_PRIVACY_NAME
//! ```
//!
//! Never write a name itself into a file, a comment, or a commit message.

use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::io::Write;
use std::ops::Range;

use sha2::digest::generic_array::GenericArray;
use sha2::{Digest, Sha256};

/// Keyed digests ([`Key::digest`]) of names that must never appear in the tree:
/// private repos, internal project code names, private image names. Kept
/// current by `examples/privacy_denylist.rs` (see *Keeping the list current*
/// above) — never by hand, and never with a name in shell history you intend to
/// keep.
pub const DENIED: &[&str] = &[
    "0983d821733d5370e1c88fc14efed4a15e88a128b270c5b2719736dcb3a1b9fc",
    "225bb5d07643ae16c389bbd35e266056c1a2d63d2141d67b5b3050dccc4ae692",
    "237288c94026ce2d7bb5aa139d84cd61a4aed6e995b1d05b18ad28fb75263171",
    "29e87b7524db634cdfb736890c8ee1408033fa9064aa9b9768431f0f4c377054",
    "3879e9d78d1cde7ad962942f661e7dadfe40d61e2ce9037b4f2c662b7ac5aac6",
    "4595abc18f17d4f1ad42ad2bf438f85d393528aba0ef4d83b54e229571b9a734",
    "4b917067033c9eb3ccca94d3e2b0ad6b2a1abec9fde81e625deafa68412f537a",
    "52ad6bd3650a51c7f66fb23f3bfc65d1be15c3ad49a414998666f276309c1256",
    "588e1c5e3c1fa93faf6ce5a093f62ffe49eda6c4e3163721d6fdcacdf25dec4f",
    "6293f2955848260095efaf7caf4d86d67cb697647fcafdab3d0b2b5ff35a2a8b",
    "667f5b6903efea2e91a5ce498ecd867acc27aef4ab270130a187a02bd9f08e6b",
    "69e104436193ac16b896ab988cfb6deac7cf8e722e0d78200ea144acf7c1a0ec",
    "6ff82733eebccce2818ea581c97efbc8354cf1ef32a38f6e9ecb8f39dafa606d",
    "7441c13eef13f74f111898a81536028e3d26bc02677a49ed04da7fa0cbeb9dd9",
    "75ec808950936e2ffe29f2689eb654915985c38c57e5f33071aae906790f9040",
    "79dc92bb4aebdd86b03182960ec622ac55d2b4bbd63cabf66c62ae5714bfaf1a",
    "87cf28c814d076d0b9b797a5a1cf4f1d267951d55d754305d098b225a9a4318f",
    "88f48aba026a909af869ca55cd9e2ff837c16baa65b7256665e57e5b6c1ce595",
    "8d06788ecd5ff2035824743e66a01e96e13196655a44f85f38a286633e1256f1",
    "9043efcbd0180c6af8ae6d2d4e90872d248b916563774eeee551325bf9c0ac14",
    "93602ca5aa9f95b48c3bcedd14c28ba2ce56829eb7bc31e84b6cee8d2fded2ac",
    "9e1bf11ff6ddeb44949d2ac661439cdcbe28aa904c82bcd2256b3dba557c1f7c",
    "b16bbb14e8f646e1a7cf3e4c943e645529a6bf4e29fc2d98a84175d9b9795cce",
    "b2e8f64610661112dd30c29723d7c4568e7151f73fe3b0afa4358dfecf0ac4bd",
    "b5d75b4c7ea8af4137bf3585ae58106a002adddc09a12589dcf9179d6ed1ffeb",
    "c2d64f23e4ede5ccaefa8ad72da2c769138ef3a8412568ba5ad0bff2f7dba53e",
    "d2b0ce98750554ec60d41338ee43c1c1d8927851509234668ad33b59ea885aba",
    "d5d1013273ae2fe1ca9a76818414f8b374b0a4e82933b45153ad18a3d0f8f3d0",
    "f22a24831b8d1ef36968390b8c89d8d23b9f9df28df31bc6c39c432173f1a02b",
    "ff8bd9cfe26f88537a9c6c68d08a2fb061d169ead6762aa5e5638c4ba7465edc",
];

/// The token a name is listed as: its ASCII letters and digits, lowercased, with
/// everything between them dropped. The scan joins words across separators, so
/// `Widget-Co`, `widget_co` and `WidgetCo` in text all meet `widgetco` here.
/// Every tool that lists a name goes through this, so a digest always agrees
/// with what the scan computes.
pub fn name_token(name: &str) -> String {
    name.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// The environment variable that holds the key, as hex digits.
pub const KEY_ENV: &str = "FABER_PRIVACY_KEY";

/// Set to `true` where the key must be present, so that its absence fails the
/// gate instead of skipping the name check. CI sets it on every run that GitHub
/// passes secrets to.
pub const KEY_REQUIRED_ENV: &str = "FABER_PRIVACY_KEY_REQUIRED";

/// Shortest key accepted, in bytes. It is SHA-256's output size: any shorter,
/// and the key becomes easier to guess than a digest.
pub const MIN_KEY_BYTES: usize = 32;

/// Shortest token worth hashing: nothing shorter is digested or listed.
/// Skipping the noise keeps the scan to a few seconds in a debug build.
pub const MIN_TOKEN: usize = 4;

/// Most adjacent words joined into one token, so a name of up to four words is
/// caught however its words are separated: `widget-data-platform`,
/// `WidgetDataPlatform` and `widget_data_platform` all yield
/// `widgetdataplatform`.
pub const MAX_JOIN: usize = 4;

/// What a masked name prints as: the mark GitHub prints for a hidden secret.
pub const MASK: &str = "***";

/// Printed wherever the key is absent. A gate that skipped in silence would look
/// exactly like one that passed.
pub const SKIP_NOTICE: &str = "PRIVACY GATE SKIPPED: FABER_PRIVACY_KEY is not set, so private \
     names were NOT checked (credentials and home paths still were). A pass here proves nothing \
     about private names. Export the key (ADR-0058) or rely on CI, which holds it.";

/// Never walked: not published (`target`, `node_modules`) or not content at all
/// (`.git`). Matched by name against files as well as directories, because in a
/// git *worktree* `.git` is a file — it holds a `gitdir:` line pointing at an
/// absolute path in someone's home, which is exactly the shape
/// [`carries_personal_path`] exists to flag.
pub const SKIP_ENTRIES: &[&str] = &[".git", "target", "node_modules"];

/// Credential shapes. Each must be followed by a long run of token characters
/// to count — prose that merely mentions a prefix is not a leak.
pub const TOKEN_PREFIXES: &[(&str, usize)] = &[
    ("ghp_", 20),
    ("gho_", 20),
    ("ghu_", 20),
    ("ghs_", 20),
    ("ghr_", 20),
    ("github_pat_", 20),
    ("AKIA", 16),
    ("xoxb-", 20),
    ("sk-", 32),
];

// Kept on separate lines on purpose: a single line holding both would make this
// file trip its own PEM check.
pub const PEM_HEAD: &str = "-----BEGIN";
pub const PEM_TAIL: &str = "PRIVATE KEY";

/// Home-directory names that describe a role or a fixture rather than a person,
/// under every root in [`HOME_ROOTS`]. `runner` is the account GitHub's hosted
/// runners use: `/Users/runner` on macOS, `/home/runner` on Linux.
/// `/Users/ci` predates ADR-0025, which replaced that isolated-deploy user
/// with the hidden `_faber` / `_faber_vm` role accounts (`src/setup/`) — it
/// stays generic here only because this suite's own tests still write it as a
/// role-path example (see `role_home_directories_are_not_personal` in this
/// file's tests).
/// `/Users/tester` is the stand-in `$HOME` that `platform::macos`'s
/// literal-path regression test sets — neither names anyone. `/Users/root` is
/// macOS's own superuser, read as a directory record (`dscl . -read /Users/root`)
/// by the guest agent's verification checks (ADR-0049 §7).
///
/// Keep this list short and obviously-not-a-person. Every addition is a small
/// hole in the check, and the check only has to catch one thing: a real
/// operator's home directory reaching a public tree.
pub const GENERIC_USERS: &[&str] = &[
    "ci", "user", "you", "me", "owner", "runner", "root", "someone", "test", "tester",
];

/// Where people's home directories live: `/Users/` on macOS, `/home/` on Linux.
pub const HOME_ROOTS: &[&str] = &["/Users/", "/home/"];

/// SHA-256's block size, which HMAC pads the key to.
const BLOCK: usize = 64;

/// SHA-256's initial hash value (FIPS 180-4, 5.3.3).
const SHA256_INIT: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// An HMAC-SHA256 key (RFC 2104), held as the two SHA-256 states its padded
/// blocks leave, so a short token's digest costs two compressions. They run
/// through `sha2`'s raw compression function, because the tree gate digests
/// millions of tokens on every keyed `cargo test`, and in a debug build the
/// generic hashing API costs three times as much per digest.
#[derive(Clone)]
pub struct Key {
    inner: [u32; 8],
    outer: [u32; 8],
}

impl Key {
    /// A key from raw bytes. One longer than a block is hashed first, as RFC
    /// 2104 requires.
    fn new(secret: &[u8]) -> Key {
        let mut block = [0u8; BLOCK];
        if secret.len() > BLOCK {
            block[..32].copy_from_slice(&Sha256::digest(secret));
        } else {
            block[..secret.len()].copy_from_slice(secret);
        }
        let mut inner = SHA256_INIT;
        compress(&mut inner, &block.map(|b| b ^ 0x36));
        let mut outer = SHA256_INIT;
        compress(&mut outer, &block.map(|b| b ^ 0x5c));
        Key { inner, outer }
    }

    /// A key from its hex form, as the environment carries it. Surrounding
    /// whitespace is ignored: a secret piped or pasted in often ends in a newline.
    pub fn from_hex(text: &str) -> Result<Key, GateError> {
        let bytes = decode_hex(text.trim()).ok_or(GateError::KeyNotHex)?;
        if bytes.len() < MIN_KEY_BYTES {
            return Err(GateError::KeyTooShort);
        }
        Ok(Key::new(&bytes))
    }

    /// The keyed digest of `token`, in hex, as [`DENIED`] stores it.
    pub fn digest(&self, token: &str) -> String {
        self.digest_bytes(token)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect()
    }

    fn digest_bytes(&self, token: &str) -> [u8; 32] {
        let inner = finish(self.inner, token.as_bytes());
        finish(self.outer, &inner)
    }
}

impl std::fmt::Debug for Key {
    // Never the key material, nor the hash states derived from it: debug output
    // reaches logs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Key(..)")
    }
}

/// Absorbs one block into `state`.
fn compress(state: &mut [u32; 8], block: &[u8]) {
    sha2::compress256(state, &[*GenericArray::from_slice(block)]);
}

/// Finishes a SHA-256 hash whose `state` has absorbed exactly one block, the
/// key's, over `message`: its whole blocks, then FIPS 180-4's padding, a 1 bit,
/// zeros, and the length in bits, counting the key's block too. The padding
/// takes a second block when the message's tail leaves no room for the length.
fn finish(mut state: [u32; 8], message: &[u8]) -> [u8; 32] {
    let (blocks, rest) = message.as_chunks::<BLOCK>();
    for block in blocks {
        compress(&mut state, block);
    }
    let mut tail = [0u8; 2 * BLOCK];
    tail[..rest.len()].copy_from_slice(rest);
    tail[rest.len()] = 0x80;
    let end = if rest.len() < BLOCK - 8 {
        BLOCK
    } else {
        2 * BLOCK
    };
    let bits = ((BLOCK + message.len()) as u64) * 8;
    tail[end - 8..end].copy_from_slice(&bits.to_be_bytes());
    for block in tail[..end].as_chunks::<BLOCK>().0 {
        compress(&mut state, block);
    }
    let mut out = [0u8; 32];
    for (bytes, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        *bytes = word.to_be_bytes();
    }
    out
}

/// `text` as bytes, two hex digits each, either case. `None` for anything else.
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let digits: Vec<u8> = text
        .chars()
        .map(|c| c.to_digit(16).map(|d| d as u8))
        .collect::<Option<_>>()?;
    let (pairs, odd) = digits.as_chunks::<2>();
    if !odd.is_empty() {
        return None;
    }
    Some(pairs.iter().map(|[high, low]| (high << 4) | low).collect())
}

/// Why a gate cannot run its name check. Deliberately carries no key material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateError {
    KeyNotHex,
    KeyTooShort,
    KeyMissing,
    NothingListed,
}

impl GateError {
    pub fn describe(self) -> &'static str {
        match self {
            GateError::KeyNotHex => "FABER_PRIVACY_KEY is not an even number of hex digits",
            GateError::KeyTooShort => "FABER_PRIVACY_KEY is shorter than 32 bytes (64 hex digits)",
            GateError::KeyMissing => {
                "FABER_PRIVACY_KEY is not set, and FABER_PRIVACY_KEY_REQUIRED says it must be: \
                 set the repository's FABER_PRIVACY_KEY Actions secret (ADR-0058)"
            }
            GateError::NothingListed => {
                "FABER_PRIVACY_KEY is set but DENIED holds no digest, so nothing would be \
                 checked: fill it with `cargo run --example privacy_denylist -- --write`, or \
                 add one name with print_digest (see src/privacy.rs)"
            }
        }
    }
}

/// The key in [`KEY_ENV`]. `Ok(None)` when it is unset or blank and
/// [`KEY_REQUIRED_ENV`] does not demand it.
pub fn key_from_env() -> Result<Option<Key>, GateError> {
    let required = std::env::var(KEY_REQUIRED_ENV).is_ok_and(|v| v.trim() == "true");
    key_from(std::env::var_os(KEY_ENV).as_deref(), required)
}

fn key_from(value: Option<&OsStr>, required: bool) -> Result<Option<Key>, GateError> {
    let text = match value {
        Some(value) => value.to_str().ok_or(GateError::KeyNotHex)?,
        None => "",
    };
    if !text.trim().is_empty() {
        return Key::from_hex(text).map(Some);
    }
    if required {
        return Err(GateError::KeyMissing);
    }
    Ok(None)
}

/// [`DENIED`] under the key in [`KEY_ENV`]. `Ok(None)` when there is no key and
/// none is required: the caller then runs no name check, and says so with
/// [`announce_skip`].
pub fn denylist_from_env() -> Result<Option<Denylist>, GateError> {
    denylist_from(key_from_env()?, DENIED)
}

fn denylist_from(key: Option<Key>, digests: &[&str]) -> Result<Option<Denylist>, GateError> {
    let Some(key) = key else {
        return Ok(None);
    };
    if digests.is_empty() {
        return Err(GateError::NothingListed);
    }
    Ok(Some(Denylist::new(key, digests)))
}

/// Print [`SKIP_NOTICE`] where it cannot be missed: as a warning annotation on
/// GitHub Actions, and straight to stderr everywhere, which the test harness
/// does not capture, so it shows even when the test passes.
pub fn announce_skip() {
    let line = if std::env::var_os("GITHUB_ACTIONS").is_some() {
        format!("::warning title=Privacy gate skipped::{SKIP_NOTICE}\n")
    } else {
        format!("\n!!! {SKIP_NOTICE}\n\n")
    };
    // Not `eprintln!`: the test harness would capture that, and hide it.
    std::io::stderr()
        .write_all(line.as_bytes())
        .expect("write the privacy gate's skip notice to stderr");
}

/// Digests under one key: says whether text names anything listed, and hides
/// what it names. It remembers each token's verdict, so a word repeated across
/// a tree is digested once.
pub struct Denylist {
    key: Key,
    denied: HashSet<[u8; 32]>,
    verdicts: HashMap<String, bool>,
}

impl Denylist {
    /// # Panics
    ///
    /// On a digest that is not 64 hex digits, which `print_digest` never
    /// prints: a malformed entry would otherwise check nothing, in silence.
    pub fn new(key: Key, digests: &[&str]) -> Denylist {
        let denied = digests
            .iter()
            .map(|d| {
                decode_hex(d)
                    .and_then(|bytes| <[u8; 32]>::try_from(bytes).ok())
                    .expect("every digest in DENIED is 64 hex digits, as print_digest prints them")
            })
            .collect();
        Denylist {
            key,
            denied,
            verdicts: HashMap::new(),
        }
    }

    fn denies(&mut self, token: &str) -> bool {
        if let Some(verdict) = self.verdicts.get(token) {
            return *verdict;
        }
        let verdict = self.denied.contains(&self.key.digest_bytes(token));
        self.verdicts.insert(token.to_owned(), verdict);
        verdict
    }

    /// Whether any token of `text` is listed.
    pub fn names_any(&mut self, text: &str) -> bool {
        each_token(text, |token, _| self.denies(token))
    }

    /// `text` with every listed name in it replaced by [`MASK`], for printing a
    /// path into a public log.
    pub fn mask(&mut self, text: &str) -> String {
        let mut spans: Vec<Range<usize>> = Vec::new();
        each_token(text, |token, span| {
            if self.denies(token) {
                spans.push(span);
            }
            false
        });
        spans.sort_by_key(|span| span.start);
        let mut merged: Vec<Range<usize>> = Vec::new();
        for span in spans {
            match merged.last_mut() {
                Some(last) if span.start <= last.end => last.end = last.end.max(span.end),
                _ => merged.push(span),
            }
        }
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        for span in merged {
            out.push_str(&text[at..span.start]);
            out.push_str(MASK);
            at = span.end;
        }
        out.push_str(&text[at..]);
        out
    }
}

/// A token, and the bytes of the text it was read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub text: String,
    pub span: Range<usize>,
}

/// Every token a scan digests, lowercased: each word, each join of up to
/// [`MAX_JOIN`] adjacent words, and each alphanumeric run whole. Joins cross
/// separators, so `Some Thing`, `SomeThing`, `some-thing` and `some_thing` all
/// yield `something`. Splitting runs into words finds a name inside
/// `WidgetcoClient` or `widgetco2`. Tokens shorter than [`MIN_TOKEN`] are left
/// out.
pub fn tokens(text: &str) -> Vec<Token> {
    let mut out = Vec::new();
    each_token(text, |token, span| {
        out.push(Token {
            text: token.to_owned(),
            span,
        });
        false
    });
    out
}

/// Calls `visit` with each of [`tokens`]' tokens and its span, without
/// allocating one, until `visit` returns `true`. Returns whether it did.
fn each_token(text: &str, mut visit: impl FnMut(&str, Range<usize>) -> bool) -> bool {
    // ASCII lowercasing changes no byte offsets, so spans index both strings.
    let lower = text.to_ascii_lowercase();
    let mut words: Vec<Range<usize>> = Vec::new();
    // A run of more words than a join reaches is digested whole as well. A
    // shorter run is already one of the joins, with the same span.
    let mut whole: Vec<Range<usize>> = Vec::new();
    for run in runs(text) {
        let before = words.len();
        split_run(text.as_bytes(), run.clone(), &mut words);
        if words.len() - before > MAX_JOIN {
            whole.push(run);
        }
    }
    let mut joined = String::new();
    for (i, first) in words.iter().enumerate() {
        joined.clear();
        for word in words[i..].iter().take(MAX_JOIN) {
            joined.push_str(&lower[word.clone()]);
            if joined.len() >= MIN_TOKEN && visit(&joined, first.start..word.end) {
                return true;
            }
        }
    }
    whole.into_iter().any(|run| visit(&lower[run.clone()], run))
}

/// The words of `text`, as byte ranges: its ASCII alphanumeric runs, each split
/// where camel case or a change between letters and digits starts a new word.
pub fn words(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    for run in runs(text) {
        split_run(text.as_bytes(), run, &mut out);
    }
    out
}

/// The ASCII alphanumeric runs of `text`, as byte ranges. Everything else,
/// non-ASCII letters included, separates.
fn runs(text: &str) -> Vec<Range<usize>> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices() {
        match (c.is_ascii_alphanumeric(), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                out.push(s..i);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(s) = start {
        out.push(s..text.len());
    }
    out
}

/// One run split into words, pushed onto `out`: before a capital that follows
/// a lower-case letter (`widgetClient`), before the last capital of a run of
/// capitals that a lower-case letter follows (`HTTPServer`), and wherever
/// letters and digits meet (`widget2`, `v2widget`).
fn split_run(text: &[u8], run: Range<usize>, out: &mut Vec<Range<usize>>) {
    let mut start = run.start;
    for i in run.start + 1..run.end {
        let (prev, cur) = (text[i - 1], text[i]);
        let camel = prev.is_ascii_lowercase() && cur.is_ascii_uppercase();
        // The byte after a run is never lower-case ASCII, so this cannot look past it.
        let acronym_end = prev.is_ascii_uppercase()
            && cur.is_ascii_uppercase()
            && text.get(i + 1).is_some_and(u8::is_ascii_lowercase);
        let digit_edge = prev.is_ascii_digit() != cur.is_ascii_digit();
        if camel || acronym_end || digit_edge {
            out.push(start..i);
            start = i;
        }
    }
    out.push(start..run.end);
}

/// A prefix followed by a long enough run of token characters to be a real
/// credential rather than documentation of the prefix.
pub fn carries_secret(line: &str) -> bool {
    for (prefix, min_tail) in TOKEN_PREFIXES {
        let hit = line.match_indices(prefix).any(|(i, _)| {
            // At a token boundary only: without this, `sk-` matches the tail of
            // any long `...task-name-like-this` identifier.
            let boundary = line[..i]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_' && c != '-');
            boundary
                && line[i + prefix.len()..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
                    .count()
                    >= *min_tail
        });
        if hit {
            return true;
        }
    }
    line.contains(PEM_HEAD) && line.contains(PEM_TAIL)
}

/// `/Users/<somebody>` or `/home/<somebody>` where somebody is a person rather
/// than a role.
pub fn carries_personal_path(line: &str) -> bool {
    HOME_ROOTS.iter().any(|root| {
        line.match_indices(root).any(|(i, _)| {
            let seg: String = line[i + root.len()..]
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_' || *c == '.')
                .collect();
            !seg.is_empty() && !GENERIC_USERS.contains(&seg.as_str())
        })
    })
}

/// Why a line was flagged. Deliberately carries no matched text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    PrivateName,
    Credential,
    PersonalPath,
}

impl Kind {
    pub fn describe(self) -> &'static str {
        match self {
            Kind::PrivateName => "private name",
            Kind::Credential => "looks like a credential",
            Kind::PersonalPath => "absolute path into someone's home",
        }
    }
}

/// Scan free text (commit messages, PR titles and bodies, comments). Private
/// names are checked only when `names` is given, which takes the key. Returns
/// `(1-based line, kind)` pairs and never the matched text.
pub fn scan_text(text: &str, mut names: Option<&mut Denylist>) -> Vec<(usize, Kind)> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let n = n + 1;
        if names.as_deref_mut().is_some_and(|d| d.names_any(line)) {
            out.push((n, Kind::PrivateName));
        }
        if carries_secret(line) {
            out.push((n, Kind::Credential));
        }
        if carries_personal_path(line) {
            out.push((n, Kind::PersonalPath));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A test key of one repeated byte, written as two hex digits.
    fn key(byte: &str) -> Key {
        Key::from_hex(&byte.repeat(MIN_KEY_BYTES)).expect("a well-formed test key")
    }

    /// Fixture names digested at run time under `key`: nothing real is in this
    /// file.
    fn listing(key: &Key, names: &[&str]) -> Denylist {
        let digests: Vec<String> = names.iter().map(|n| key.digest(n)).collect();
        let digests: Vec<&str> = digests.iter().map(String::as_str).collect();
        Denylist::new(key.clone(), &digests)
    }

    fn texts(line: &str) -> Vec<String> {
        tokens(line).into_iter().map(|t| t.text).collect()
    }

    #[test]
    fn digests_are_hmac_sha256() {
        // RFC 4231 test cases 2, 6 and 7: a short key, a key longer than a
        // block, and a message longer than a block.
        assert_eq!(
            Key::new(b"Jefe").digest("what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            Key::new(&[0xaa; 131]).digest("Test Using Larger Than Block-Size Key - Hash Key First"),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
        assert_eq!(
            Key::new(&[0xaa; 131]).digest(
                "This is a test using a larger than block-size key and a larger than \
                 block-size data. The key needs to be hashed before being used by the HMAC \
                 algorithm."
            ),
            "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2"
        );
    }

    /// HMAC-SHA256 through `sha2`'s ordinary hashing API: the reference the raw
    /// compression path must agree with.
    fn reference_hmac(secret: &[u8], message: &[u8]) -> [u8; 32] {
        let mut block = [0u8; BLOCK];
        if secret.len() > BLOCK {
            block[..32].copy_from_slice(&Sha256::digest(secret));
        } else {
            block[..secret.len()].copy_from_slice(secret);
        }
        let inner = Sha256::new()
            .chain_update(block.map(|b| b ^ 0x36))
            .chain_update(message)
            .finalize();
        Sha256::new()
            .chain_update(block.map(|b| b ^ 0x5c))
            .chain_update(inner)
            .finalize()
            .into()
    }

    #[test]
    fn raw_compression_agrees_with_the_hashing_api() {
        // Every tail length, in one block and two, and keys either side of a block.
        for key_len in [1, 32, 64, 65, 131] {
            let secret: Vec<u8> = (0..key_len).map(|i| i as u8).collect();
            let key = Key::new(&secret);
            for len in 0..=130 {
                let message = "x".repeat(len);
                assert_eq!(
                    key.digest_bytes(&message),
                    reference_hmac(&secret, message.as_bytes()),
                    "key of {key_len} bytes, message of {len}"
                );
            }
        }
    }

    #[test]
    fn a_key_is_at_least_32_bytes_of_hex() {
        assert_eq!(Key::from_hex("not hex").unwrap_err(), GateError::KeyNotHex);
        assert_eq!(
            Key::from_hex(&"a".repeat(65)).unwrap_err(),
            GateError::KeyNotHex
        );
        assert_eq!(
            Key::from_hex(&"ab".repeat(31)).unwrap_err(),
            GateError::KeyTooShort
        );
        // Case does not matter, and the newline a pipe leaves is ignored.
        let padded = format!("{}\n", "AB".repeat(MIN_KEY_BYTES));
        assert_eq!(
            Key::from_hex(&padded).unwrap().digest("token"),
            key("ab").digest("token")
        );
    }

    #[test]
    fn an_absent_key_skips_the_check_unless_required() {
        assert!(key_from(None, false).unwrap().is_none());
        assert!(key_from(Some(OsStr::new(" \n")), false).unwrap().is_none());
        assert_eq!(key_from(None, true).unwrap_err(), GateError::KeyMissing);
        assert_eq!(
            key_from(Some(OsStr::new("")), true).unwrap_err(),
            GateError::KeyMissing
        );
    }

    #[test]
    fn a_key_with_nothing_listed_is_an_error() {
        assert_eq!(
            denylist_from(Some(key("6a")), &[]).err(),
            Some(GateError::NothingListed)
        );
        assert!(denylist_from(None, &[]).unwrap().is_none());
    }

    #[test]
    fn joins_adjacent_words_so_spelling_does_not_matter() {
        for line in ["Some Thing", "SomeThing", "some-thing", "some_thing"] {
            assert!(
                texts(line).contains(&"something".to_string()),
                "{line:?} must reduce to the same token"
            );
        }
    }

    #[test]
    fn joins_at_most_four_words() {
        let found = texts("alpha beta gamma delta omega");
        assert!(found.contains(&"alphabetagammadelta".to_string()));
        assert!(!found.contains(&"alphabetagammadeltaomega".to_string()));
    }

    #[test]
    fn splits_camel_case_and_digits_and_keeps_each_run_whole() {
        let found = texts("HTTPServer WidgetcoClient widgetco2 AlphaBetaGammaDeltaOmega");
        for want in [
            "http",
            "server",
            "widgetco",
            "widgetcoclient",
            "widgetco2",
            "alphabetagammadeltaomega",
        ] {
            assert!(found.contains(&want.to_string()), "{want:?}");
        }
    }

    #[test]
    fn a_token_spans_the_words_it_joins() {
        let line = "see widget-data-platform here";
        let join = tokens(line)
            .into_iter()
            .find(|t| t.text == "widgetdataplatform")
            .expect("the three words joined");
        assert_eq!(&line[join.span], "widget-data-platform");
    }

    #[test]
    fn finds_a_name_inside_a_slug_or_an_image_tag() {
        for line in [
            "owner/widgetco",
            "ghcr.io/owner/widgetco-runner:2.3",
            "faber-cache-widgetco",
        ] {
            assert!(texts(line).contains(&"widgetco".to_string()), "{line:?}");
        }
    }

    #[test]
    fn short_tokens_are_not_hashed() {
        assert!(!texts("a bc def").iter().any(|t| t.len() < MIN_TOKEN));
    }

    #[test]
    fn a_listed_name_is_the_token_the_scan_joins() {
        for name in ["Widget-Co", "widget_co", "WidgetCo", "widget.co"] {
            let token = name_token(name);
            assert_eq!(token, "widgetco", "{name:?}");
            assert!(texts(name).contains(&token), "{name:?}");
        }
        assert_eq!(name_token("Widget2Go"), "widget2go");
        assert!(texts("Widget2Go").contains(&"widget2go".to_string()));
    }

    #[test]
    fn finds_a_three_word_name_written_with_hyphens() {
        let mut names = listing(&key("1f"), &["widgetdataplatform"]);
        let text = "fine\ndeploy widget-data-platform to staging\nfine";
        assert_eq!(
            scan_text(text, Some(&mut names)),
            vec![(2, Kind::PrivateName)]
        );
    }

    #[test]
    fn finds_a_name_embedded_in_camel_case() {
        let mut names = listing(&key("2e"), &["widgetco"]);
        assert_eq!(
            scan_text("let client = WidgetcoClient::new();", Some(&mut names)),
            vec![(1, Kind::PrivateName)]
        );
    }

    #[test]
    fn finds_a_name_run_together_with_digits() {
        let mut names = listing(&key("3d"), &["widgetco"]);
        for line in ["image: widgetco2:latest", "v2widgetco", "widgetco2025"] {
            assert_eq!(
                scan_text(line, Some(&mut names)),
                vec![(1, Kind::PrivateName)],
                "{line:?}"
            );
        }
    }

    #[test]
    fn a_digest_matches_nothing_without_its_key() {
        let listed = key("4c").digest("fixturecorp");
        let text = "fix: bump owner/FixtureCorp-runner image";
        // No key, no name check: the gates announce the skip instead.
        assert!(scan_text(text, None).is_empty());
        // Under any other key the same name digests to something else, so a
        // published digest cannot confirm a guess.
        assert!(!Denylist::new(key("5b"), &[listed.as_str()]).names_any(text));
        assert!(Denylist::new(key("4c"), &[listed.as_str()]).names_any(text));
    }

    #[test]
    fn mask_hides_every_listed_name_and_nothing_else() {
        let mut names = listing(&key("7f"), &["widgetco", "widgetdataplatform"]);
        assert_eq!(
            names.mask("docs/widgetco-notes/WidgetcoClient.md"),
            "docs/***-notes/***Client.md"
        );
        assert_eq!(
            names.mask("ghcr.io/owner/widget-data-platform:2.3"),
            "ghcr.io/owner/***:2.3"
        );
        assert_eq!(names.mask("src/privacy.rs"), "src/privacy.rs");
    }

    #[test]
    fn a_credential_needs_a_body_not_just_a_prefix() {
        assert!(!carries_secret("tokens start with ghp_ and are opaque"));
        // Assembled at run time: spelled out, this line would trip the scan it tests.
        let real = format!("token = {}{}", "ghp_", "0123456789abcdefghijklmnopqrstuv");
        assert!(carries_secret(&real));
        // A long kebab identifier is not an `sk-` credential.
        assert!(!carries_secret(
            "  let task-name-that-runs-rather-long-indeed = 1;"
        ));
    }

    #[test]
    fn role_home_directories_are_not_personal() {
        assert!(!carries_personal_path("/Users/ci/Library/Logs/faber"));
        assert!(!carries_personal_path("/home/runner/work/faber/faber"));
        assert!(!carries_personal_path(
            "dscl . -read /Users/root AuthenticationAuthority"
        ));
        assert!(!carries_personal_path(
            "under /Users/ there is a home per account"
        ));
        assert!(!carries_personal_path("and on Linux under /home/"));
        // Both roots spelled out, not read from HOME_ROOTS: dropping one from
        // the list must fail here.
        for root in ["/Users/", "/home/"] {
            let personal = format!("{root}{}/Development/faber", "adalovelace");
            assert!(carries_personal_path(&personal), "{root}");
        }
        // Anywhere in a line, not only where a path starts: a home root inside
        // a longer path still names someone's directory.
        let inside = format!("/var/home/{}/notes", "adalovelace");
        assert!(carries_personal_path(&inside));
    }

    #[test]
    fn text_scan_finds_a_name_and_reports_only_the_line() {
        let mut names = listing(&key("8e"), &["fixturecorp"]);
        let text = "fine line\nfix: bump owner/FixtureCorp-runner image\nalso fine";
        let hits = scan_text(text, Some(&mut names));
        assert_eq!(hits, vec![(2, Kind::PrivateName)]);
        assert!(!format!("{hits:?}").to_lowercase().contains("fixturecorp"));
    }

    #[test]
    fn text_scan_is_clean_on_clean_text_and_flags_secrets_and_paths() {
        let mut names = listing(&key("9d"), &["fixturecorp"]);
        assert!(scan_text("feat: add a thing\n\nBody text.", Some(&mut names)).is_empty());
        let secret = format!("token {}{}", "ghp_", "0123456789abcdefghijklmnopqrstuv");
        let path = format!("see /Users/{}/x", "adalovelace");
        let text = format!("{secret}\n{path}");
        let want = vec![(1, Kind::Credential), (2, Kind::PersonalPath)];
        assert_eq!(scan_text(&text, Some(&mut names)), want);
        // Without the key, credentials and paths are still checked.
        assert_eq!(scan_text(&text, None), want);
    }

    #[test]
    fn shipped_denylist_is_well_formed() {
        assert!(DENIED
            .iter()
            .all(|d| d.len() == 64 && d.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))));
    }
}
