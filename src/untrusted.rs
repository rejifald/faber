//! What faber accepts from the Docker daemon, and how much of it (#105).
//!
//! On a host that grants dind (ADR-0013) a job is root in the VM that runs Docker,
//! so it can replace or steer the daemon faber talks to. Until faber's Docker calls
//! move out of the account that holds the GitHub credential (#107), everything the
//! daemon says reaches that account. So faber reads it the way it would read input
//! from a party that may be lying:
//!
//! - **Bounded.** No `docker` call's stdout or stderr is read past a cap:
//!   [`OUTPUT_CAP`], or a larger cap the call names for itself. A call that writes
//!   more is an error, and its process is killed rather than read to the end.
//! - **Typed.** A listing with more than one field is asked for as one JSON object
//!   per line, every value escaped by Docker's own `json` template function, and
//!   parsed with serde ([`json_lines`]). A value can no longer forge a field or a
//!   whole row by carrying a tab or a newline, which a tab-separated listing could
//!   not rule out. A single value is parsed into the type faber uses.
//! - **Validated.** Every value faber acts on is checked before it is used, against
//!   the format faber itself writes or the one Docker itself allows: ids are hex of
//!   the right length, names are in Docker's own alphabet, faber's labels look like
//!   faber wrote them, numbers are in range. An item that fails is skipped with a
//!   warning ([`accept`]), and every caller reads a skipped item in the direction
//!   that deletes less and admits less, never more.
//!
//! What that buys, and what it does not. A lying daemon can still disrupt CI on this
//! host: it runs the containers. It cannot make faber run a command of its choosing,
//! because no string read from it is ever part of a shell script. The scripts faber
//! hands to `sh -c` are constants, and data reaches them only as positional
//! arguments. And it cannot steer a GitHub write onto a runner or job faber did not
//! mint: see [`crate::docker::MintedRunner`].
//!
//! A repo is the other party whose values reach a build, and the one validator for
//! that lives here too: [`release_version`] (#113).

use std::process::{Output, Stdio};
use std::time::Duration;

use serde::de::DeserializeOwned;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tracing::warn;

/// How much of one stream faber reads from one `docker` call. Far more than any
/// listing, inspect or probe it makes has needed, and far less than a daemon that
/// means harm could otherwise make it hold.
pub(crate) const OUTPUT_CAP: usize = 8 * 1024 * 1024;

/// The largest memory figure the daemon may report, in MiB: 16 TiB, orders of
/// magnitude past any VM faber runs on.
pub(crate) const MAX_MEM_MIB: u64 = 16 << 20;

/// [`MAX_MEM_MIB`] in bytes.
pub(crate) const MAX_MEM_BYTES: u64 = MAX_MEM_MIB << 20;

/// The largest byte count one block or network I/O counter may report: 8 PiB,
/// far past anything a run moves. A bigger figure is held to it, like
/// [`MAX_MEM_MIB`].
pub(crate) const MAX_IO_BYTES: u64 = 8 << 50;

/// The largest CPU count the daemon may report.
pub(crate) const MAX_CPUS: u64 = 4096;

/// Run `cmd` to completion, reading at most `cap` bytes from each of stdout and
/// stderr, and give up after `limit`.
///
/// `kill_on_drop(true)` (set by [`output_capped`]) is load-bearing rather than
/// tidiness: dropping a timed-out `tokio::process` future does **not** reap the
/// child, so bounding the wait without it would only turn a hung task into a leaked
/// process, the same exhaustion minus the evidence.
pub(crate) async fn output_within(
    cmd: Command,
    limit: Duration,
    cap: usize,
    what: &str,
) -> anyhow::Result<Output> {
    match tokio::time::timeout(limit, output_capped(cmd, cap, what)).await {
        Ok(out) => out,
        Err(_) => anyhow::bail!("{what} exceeded {limit:?} — daemon unresponsive"),
    }
}

/// Run `cmd` to completion, reading at most `cap` bytes from each of stdout and
/// stderr, with no time limit. For the one call that is unbounded in time by design
/// (`docker wait`), which must still not be unbounded in size.
///
/// A stream that runs past `cap` is an error, and the process is killed then rather
/// than left writing into a pipe nobody reads. stdin is closed: nothing faber runs
/// this way reads one ([`output_within_fed`] is the call that feeds it).
pub(crate) async fn output_capped(cmd: Command, cap: usize, what: &str) -> anyhow::Result<Output> {
    run_capped(cmd, None, cap, what).await
}

/// [`output_within`], with `input` written to the child's stdin and the pipe
/// then closed, so the child reads to end of file. Two callers:
///
/// - a variant build, which reads its recipe from stdin (`docker build --file -`,
///   ADR-0028);
/// - a credential fill, the one way faber hands a container a secret (ADR-0026: a
///   credential reaches a container through a named volume a short-lived container
///   fills): stdin is visible to no other account, where argv is to every one
///   through `ps` and a host file is to whoever can read its directory.
///
/// A child that exits 0 without having taken all of `input` is an error: a build
/// that succeeded on part of a recipe proves nothing about the rest, and a fill
/// that stopped early wrote part of a credential. A child that fails before
/// reading it reports its own failure, not the broken pipe.
pub(crate) async fn output_within_fed(
    cmd: Command,
    input: Vec<u8>,
    limit: Duration,
    cap: usize,
    what: &str,
) -> anyhow::Result<Output> {
    match tokio::time::timeout(limit, run_capped(cmd, Some(input), cap, what)).await {
        Ok(out) => out,
        Err(_) => anyhow::bail!("{what} exceeded {limit:?} — daemon unresponsive"),
    }
}

async fn run_capped(
    mut cmd: Command,
    input: Option<Vec<u8>>,
    cap: usize,
    what: &str,
) -> anyhow::Result<Output> {
    use tokio::io::AsyncWriteExt;
    cmd.stdin(if input.is_some() {
        Stdio::piped()
    } else {
        Stdio::null()
    })
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .kill_on_drop(true);
    let mut child = cmd.spawn()?;
    let (Some(stdout), Some(stderr)) = (child.stdout.take(), child.stderr.take()) else {
        anyhow::bail!("{what}: its output was not captured");
    };
    let stdin = child.stdin.take();
    let feed = async move {
        let (Some(bytes), Some(mut pipe)) = (input, stdin) else {
            return Ok(());
        };
        let wrote = pipe.write_all(&bytes).await;
        // Dropped, and so closed, here: the child reads to end of file.
        drop(pipe);
        wrote
    };
    let (fed, read) = tokio::join!(feed, async {
        tokio::try_join!(
            read_capped(stdout, cap, what, "stdout"),
            read_capped(stderr, cap, what, "stderr"),
        )
    });
    match read {
        Ok((stdout, stderr)) => {
            let status = child.wait().await?;
            if let Err(e) = fed {
                if status.success() {
                    anyhow::bail!("{what} exited 0 without reading all of its input: {e}");
                }
            }
            Ok(Output {
                status,
                stdout,
                stderr,
            })
        }
        Err(e) => {
            // Nothing more it writes will be read, so stop it writing.
            let _ = child.start_kill();
            let _ = child.wait().await;
            Err(e)
        }
    }
}

/// Read one stream to its end, or fail as soon as it passes `cap`.
async fn read_capped<R: AsyncRead + Unpin>(
    stream: R,
    cap: usize,
    what: &str,
    name: &str,
) -> anyhow::Result<Vec<u8>> {
    let mut buf = Vec::new();
    stream.take(cap as u64 + 1).read_to_end(&mut buf).await?;
    if buf.len() > cap {
        anyhow::bail!("{what} wrote more than {cap} bytes to {name} — refusing to read further");
    }
    Ok(buf)
}

/// Parse a listing printed as one JSON object per line into `T`s.
///
/// A line that does not parse is skipped, not fatal: a listing is many items, and
/// one bad one is no reason to act on none. The skips are counted into one warning
/// for the call. It names the first parse error, never the line, whose content is
/// the daemon's. Blank lines are not items.
pub(crate) fn json_lines<T: DeserializeOwned>(text: &str, what: &str) -> Vec<T> {
    let mut out = Vec::new();
    let mut skipped = 0usize;
    let mut first: Option<String> = None;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        match serde_json::from_str::<T>(line) {
            Ok(item) => out.push(item),
            Err(e) => {
                skipped += 1;
                first.get_or_insert_with(|| for_log(&e.to_string(), 160));
            }
        }
    }
    if skipped > 0 {
        warn!(
            "{what}: skipped {skipped} line(s) that are not the JSON faber asked for ({})",
            first.unwrap_or_default()
        );
    }
    out
}

/// [`json_lines`], all or nothing: `None`, with a warning, when any line does not
/// parse.
///
/// For a listing whose rows protect something: a shield against a reap, or a
/// ledger that must charge every container. There a row faber cannot read must
/// mean "cannot tell", never "absent", or a skipped row would admit or delete
/// more.
pub(crate) fn json_lines_all<T: DeserializeOwned>(text: &str, what: &str) -> Option<Vec<T>> {
    let mut out = Vec::new();
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        match serde_json::from_str::<T>(line) {
            Ok(item) => out.push(item),
            Err(e) => {
                warn!(
                    "{what}: a line is not the JSON faber asked for ({}) — reading the whole \
                     listing as unreadable",
                    for_log(&e.to_string(), 160)
                );
                return None;
            }
        }
    }
    Some(out)
}

/// [`accept`], all or nothing: `None`, with a warning, when `check` refuses any
/// item, for the listings [`json_lines_all`] is for. An item that is simply not
/// this caller's (`Ok(None)`) is still left out without a word.
pub(crate) fn accept_all<T, U>(
    items: impl IntoIterator<Item = T>,
    what: &str,
    mut check: impl FnMut(T) -> Result<Option<U>, &'static str>,
) -> Option<Vec<U>> {
    let mut out = Vec::new();
    for item in items {
        match check(item) {
            Ok(Some(kept)) => out.push(kept),
            Ok(None) => {}
            Err(why) => {
                warn!(
                    "{what}: an item is not in the form faber reads ({why}) — reading the whole \
                     listing as unreadable"
                );
                return None;
            }
        }
    }
    Some(out)
}

/// Keep the items `check` accepts, as what it maps them to.
///
/// `check` answers `Ok(Some(_))` for an item to keep, `Ok(None)` for one that is
/// simply not this caller's (a container of another kind, say), and `Err(why)` for
/// one that is not in the form faber reads. Refused items are counted into one
/// warning for the whole listing, with the first reason. The reason is a fixed
/// description, never the item, whose content is the daemon's.
pub(crate) fn accept<T, U>(
    items: impl IntoIterator<Item = T>,
    what: &str,
    mut check: impl FnMut(T) -> Result<Option<U>, &'static str>,
) -> Vec<U> {
    let mut out = Vec::new();
    let mut refused = 0usize;
    let mut first: Option<&'static str> = None;
    for item in items {
        match check(item) {
            Ok(Some(kept)) => out.push(kept),
            Ok(None) => {}
            Err(why) => {
                refused += 1;
                first.get_or_insert(why);
            }
        }
    }
    if refused > 0 {
        warn!(
            "{what}: skipped {refused} item(s) not in the form faber reads ({})",
            first.unwrap_or_default()
        );
    }
    out
}

// --- the values ----------------------------------------------------------------

/// Exactly `len` lowercase hex digits, the form Docker prints every id in.
pub(crate) fn hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A full Docker object id, as `--no-trunc` prints one: 64 hex digits.
pub(crate) fn object_id(s: &str) -> bool {
    hex(s, 64)
}

/// An image content id: `sha256:` and 64 hex digits.
pub(crate) fn image_id(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|h| hex(h, 64))
}

/// A name in the alphabet Docker itself allows for a container or a volume:
/// `[a-zA-Z0-9][a-zA-Z0-9_.-]*`, at most 255 bytes.
///
/// That alphabet is what keeps a name off every other meaning a command line could
/// give it: it cannot start with `-` (an option), contain `/` (a path) or `:` (a
/// mount spec), or carry whitespace, quotes or anything a shell would act on.
pub(crate) fn object_name(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 255
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
}

/// An image reference as Docker prints one: `[registry/]repo[:tag][@digest]`.
///
/// Not the whole reference grammar, only its alphabet. It must start alphanumeric
/// and use nothing outside `[A-Za-z0-9._/:@-]`, which already excludes whitespace,
/// quotes, shell metacharacters and a leading `-`.
pub(crate) fn image_ref(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 512
        && b[0].is_ascii_alphanumeric()
        && b.iter().all(|&c| {
            c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'/' | b':' | b'@' | b'-')
        })
}

/// `owner/repo` in GitHub's own alphabet, which is the only shape a repo faber
/// serves can have.
///
/// faber builds API paths from repo names, so this is also what keeps a name that
/// came off a label from steering one: no `..`, no second `/`, no `?` or `#`.
pub(crate) fn repo_slug(s: &str) -> bool {
    let Some((owner, repo)) = s.split_once('/') else {
        return false;
    };
    // `_` in the owner only, and never first: an Enterprise Managed Users login is
    // `{handle}_{shortcode}`, the one owner GitHub spells with one.
    let owner_ok = !owner.is_empty()
        && owner.len() <= 39
        && owner.as_bytes()[0].is_ascii_alphanumeric()
        && owner
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_'));
    let repo_ok = !repo.is_empty()
        && repo.len() <= 100
        && repo != "."
        && repo != ".."
        && repo
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'));
    owner_ok && repo_ok
}

/// A decimal number and nothing looser: digits only, no sign, no space, no empty
/// string, and small enough to be a `u64`.
pub(crate) fn number(s: &str) -> Option<u64> {
    if s.is_empty() || s.len() > 20 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// `{job_id}-{mint_ts}-{instance}`: the claim id faber stamps as `org.faber.id`
/// and builds every per-job name from. Two runs of digits, then the minting
/// instance's id (#94, [`crate::instance::valid`]), `-`-separated and nothing
/// else. A claim from before instance ids, `{job_id}-{mint_ts}`, still reads.
pub(crate) fn claim_id(s: &str) -> Option<(u64, u64)> {
    let (job, rest) = s.split_once('-')?;
    let ts = match rest.split_once('-') {
        Some((ts, inst)) if crate::instance::valid(inst) => ts,
        Some(_) => return None,
        None => rest,
    };
    Some((number(job)?, number(ts)?))
}

/// `faber-{job_id}-{mint_ts}-{instance}` (or, before #94, without the instance):
/// the name faber registers a runner under.
pub(crate) fn runner_name(s: &str) -> Option<(u64, u64)> {
    claim_id(s.strip_prefix("faber-")?)
}

/// Free text a label may carry, such as a job name inside a class key or a profile
/// name: printable, with no control characters (so no newline, tab or escape), and
/// at most `max` bytes.
pub(crate) fn label_text(s: &str, max: usize) -> bool {
    s.len() <= max && !s.chars().any(char::is_control)
}

/// A toolchain version as faber records one (`1.61.1`, `1.62.0-alpha.1+exp`):
/// starts alphanumeric, uses only `[A-Za-z0-9.+_-]`, at most 64 bytes.
pub(crate) fn version(s: &str) -> bool {
    let b = s.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'+' | b'_' | b'-'))
}

/// A release version a **repo** names, held to strict semver before it may reach a
/// build (#113): `MAJOR.MINOR.PATCH` in digits, optionally `-` and a pre-release
/// in `[0-9A-Za-z.-]`, at most 64 bytes. No build metadata, no `v`, no range.
///
/// Stricter than [`version`] on purpose. A repo's lockfile is read at a ref any
/// branch can edit, and the value lands after `playwright@` in an `npx` call that
/// runs while a variant builds. Quoting stops a shell from reading it; it does not
/// stop npm, which takes `npm:other@1.0.0`, a URL or a git spec as a package spec
/// and installs that instead. Digits first and no `:`, `/` or `@` anywhere is what
/// leaves npm only one reading.
///
/// `-` is allowed inside the pre-release because semver allows it there and
/// Playwright publishes with it (`1.50.0-alpha-2024-11-25`).
pub(crate) fn release_version(s: &str) -> bool {
    fn numeric(p: &str) -> bool {
        !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())
    }
    if s.len() > 64 {
        return false;
    }
    let (core, pre) = match s.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (s, None),
    };
    let mut parts = core.split('.');
    let core_ok = (0..3).all(|_| parts.next().is_some_and(numeric)) && parts.next().is_none();
    let pre_ok = pre.is_none_or(|p| {
        !p.is_empty()
            && p.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
    });
    core_ok && pre_ok
}

/// A Docker `USER` value: `user`, `uid`, `user:group` or `uid:gid`, each part in
/// the alphabet of a POSIX user or group name.
pub(crate) fn user_spec(s: &str) -> bool {
    fn part(p: &str) -> bool {
        let b = p.as_bytes();
        !b.is_empty()
            && b.len() <= 32
            && (b[0].is_ascii_alphanumeric() || b[0] == b'_')
            && b.iter()
                .all(|&c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'-'))
    }
    match s.split_once(':') {
        Some((user, group)) => part(user) && part(group),
        None => part(s),
    }
}

/// An absolute host path with no control characters, at most 4096 bytes. Absolute
/// is what keeps it from being read as an option by the program it is handed to.
#[cfg(any(target_os = "linux", test))]
pub(crate) fn host_path(s: &str) -> bool {
    s.starts_with('/') && s.len() <= 4096 && !s.chars().any(char::is_control)
}

/// `s` made safe for a log line: control characters escaped, at most `max`
/// characters kept.
pub(crate) fn for_log(s: &str, max: usize) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i == max {
            out.push('…');
            break;
        }
        if c.is_control() {
            out.extend(c.escape_default());
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- bounded output --------------------------------------------------------

    fn cmd(program: &str, args: &[&str]) -> Command {
        let mut c = Command::new(program);
        c.args(args);
        c
    }

    #[tokio::test]
    async fn oversized_stdout_is_refused_with_a_clear_error() {
        let err = output_within(
            cmd("head", &["-c", "5000", "/dev/zero"]),
            Duration::from_secs(10),
            1024,
            "head",
        )
        .await
        .expect_err("5000 bytes against a 1024-byte cap");
        let msg = err.to_string();
        assert!(msg.contains("more than 1024 bytes"), "{msg}");
        assert!(msg.contains("stdout"), "{msg}");
    }

    #[tokio::test]
    async fn oversized_stderr_is_refused_too() {
        // The script is a constant of this test: nothing here reads the daemon.
        let err = output_within(
            cmd("sh", &["-c", "head -c 5000 /dev/zero >&2"]),
            Duration::from_secs(10),
            1024,
            "sh",
        )
        .await
        .expect_err("stderr is capped like stdout");
        assert!(err.to_string().contains("stderr"), "{err}");
    }

    /// A process that never stops writing is cut off at the cap and killed, not read
    /// until it ends. Without the kill this would hang for as long as `yes` runs.
    #[tokio::test]
    async fn a_stream_that_never_ends_is_cut_off_at_the_cap() {
        let started = std::time::Instant::now();
        let err = output_capped(cmd("yes", &[]), 64 * 1024, "yes")
            .await
            .expect_err("an endless stream passes any cap");
        assert!(err.to_string().contains("more than 65536 bytes"), "{err}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "must return once the cap is passed, took {:?}",
            started.elapsed()
        );
    }

    #[tokio::test]
    async fn output_within_the_cap_is_returned_whole() {
        let out = output_within(
            cmd("echo", &["hello"]),
            Duration::from_secs(10),
            OUTPUT_CAP,
            "echo",
        )
        .await
        .expect("a prompt command answers");
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
        // Exactly the cap is still within it.
        let out = output_within(
            cmd("head", &["-c", "1024", "/dev/zero"]),
            Duration::from_secs(10),
            1024,
            "head",
        )
        .await
        .expect("1024 bytes fits a 1024-byte cap");
        assert_eq!(out.stdout.len(), 1024);
    }

    /// A variant build reads its recipe from stdin (ADR-0028): every byte fed
    /// reaches the child, and the pipe is closed after them.
    #[tokio::test]
    async fn output_within_fed_hands_the_child_its_input() {
        let input = b"FROM base\nRUN true\n".repeat(10_000);
        let out = output_within_fed(
            cmd("cat", &[]),
            input.clone(),
            Duration::from_secs(10),
            OUTPUT_CAP,
            "cat",
        )
        .await
        .expect("cat reads its input to the end");
        assert!(out.status.success());
        assert_eq!(out.stdout, input);
    }

    /// The bug the time limit exists for: a wedged daemon never answers and never
    /// errors, so an unbounded call waits forever.
    #[tokio::test]
    async fn output_within_gives_up_on_a_hanging_command() {
        let started = tokio::time::Instant::now();
        let err = output_within(
            cmd("sleep", &["30"]),
            Duration::from_millis(150),
            OUTPUT_CAP,
            "sleep test",
        )
        .await
        .expect_err("a command outliving its budget must error, not hang");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "bounding must return promptly, took {:?}",
            started.elapsed()
        );
        assert!(err.to_string().contains("exceeded"), "{err}");
    }

    // --- typed parsing ---------------------------------------------------------

    #[derive(Debug, serde::Deserialize, PartialEq)]
    struct Row {
        #[serde(rename = "Name")]
        name: String,
    }

    #[test]
    fn a_line_that_is_not_the_json_asked_for_is_skipped_not_fatal() {
        let text = "{\"Name\":\"faber-vol-1-2\"}\nnot json\n\n{\"Name\":5}\n{\"Name\":\"b\"}\n";
        assert_eq!(
            json_lines::<Row>(text, "test"),
            vec![
                Row {
                    name: "faber-vol-1-2".into()
                },
                Row { name: "b".into() }
            ]
        );
    }

    /// The reason the listings are JSON: a value carrying a newline is one escaped
    /// string inside one line, so it cannot become a second row of its own.
    #[test]
    fn a_newline_inside_a_value_cannot_forge_a_row() {
        let text = r#"{"Name":"a\n{\"Name\":\"forged\"}"}"#;
        let rows = json_lines::<Row>(text, "test");
        assert_eq!(rows.len(), 1);
        assert!(
            !object_name(&rows[0].name),
            "and the value itself is refused"
        );
    }

    /// The all-or-nothing variants, for a listing that shields or charges: one row
    /// faber cannot read makes the whole listing unreadable, never one row short.
    #[test]
    fn a_shielding_listing_with_one_unreadable_row_is_unreadable_whole() {
        let good = "{\"Name\":\"a\"}\n\n{\"Name\":\"b\"}\n";
        assert_eq!(
            json_lines_all::<Row>(good, "test").map(|rows| rows.len()),
            Some(2)
        );
        assert_eq!(
            json_lines_all::<Row>("{\"Name\":\"a\"}\nnot json\n", "test"),
            None
        );
        let check = |s: &str| match s {
            "skip" => Ok(None),
            s => number(s).map(Some).ok_or("not a number"),
        };
        assert_eq!(
            accept_all(["1", "skip", "3"], "test", check),
            Some(vec![1, 3])
        );
        assert_eq!(accept_all(["1", "x", "3"], "test", check), None);
    }

    #[test]
    fn accept_keeps_maps_and_skips_in_one_pass() {
        let kept = accept(["1", "x", "3", "skip"], "test", |s| match s {
            "skip" => Ok(None),
            s => number(s).map(Some).ok_or("not a number"),
        });
        assert_eq!(kept, vec![1, 3]);
    }

    // --- the values -------------------------------------------------------------

    #[test]
    fn ids_are_hex_of_the_right_length() {
        let full = "51e4935e3252c67d384e60ac3cb97cf7d436e5b29eb9b2ea3f822415d84cd3f2";
        assert!(object_id(full));
        assert!(!object_id(&full[..12]), "a short id is not a full one");
        assert!(!object_id(&full.to_uppercase()), "Docker prints lowercase");
        assert!(!object_id(&format!("{}z", &full[..63])));
        assert!(!object_id(&format!("{full};rm -rf /")));
        assert!(image_id(&format!("sha256:{full}")));
        assert!(!image_id(full), "an image id says its algorithm");
        assert!(!image_id(&format!("sha512:{full}")));
    }

    #[test]
    fn names_stay_in_dockers_own_alphabet() {
        for good in ["faber-vol-1-2", "faber-cache-owner-demo", "a", "x_y.z-1"] {
            assert!(object_name(good), "{good}");
        }
        for bad in [
            "", "-rf", "--help", "/:/c", "vol:/etc", "a b", "$(id)", "a;b", "a`b`", "a\nb", "../x",
            "x/y",
        ] {
            assert!(!object_name(bad), "{bad:?}");
        }
        assert!(!object_name(&"a".repeat(256)), "an oversized name");
    }

    #[test]
    fn image_refs_admit_registries_tags_and_digests_only() {
        for good in [
            "ghcr.io/owner/my-runner:2.335.1",
            "faber-runner:0123456789ab-pw-1.61.1",
            "localhost:5000/x:y",
            "busybox",
            "sha256:00ff",
            "repo@sha256:00ff",
        ] {
            assert!(image_ref(good), "{good}");
        }
        for bad in ["", "-f", "<none>:<none>", "a b", "x;y", "x'y", "x\"y"] {
            assert!(!image_ref(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_repo_slug_cannot_steer_a_url() {
        for good in ["owner/demo", "o/r", "owner-2/demo.repo_x"] {
            assert!(repo_slug(good), "{good}");
        }
        for bad in [
            "owner",
            "owner/",
            "/demo",
            "owner/demo/actions",
            "x/../../orgs/victim",
            "owner/..",
            "owner/.",
            "owner/demo?name=x",
            "owner/demo#x",
            "-owner/demo",
            "owner/de mo",
        ] {
            assert!(!repo_slug(bad), "{bad:?}");
        }
    }

    /// An Enterprise Managed Users login is `{handle}_{shortcode}`, so an owner may
    /// carry `_` after its first character. Nothing else in the rule loosens.
    #[test]
    fn an_enterprise_managed_owner_may_carry_an_underscore() {
        for good in ["octocat_acme/demo", "octo-cat_acme/demo_x.y"] {
            assert!(repo_slug(good), "{good}");
        }
        for bad in [
            "_acme/demo",
            "octocat_acme",
            "octocat_acme/",
            "octocat_acme/demo/actions",
            "octocat_acme/..",
            "octo.cat/demo",
            "octocat_acme_with_a_handle_far_too_long_x/demo",
        ] {
            assert!(!repo_slug(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_claim_id_is_two_numbers_and_nothing_else() {
        assert_eq!(claim_id("1001-1756000000000"), Some((1001, 1756000000000)));
        assert_eq!(runner_name("faber-7-1000"), Some((7, 1000)));
        // #94: the minting instance rides at the end.
        assert_eq!(
            claim_id("1001-1756000000000-builder"),
            Some((1001, 1756000000000))
        );
        assert_eq!(runner_name("faber-7-1000-h1a2b3c4"), Some((7, 1000)));
        for bad in [
            "",
            "7",
            "7-",
            "-7",
            "7-x",
            "7-1-2",
            "7-1;id",
            " 7-1",
            "+7-1",
            "7-1-",
            "7-1-Builder",
            "7-1-a;id",
            "7-1-a-b",
            "7-1-abcdefghijklmnopq",
        ] {
            assert_eq!(claim_id(bad), None, "{bad:?}");
        }
        assert_eq!(runner_name("faber-runner-7-1000"), None);
        assert_eq!(runner_name("7-1000"), None);
    }

    #[test]
    fn label_text_refuses_control_characters_and_oversized_values() {
        assert!(label_text("owner/demo::12345::test + build", 1024));
        assert!(label_text("Playwright E2E (shard 1/3)", 1024));
        assert!(!label_text("a\tb", 1024));
        assert!(!label_text("a\nb", 1024));
        assert!(!label_text("a\u{1b}[31m", 1024));
        assert!(!label_text(&"x".repeat(1025), 1024));
    }

    #[test]
    fn versions_users_and_projects_are_held_to_their_own_alphabets() {
        assert!(version("1.61.1"));
        assert!(version("1.62.0-alpha.1+exp"));
        assert!(!version("1.2.3; rm -rf /"));
        assert!(!version("1.2.3\"$(id)\""));
        assert!(!version(""));

        for good in [
            "root",
            "runner",
            "1001",
            "1001:1001",
            "runner:docker",
            "_apt",
        ] {
            assert!(user_spec(good), "{good}");
        }
        for bad in ["", "a b", "runner;id", "$(id)", "a:b:c", ":x", "-u"] {
            assert!(!user_spec(bad), "{bad:?}");
        }
    }

    /// #113: what a lockfile pins reaches `npx --yes "playwright@${PW_VERSION}"`.
    #[test]
    fn a_repo_version_is_strict_semver_and_nothing_npm_reads_as_another_spec() {
        for good in [
            "1.61.1",
            "0.0.0",
            "1.62.0-alpha.1",
            "1.50.0-alpha-2024-11-25",
            "1.48.0-beta-1728034490000",
        ] {
            assert!(release_version(good), "{good}");
        }
        for bad in [
            "",
            "1.61",
            "1.61.1.1",
            "v1.61.1",
            "1.61.x",
            "^1.61.1",
            "1.61.1+build",
            "1.61.1-",
            "1..1",
            "npm:evil@1.0.0",
            "1.0.0-npm:evil@1.0.0",
            "1.0.0-a/b",
            "https://example.com/x.tgz",
            "github:owner/demo",
            "latest",
            "1.61.1 ",
            "1.61.1\n",
            "-1.61.1",
        ] {
            assert!(!release_version(bad), "{bad:?}");
        }
        assert!(!release_version(&format!("1.0.0-{}", "a".repeat(64))));
    }

    #[test]
    fn numbers_are_digits_only() {
        assert_eq!(number("6600"), Some(6600));
        for bad in [
            "",
            "-1",
            "+1",
            " 1",
            "1.0",
            "1e3",
            "0x10",
            "99999999999999999999999",
        ] {
            assert_eq!(number(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_host_path_is_absolute_and_printable() {
        assert!(host_path("/var/lib/docker"));
        assert!(!host_path("var/lib/docker"));
        assert!(!host_path("--help"));
        assert!(!host_path("/var/lib\n/docker"));
    }

    #[test]
    fn a_log_line_carries_no_control_characters_and_has_an_end() {
        assert_eq!(for_log("a\nb", 10), "a\\nb");
        assert_eq!(for_log("abcdef", 3), "abc…");
        assert_eq!(for_log("\u{1b}[31m", 10), "\\u{1b}[31m");
    }
}
