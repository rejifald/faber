//! The daemon's log output, held to one line per event (#82).
//!
//! faber logs text GitHub controls — job, workflow and repo names, which anyone who
//! can push a workflow chooses — and `tracing`'s fmt layer escapes ESC, BEL, BS, FF,
//! DEL and the C1 range in it, but not a carriage return. A name carrying `\r`
//! makes a terminal that `cat`s or `tail`s `stderr.log` return to the start of the
//! line and print over it, so a line can hide or spoof what it says.
//!
//! Fixed at the writer rather than at each call site, so a log line added later
//! cannot miss it: [`Stderr`] is what the subscriber writes through, and every
//! formatted event passes [`escape`] on its way out. That escapes every C0 control
//! character the fmt layer lets through except the two a log line is made of: tab,
//! and the newline that ends it.
//!
//! ESC is the one C0 character it leaves alone, because by the time an event reaches
//! the writer every ESC in it is the fmt layer's own: it writes the ANSI colours of
//! the timestamp and level with it, and it has already escaped any ESC that came in
//! a message or a field. Escaping it again would turn each of those colour codes
//! into literal `\x1b[2m` text on every line the daemon writes.
//!
//! The log is also held to one line per *change*. A warning about a standing
//! condition — a VM larger than the machine share, a variant build the disk floor
//! refuses, a job whose declared needs its image cannot serve — is found again on
//! every probe or pass, and logging each finding buried everything else: in
//! October 2026 four such warnings were about two fifths of `stderr.log`'s lines.
//! [`Repeats`] lets a call site log a condition when it is new or its text changes,
//! and again once an hour while it stands.

use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use tracing_subscriber::fmt::MakeWriter;

/// `bytes` with every C0 control character but `\t`, `\n` and ESC written as
/// `\xNN`. ESC is the fmt layer's, which escapes it in a value itself (see the
/// module doc).
///
/// Byte-wise, which is sound for UTF-8: no byte below `0x20` occurs inside a
/// multi-byte sequence, so nothing else is touched.
pub fn escape(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    for &b in bytes {
        if b < 0x20 && !matches!(b, b'\t' | b'\n' | 0x1b) {
            out.extend_from_slice(format!("\\x{b:02x}").as_bytes());
        } else {
            out.push(b);
        }
    }
    out
}

/// A writer that passes everything through [`escape`] into `inner`.
pub struct Escaping<W>(pub W);

impl<W: Write> Write for Escaping<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // All or nothing: a short write of the escaped bytes could not be mapped
        // back to a count of `buf`'s.
        self.0.write_all(&escape(buf))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

/// The daemon's log writer: stderr, escaped. Install with
/// `tracing_subscriber::fmt().with_writer(Stderr)`.
pub struct Stderr;

impl<'a> MakeWriter<'a> for Stderr {
    type Writer = Escaping<io::Stderr>;

    fn make_writer(&'a self) -> Self::Writer {
        Escaping(io::stderr())
    }
}

/// How often a standing condition [`Repeats`] holds back is logged again: often
/// enough that a `tail` of the last hour still shows it, rarely enough that it
/// cannot crowd anything out.
pub const REPEAT_REMIND: Duration = Duration::from_secs(3_600);

/// How many conditions [`Repeats`] remembers. Keys carry job ids, so the set
/// would otherwise grow for the process life; over the cap, conditions not
/// logged within [`REPEAT_REMIND`] are forgotten first, and everything if that
/// is not enough — forgetting costs one extra line, never a missing one.
const REPEATS_CAP: usize = 4_096;

/// What each standing condition last logged, and when, by key.
#[derive(Default)]
pub struct Repeats {
    seen: Mutex<HashMap<String, (String, Instant)>>,
}

impl Repeats {
    /// Whether the condition `key` should be logged now, saying `msg`: it is new,
    /// its text changed, or [`REPEAT_REMIND`] has passed since it was last
    /// logged. Records the answer, so the caller logs exactly when this is true.
    pub fn fresh(&self, key: &str, msg: &str, now: Instant) -> bool {
        let mut seen = self.seen.lock().unwrap();
        if let Some((last, at)) = seen.get(key) {
            if last == msg && now.saturating_duration_since(*at) < REPEAT_REMIND {
                return false;
            }
        }
        if seen.len() >= REPEATS_CAP && !seen.contains_key(key) {
            seen.retain(|_, (_, at)| now.saturating_duration_since(*at) < REPEAT_REMIND);
            if seen.len() >= REPEATS_CAP {
                seen.clear();
            }
        }
        seen.insert(key.to_string(), (msg.to_string(), now));
        true
    }

    /// Forget `key`, the condition having cleared. Returns whether it had been
    /// logged, so the caller can say it cleared only when it had said it stood.
    pub fn cleared(&self, key: &str) -> bool {
        self.seen.lock().unwrap().remove(key).is_some()
    }
}

/// `msg` without its ASCII digits: the text a repeat check compares when the
/// message's figures (seconds, MiB) move on every pass but its wording does not.
pub fn wording(msg: &str) -> String {
    msg.chars().filter(|c| !c.is_ascii_digit()).collect()
}

/// The daemon's [`Repeats`].
pub fn repeats() -> &'static Repeats {
    static REPEATS: LazyLock<Repeats> = LazyLock::new(Repeats::default);
    &REPEATS
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn a_standing_condition_is_logged_once_per_change_and_hourly() {
        let r = Repeats::default();
        let t0 = Instant::now();
        assert!(r.fresh("vm", "too big", t0), "new");
        assert!(!r.fresh("vm", "too big", t0 + Duration::from_secs(60)));
        assert!(!r.fresh("vm", "too big", t0 + Duration::from_secs(3_599)));
        assert!(
            r.fresh("vm", "too big", t0 + REPEAT_REMIND),
            "hourly reminder"
        );
        assert!(
            r.fresh("vm", "bigger", t0 + REPEAT_REMIND + Duration::from_secs(1)),
            "a changed text is logged at once"
        );
        assert!(r.fresh("job 2", "too big", t0), "keys are independent");
    }

    #[test]
    fn causes_differing_only_in_figures_log_once() {
        let r = Repeats::default();
        let t0 = Instant::now();
        let a = "failed 12s ago, not tried again for 288s; 900 MiB free";
        let b = "failed 40s ago, not tried again for 260s; 850 MiB free";
        assert!(r.fresh("v", &wording(a), t0));
        assert!(!r.fresh("v", &wording(b), t0 + Duration::from_secs(30)));
        assert!(r.fresh("v", &wording("refused: no disk"), t0));
    }

    #[test]
    fn a_cleared_condition_is_reported_once_and_logged_afresh() {
        let r = Repeats::default();
        let t0 = Instant::now();
        assert!(!r.cleared("vm"), "never logged, so nothing to clear");
        assert!(r.fresh("vm", "too big", t0));
        assert!(r.cleared("vm"));
        assert!(!r.cleared("vm"));
        assert!(
            r.fresh("vm", "too big", t0),
            "it stands again: logged again"
        );
    }

    #[test]
    fn over_the_cap_stale_conditions_go_first_and_a_live_one_stays_quiet() {
        let r = Repeats::default();
        let t0 = Instant::now();
        let later = t0 + REPEAT_REMIND;
        assert!(r.fresh("live", "x", later));
        for i in 0..REPEATS_CAP - 1 {
            assert!(r.fresh(&format!("job {i}"), "x", t0));
        }
        assert!(r.fresh("new", "x", later), "over the cap, the stale go");
        assert_eq!(r.seen.lock().unwrap().len(), 2);
        assert!(
            !r.fresh("live", "x", later),
            "a recent one is still held back"
        );
    }

    #[test]
    fn a_carriage_return_is_escaped_and_the_line_keeps_its_end() {
        assert_eq!(
            escape(b"job ok\rjob FORGED\n"),
            b"job ok\\x0djob FORGED\n".to_vec()
        );
        assert_eq!(escape(b"a\tb\n"), b"a\tb\n".to_vec());
        assert_eq!(escape(b"\x0b\x00"), b"\\x0b\\x00".to_vec());
        assert_eq!(escape(b"\x1b[2m"), b"\x1b[2m".to_vec());
        assert_eq!(
            escape("caf\u{e9} \u{2014}\n".as_bytes()),
            "caf\u{e9} \u{2014}\n".as_bytes()
        );
    }

    /// A buffer the subscriber can write into, so the test reads what the daemon
    /// would have put on stderr.
    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// End to end through the same subscriber shape `main` installs: the fmt layer
    /// passes `\r` through untouched, and the writer is what stops it.
    #[test]
    fn a_github_supplied_name_cannot_overwrite_its_own_log_line() {
        let buf = Buf::default();
        let sink = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_target(false)
            .with_ansi(false)
            .with_writer(move || Escaping(sink.clone()))
            .compact()
            .finish();
        let job = "e2e\rowner/demo job 1: succeeded";
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!("owner/demo job 1: {job} was killed");
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(!out.contains('\r'), "{out:?}");
        assert!(out.contains("e2e\\x0downer/demo"), "{out:?}");
        assert!(
            out.ends_with('\n') && out.matches('\n').count() == 1,
            "{out:?}"
        );
    }

    /// `main` leaves ANSI on, so the fmt layer colours the timestamp and level with
    /// ESC sequences. Those reach the terminal as colours, not as `\x1b[` text, and
    /// an ESC that came in a GitHub-supplied name is still escaped, by the fmt layer.
    #[test]
    fn the_formatters_colours_survive_and_a_supplied_escape_does_not() {
        let buf = Buf::default();
        let sink = buf.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_target(false)
            .with_ansi(true)
            .with_writer(move || Escaping(sink.clone()))
            .compact()
            .finish();
        let job = "e2e\x1b[2Kowner/demo\r";
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!("job {job} was killed");
        });
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert!(out.contains("\x1b["), "the colours are real ESCs: {out:?}");
        assert!(
            !out.contains("\\x1b[2m"),
            "no colour code is escaped: {out:?}"
        );
        assert!(out.contains("e2e\\x1b[2Kowner/demo\\x0d"), "{out:?}");
    }
}
