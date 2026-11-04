//! `SHA256SUMS` parsing and digest checks.

use std::collections::HashMap;

/// Parse a `SHA256SUMS` file — the `shasum -a 256` text-mode format, a
/// lowercase hex digest then two spaces then the file name — into a
/// `name -> digest` map. Also tolerates the single-space-plus-`*`
/// binary-mode separator some `sha256sum` builds emit, since splitting on
/// the first run of whitespace handles both without needing to know which
/// produced the file. A line that isn't a valid 64-character hex digest
/// followed by a name is skipped rather than failing the whole parse, so one
/// stray blank line can't hide every other asset's checksum.
pub fn parse_sha256sums(text: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let (Some(digest), Some(name)) = (parts.next(), parts.next()) else {
            continue;
        };
        let name = name.trim().trim_start_matches('*');
        if digest.len() == 64 && digest.chars().all(|c| c.is_ascii_hexdigit()) {
            out.insert(name.to_string(), digest.to_ascii_lowercase());
        }
    }
    out
}

/// Hex-encode the SHA-256 digest of `bytes`, lowercase (matching `shasum -a
/// 256`'s own output, and what [`parse_sha256sums`] normalizes to).
pub fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

/// Check `bytes` against the digest `SHA256SUMS` published for `name`.
/// Case-insensitive: nothing guarantees hex casing in a hand-edited or
/// third-party-generated sums file, even though `parse_sha256sums` itself
/// always normalizes to lowercase.
pub fn verify_digest(
    sums: &HashMap<String, String>,
    name: &str,
    bytes: &[u8],
) -> Result<(), String> {
    let expected = sums
        .get(name)
        .ok_or_else(|| format!("SHA256SUMS has no entry for {name}"))?;
    let actual = sha256_hex(bytes);
    if expected.eq_ignore_ascii_case(&actual) {
        Ok(())
    } else {
        Err(format!(
            "{name}: sha256 mismatch (expected {expected}, got {actual})"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sha256sums_standard_format() {
        let text = format!(
            "{}  faber-aarch64-apple-darwin\n{}  faber-app-aarch64-apple-darwin.tar.gz\n",
            "a".repeat(64),
            "b".repeat(64)
        );
        let sums = parse_sha256sums(&text);
        assert_eq!(
            sums.get("faber-aarch64-apple-darwin").map(String::as_str),
            Some("a".repeat(64)).as_deref()
        );
        assert_eq!(
            sums.get("faber-app-aarch64-apple-darwin.tar.gz")
                .map(String::as_str),
            Some("b".repeat(64)).as_deref()
        );
    }

    #[test]
    fn parses_sha256sums_binary_mode_separator_and_skips_blank_lines() {
        let text = format!("\n{} *faber-aarch64-apple-darwin\n  \n", "c".repeat(64));
        let sums = parse_sha256sums(&text);
        assert_eq!(
            sums.get("faber-aarch64-apple-darwin").map(String::as_str),
            Some("c".repeat(64)).as_deref()
        );
    }

    #[test]
    fn accepts_a_matching_digest_case_insensitively() {
        let digest = sha256_hex(b"hello world");
        let mut sums = HashMap::new();
        sums.insert("f".to_string(), digest.to_ascii_uppercase());
        assert!(verify_digest(&sums, "f", b"hello world").is_ok());
    }

    #[test]
    fn rejects_a_digest_mismatch() {
        let mut sums = HashMap::new();
        sums.insert("faber-aarch64-apple-darwin".to_string(), "0".repeat(64));
        let err = verify_digest(&sums, "faber-aarch64-apple-darwin", b"actual content")
            .expect_err("a wrong expectation must not verify");
        assert!(err.contains("mismatch"), "error should explain why: {err}");
    }

    #[test]
    fn rejects_a_missing_sums_entry() {
        let sums = HashMap::new();
        let err = verify_digest(&sums, "faber-aarch64-apple-darwin", b"content")
            .expect_err("an asset with no published digest must not verify");
        assert!(err.contains("no entry"), "error should explain why: {err}");
    }
}
