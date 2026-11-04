//! The version marker every faber binary carries, so setup can learn the
//! daemon copy's version without running it.
//!
//! Setup must never execute the daemon copy (ADR-0025's root invariant). It is
//! `_faber`'s file, so whatever it does, it would do inside the operator's sudo
//! session: with the operator's terminal as its controlling tty it could type
//! into their shell (`TIOCSTI`) or answer setup's own prompt. So instead of
//! asking the binary, setup reads its bytes and looks for [`MARKER`]:
//! `FABER-VERSION:<version>` and a NUL.
//!
//! What it finds is an untrusted hint. `_faber` can write any bytes it likes
//! into its own copy, so the version decides exactly one thing, whether a copy
//! newer than this binary is kept rather than replaced, and `--replace-daemon`
//! overrides even that. It is validated as a version and reduced to [`version_chars`]
//! before it is ever printed.

/// `FABER-VERSION:<FABER_VERSION>\0`. Kept in the binary by [`keep`].
#[used]
pub static MARKER: &str = concat!("FABER-VERSION:", env!("FABER_VERSION"), "\0");

/// The longest version the scanner accepts.
pub const MAX_VERSION: usize = 64;

/// Make sure [`MARKER`] is in the linked binary. Called from `setup::run`: live
/// code that reads the marker's address can be stripped by neither the
/// compiler nor the linker, whatever LTO does with an otherwise unused static.
pub fn keep() {
    std::hint::black_box(MARKER.as_ptr());
}

/// The prefix the scanner looks for, stored reversed and turned around at run
/// time: the forward prefix must exist in a built binary only inside
/// [`MARKER`], wherever the linker happens to lay out other constants.
pub(crate) fn needle() -> Vec<u8> {
    b":NOISREV-REBAF".iter().rev().copied().collect()
}

/// `s` with everything but `[0-9A-Za-z.+-]` dropped, at most
/// [`MAX_VERSION`] characters: safe to print whoever wrote it.
pub fn version_chars(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '-'))
        .take(MAX_VERSION)
        .collect()
}

/// A version as the marker holds it: only `[0-9A-Za-z.+-]`, at most
/// [`MAX_VERSION`] characters, and a version faber can compare.
fn valid(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() || bytes.len() > MAX_VERSION {
        return None;
    }
    let s = std::str::from_utf8(bytes).ok()?;
    if version_chars(s) != s {
        return None;
    }
    crate::update::parse_version(s).map(|_| s.to_string())
}

/// Finds [`MARKER`] in a stream of bytes fed to it in chunks, however the
/// chunks split it.
pub struct Scanner {
    needle: Vec<u8>,
    window: Vec<u8>,
    found: Option<String>,
}

impl Default for Scanner {
    fn default() -> Self {
        Self::new()
    }
}

impl Scanner {
    pub fn new() -> Self {
        Self {
            needle: needle(),
            window: Vec::new(),
            found: None,
        }
    }

    pub fn feed(&mut self, chunk: &[u8]) {
        if self.found.is_some() {
            return;
        }
        self.window.extend_from_slice(chunk);
        let n = self.needle.len();
        let mut from = 0;
        while let Some(i) = find(&self.window[from..], &self.needle) {
            let start = from + i + n;
            let rest = &self.window[start..];
            match rest.iter().position(|b| *b == 0) {
                Some(end) => {
                    if let Some(v) = valid(&rest[..end]) {
                        self.found = Some(v);
                        self.window.clear();
                        return;
                    }
                    from = start;
                }
                // A marker cut off at the end of this chunk: keep it whole in
                // the window for the next one.
                None if rest.len() <= MAX_VERSION => break,
                None => from = start,
            }
        }
        // Keep enough of the tail to hold a marker split across chunks.
        let keep = n + MAX_VERSION + 1;
        if self.window.len() > keep {
            self.window.drain(..self.window.len() - keep);
        }
    }

    /// The first valid version found, if any.
    pub fn finish(self) -> Option<String> {
        self.found
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_in_chunks(bytes: &[u8], chunk: usize) -> Option<String> {
        let mut s = Scanner::new();
        for c in bytes.chunks(chunk.max(1)) {
            s.feed(c);
        }
        s.finish()
    }

    /// Test inputs are built from [`needle`] at run time: a contiguous marker
    /// prefix in this test binary's own constants, padded by a NUL, could be
    /// found before the real [`MARKER`] by the self-scan below.
    fn with_marker(version: &str) -> Vec<u8> {
        let mut bytes = vec![0x7f, b'E', b'L', b'F', 1, 2, 3];
        bytes.extend(needle());
        bytes.extend_from_slice(version.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(&[9; 200]);
        bytes
    }

    #[test]
    fn the_marker_is_found_however_the_chunks_split_it() {
        let bytes = with_marker("0.4.0+gabc1234");
        for chunk in 1..=bytes.len() {
            assert_eq!(
                scan_in_chunks(&bytes, chunk).as_deref(),
                Some("0.4.0+gabc1234"),
                "chunk size {chunk}"
            );
        }
    }

    /// Anything that is not a clean, comparable version is no marker at all.
    #[test]
    fn a_malformed_marker_is_ignored() {
        for bad in [
            "",
            "not-a-version",
            "0.4",
            "0.4.0\u{1b}[2J",
            "0.4.0 rm",
            &"9".repeat(MAX_VERSION + 1),
        ] {
            assert_eq!(scan_in_chunks(&with_marker(bad), 7), None, "{bad:?}");
        }
        // No NUL within reach: not a marker.
        let mut unterminated = needle();
        unterminated.extend_from_slice(b"0.4.0");
        unterminated.extend_from_slice(&[b'1'; 100]);
        assert_eq!(scan_in_chunks(&unterminated, 16), None);
    }

    /// A bogus match before the real one does not hide it.
    #[test]
    fn the_first_valid_marker_wins() {
        let mut bytes = with_marker("junk!");
        bytes.extend(with_marker("0.5.1"));
        bytes.extend(with_marker("0.6.0"));
        assert_eq!(scan_in_chunks(&bytes, 5).as_deref(), Some("0.5.1"));
    }

    #[test]
    fn version_chars_keeps_only_version_characters() {
        assert_eq!(version_chars("0.4.0+gabc"), "0.4.0+gabc");
        assert_eq!(version_chars("0.4.0\u{1b}[31m;rm -rf /"), "0.4.031mrm-rf");
        assert_eq!(version_chars(&"1".repeat(100)).len(), MAX_VERSION);
    }

    /// This very binary carries its own version, found the way setup finds
    /// the daemon copy's: by reading bytes, never by running anything.
    #[test]
    fn a_built_faber_carries_its_marker() {
        keep();
        assert!(MARKER.ends_with('\0'));
        let exe = std::env::current_exe().unwrap();
        let mut file = std::fs::File::open(exe).unwrap();
        let mut s = Scanner::new();
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let n = std::io::Read::read(&mut file, &mut buf).unwrap();
            if n == 0 {
                break;
            }
            s.feed(&buf[..n]);
        }
        assert_eq!(s.finish().as_deref(), Some(env!("FABER_VERSION")));
    }
}
