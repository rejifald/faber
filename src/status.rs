//! `faber status` — the audit log, read back.
//!
//! The dashboard answers "what is faber doing" for someone sitting in front of a
//! browser. This answers "what went wrong" for someone in a terminal, over SSH, or
//! in a script — from the durable trail rather than from live state, so it works
//! on a daemon that is down, and reports the outage that took it down.
//!
//! Everything here reads; nothing writes. The one live thing it does is probe the
//! control panel, best-effort, to say whether a daemon is up at all — the answer
//! the log by construction cannot give.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::audit::{now_ms, Audit, Entry};

/// The most of one free-text field (a cause, a label, the daemon's
/// `last_error`) the human report prints. Each is a sentence or a line of
/// output; nothing a writer of the log chooses can make one a screenful.
const FIELD_MAX: usize = 2048;

/// How long to wait for the control panel before calling the daemon unreachable.
/// Loopback to a process that is either listening or isn't — a full second is
/// already generous, and `status` must stay usable when the daemon is wedged.
const PROBE_TIMEOUT: Duration = Duration::from_secs(1);

/// Left margin for a failed run's continuation lines, aligned under its label so
/// the detail and the cause read as belonging to the run above them.
const INDENT: &str = "              ";

/// What to report on.
pub struct Opts {
    /// How far back to read.
    pub since: Duration,
    /// Restrict to one `owner/repo`.
    pub repo: Option<String>,
    /// How many failed runs to print in full (all of them are still counted).
    pub limit: usize,
    /// Control-panel port to probe for live state.
    pub port: u16,
    /// Emit the summary as JSON instead of a human report.
    pub json: bool,
    /// The log is this host's SYSTEM audit log (ADR-0025), which the operator
    /// reads through group `_faber_operator` — so a refusal to read it has a
    /// known cause and a known fix.
    pub system_log: bool,
}

/// Parse a `--since` window: `90s`, `45m`, `24h`, `7d`.
///
/// A bare number is rejected rather than guessed at: `--since 24` is as plausibly
/// a day as a minute, and a status report over the wrong window is worse than an
/// error, because it looks like an answer.
pub fn parse_since(spec: &str) -> anyhow::Result<Duration> {
    let spec = spec.trim();
    let (n, unit) =
        spec.split_at(spec.find(|c: char| !c.is_ascii_digit()).ok_or_else(|| {
            anyhow::anyhow!("--since {spec} needs a unit: s, m, h or d (e.g. 24h)")
        })?);
    let n: u64 = n
        .parse()
        .map_err(|_| anyhow::anyhow!("--since {spec}: {n:?} is not a number"))?;
    let secs = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        other => anyhow::bail!("--since {spec}: unknown unit {other:?} (use s, m, h or d)"),
    };
    Ok(Duration::from_secs(n.saturating_mul(secs)))
}

/// The digest of one window of the audit log.
#[derive(Debug, Default, Serialize)]
pub struct Summary {
    /// The oldest timestamp included.
    pub since_ms: u64,
    /// Lines read in the window (every kind, before any repo filter).
    pub lines: usize,
    /// Finished runs by `result` — `success`, `failure`, `infra_error`, …
    pub runs: BTreeMap<String, u64>,
    /// Gate decisions by verdict *family* (`refuse:actor`, not
    /// `refuse:actor:dependabot[bot]`) — the reason class is the diagnosis, and
    /// the actor name is unbounded cardinality that would bury it.
    pub decisions: BTreeMap<String, u64>,
    /// Jobs auto-re-run (ADR-0003).
    pub retries: u64,
    /// Actionable runner-image drift checks (ADR-0001).
    pub drift: u64,
    /// faber's own faults and the recoveries that ended them, oldest first.
    pub faults: Vec<Entry>,
    /// Runs that did not succeed, newest first. Excludes `reclaimed` — a
    /// deliberate stand-down is not something that went wrong.
    pub failures: Vec<Entry>,
}

/// Fold a window of audit lines into the digest. Pure — the whole report is a
/// function of the lines, which is what makes it testable without a daemon, a
/// log file, or a clock.
pub fn summarize(entries: Vec<Entry>, repo: Option<&str>, since_ms: u64) -> Summary {
    let mut s = Summary {
        since_ms,
        lines: entries.len(),
        ..Default::default()
    };
    for e in entries {
        // The repo filter is applied after the line count, so a filtered report
        // still says how much log it read — otherwise "0 lines" reads as "faber
        // wrote nothing" when it means "nothing about this repo".
        if repo.is_some_and(|r| e.repo != r) {
            continue;
        }
        match e.kind.as_str() {
            "outcome" => {
                let result = e.result.clone().unwrap_or_else(|| "unknown".to_string());
                *s.runs.entry(result.clone()).or_default() += 1;
                if result != "success" && result != "reclaimed" {
                    s.failures.push(e);
                }
            }
            "decision" => {
                let verdict = e.verdict.as_deref().unwrap_or("unknown");
                *s.decisions.entry(verdict_family(verdict)).or_default() += 1;
            }
            "retry" => s.retries += 1,
            "drift" => s.drift += 1,
            "fault" => s.faults.push(e),
            _ => {}
        }
    }
    s.failures
        .sort_unstable_by_key(|f| std::cmp::Reverse(f.ts_ms));
    s
}

/// `refuse:actor:dependabot[bot]` → `refuse:actor`; `provision` → `provision`.
/// Keeps the reason and drops the unbounded tail.
fn verdict_family(verdict: &str) -> String {
    verdict.split(':').take(2).collect::<Vec<_>>().join(":")
}

/// The live state `faber status` borrows from a running daemon. A deliberately
/// small subset of the panel's `StateView` — the fields the log cannot supply.
///
/// `pub(crate)` so that [`crate::standby`]'s tests can prove the stub a daemon
/// serves while it waits for GitHub still parses here.
#[derive(Debug, Deserialize)]
pub(crate) struct Live {
    pub(crate) paused: bool,
    pub(crate) draining: bool,
    pub(crate) uptime_s: u64,
    pub(crate) healthy: bool,
    #[serde(default)]
    pub(crate) last_error: Option<String>,
    #[serde(default)]
    pub(crate) inflight: Vec<serde_json::Value>,
    /// Present only while the daemon is in standby, waiting for GitHub
    /// ([`crate::standby`]). Its contents are for the panel; its presence is
    /// what this report needs.
    #[serde(default)]
    pub(crate) standby: Option<serde_json::Value>,
    /// The GitHub credential (ADR-0027), as `/api/state` reports it. Absent
    /// from a daemon older than the store.
    #[serde(default)]
    pub(crate) credential: Option<serde_json::Value>,
    /// The memory budget as `/api/state` reports it; `budget.vm` is the VMs'
    /// admission (ADR-0026). Absent from an older daemon.
    #[serde(default)]
    pub(crate) budget: Option<serde_json::Value>,
}

/// The `wall` and `image` lines from `budget.vm`: the wall's verdict, and whether
/// the Linux image is the one this release pins. Both come from the daemon, which
/// is the one that reads the wall (a verdict older than its limit already arrives
/// as `red`, with its age in the detail). Empty from a daemon that predates the
/// VMs.
fn vm_lines(vm: &serde_json::Value) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(wall) = vm.get("wall").and_then(|v| v.as_str()) {
        let verdict = match wall {
            "open" => "green",
            "out_of_date" => "out of date",
            other => other,
        };
        let mut line = format!("wall     {}", field(verdict));
        if let Some(why) = vm.get("wall_detail").and_then(|v| v.as_str()) {
            line.push_str(&format!(" · {}", field(why)));
        }
        lines.push(line);
    }
    // ADR-0051's session: only where the record names the session account.
    if let Some(session) = vm
        .get("session")
        .and_then(|v| v.as_str())
        .filter(|s| *s != "not_required")
    {
        let mut line = format!("session  {}", field(session));
        if let Some(why) = vm.get("session_detail").and_then(|v| v.as_str()) {
            line.push_str(&format!(" · {}", field(why)));
        }
        lines.push(line);
    }
    if let Some(linux) = vm.pointer("/images/linux").and_then(|v| v.as_str()) {
        let standing = match linux {
            "current" => "current".to_string(),
            "out_of_date" => "out of date · run `sudo faber setup`".to_string(),
            "unknown" => "not judged yet".to_string(),
            other => field(other).into_owned(),
        };
        let mut line = format!("image    linux {standing}");
        if let Some(problem) = vm.get("runner_image").and_then(|v| v.as_str()) {
            line.push_str(&format!(" · runner image: {}", field(problem)));
        }
        lines.push(line);
    }
    // ADR-0049 §7: each golden image a profile names, and what holds it back.
    for row in vm
        .get("golden")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
    {
        let (Some(image), Some(standing)) = (
            row.get("image").and_then(|v| v.as_str()),
            row.get("standing").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let mut line = format!("image    {} {}", field(image), field(standing));
        if let Some(why) = row.get("detail").and_then(|v| v.as_str()) {
            line.push_str(&format!(" · {}", field(why)));
        }
        lines.push(line);
    }
    lines
}

/// The `token` line: the credential's state, whose it is, when it expires,
/// and why, for anything but a plain `stored`. `None` when the daemon did not
/// say.
fn credential_line(c: &serde_json::Value) -> Option<String> {
    let status = c.get("status")?.as_str()?;
    let mut line = format!("token    {status}");
    if let Some(owner) = c.get("owner").and_then(|v| v.as_str()) {
        line.push_str(&format!(" · {}", field(owner)));
    }
    if let Some(ms) = c.get("expires_at_ms").and_then(|v| v.as_u64()) {
        line.push_str(&format!(" · expires {}", crate::credential::date_of(ms)));
    }
    if let Some(reason) = c.get("reason").and_then(|v| v.as_str()) {
        line.push_str(&format!(" · {}", field(reason)));
    }
    let missing = c
        .get("missing_permissions")
        .and_then(|v| v.as_array())
        .map_or(0, Vec::len);
    if missing > 0 {
        line.push_str(&format!(
            " · {missing} missing permission{} (see the dashboard)",
            if missing == 1 { "" } else { "s" }
        ));
    }
    if let Some(warning) = c.get("file_warning").and_then(|v| v.as_str()) {
        line.push_str(&format!(" · {}", field(warning)));
    }
    Some(line)
}

/// Whether the log at `path` can be read: `Ok` for a readable file and for no
/// file at all (a daemon that never ran here writes nothing, which is an empty
/// report, not an error).
fn readable(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::File::open(path) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Why the log could not be read, and what to do about it.
///
/// Refused rather than reported as empty: a report over a log it could not read
/// says "nothing happened", which looks exactly like an answer. Under SYSTEM a
/// permission error on the host's own log has one cause: the account running
/// this is not yet in group `_faber_operator` (`sudo faber setup` adds the
/// operator, and a group change reaches only sessions started after it).
fn unreadable(path: &str, err: &std::io::Error, system_log: bool) -> String {
    if system_log && err.kind() == std::io::ErrorKind::PermissionDenied {
        format!(
            "cannot read {path}: {err} — your account needs group {}: open a new terminal \
             after `sudo faber setup`",
            crate::platform::layout::FABER_OPERATOR_GROUP
        )
    } else {
        format!("cannot read {path}: {err}")
    }
}

/// Ask the control panel how the daemon is doing. `None` for "no daemon
/// answered", which is itself a finding and is reported as one — not an error,
/// since reading the log of a stopped daemon is a first-class use of this command.
async fn probe(port: u16) -> Option<Live> {
    // Over the control socket, or (on a LEGACY host) plain loopback for a daemon
    // from before ADR-0028. A daemon that refuses this account reads as "not
    // answering", with the reason on stderr.
    let socket = crate::platform::control_socket().unwrap_or_default();
    let fallback = crate::control::fallback_here(port);
    let answer = tokio::time::timeout(
        PROBE_TIMEOUT,
        crate::control::request(&socket, fallback, reqwest::Method::GET, "/api/state", None),
    )
    .await
    .ok()?;
    let (status, text) = match answer {
        Ok(answer) => answer,
        Err(crate::control::ClientError::Refused { socket }) => {
            eprintln!("{}", crate::control::refused_text(&socket));
            return None;
        }
        Err(_) => return None,
    };
    if !status.is_success() {
        return None;
    }
    serde_json::from_str::<Live>(&text).ok()
}

/// Render the report to stdout.
pub async fn run(audit: &Audit, opts: Opts) -> anyhow::Result<()> {
    if let Err(e) = readable(audit.path()) {
        anyhow::bail!(unreadable(
            &audit.path().display().to_string(),
            &e,
            opts.system_log
        ));
    }
    let now = now_ms();
    let since_ms = now.saturating_sub(opts.since.as_millis() as u64);
    let summary = summarize(audit.read_since(since_ms), opts.repo.as_deref(), since_ms);

    let text = if opts.json {
        json_report(&summary)?
    } else {
        report(
            &summary,
            probe(opts.port).await.as_ref(),
            &audit.path().display().to_string(),
            &opts,
            now,
        )
    };
    emit(&text);
    Ok(())
}

/// [`render`], made safe to print. Nearly every field in the report is text
/// faber did not write: job, workflow and repo names come from GitHub, and
/// whoever pushes a workflow chooses them; causes quote Docker and GitHub; and
/// `last_error` is whatever the daemon says. Neutralized once, over the whole
/// report ([`crate::term::sanitize`]), so a field added later is covered too.
fn report(
    summary: &Summary,
    live: Option<&Live>,
    audit_path: &str,
    opts: &Opts,
    now: u64,
) -> String {
    crate::term::sanitize(&render(summary, live, audit_path, opts, now)).into_owned()
}

/// `--json`: the digest, still valid JSON, with nothing in it a terminal would
/// act on ([`crate::term::sanitize_json`]).
fn json_report(summary: &Summary) -> anyhow::Result<String> {
    Ok(crate::term::sanitize_json(&serde_json::to_string_pretty(summary)?).into_owned())
}

/// One free-text field, cut to [`FIELD_MAX`]. [`report`] neutralizes the rest.
fn field(text: &str) -> Cow<'_, str> {
    crate::term::sanitize_bounded(text, FIELD_MAX)
}

/// Write the report, treating a closed stdout as success.
///
/// `faber status | head` and `| grep -q` close the pipe early, and the default
/// `println!` behaviour on that is a panic — a Rust backtrace on top of the report
/// you asked for, from a command whose entire purpose is being read and filtered.
fn emit(text: &str) {
    use std::io::Write;
    let out = std::io::stdout();
    let mut out = out.lock();
    // Any write error here is the reader having gone away (or a full disk on a
    // redirect); neither is something this command can or should report on.
    let _ = out.write_all(text.as_bytes());
    let _ = out.write_all(b"\n");
    let _ = out.flush();
}

/// Render the whole report. Pure: takes the digest, whatever the daemon said, and
/// the clock, and returns the text — so the layout can be tested without a log
/// file, a daemon or a terminal.
fn render(
    summary: &Summary,
    live: Option<&Live>,
    audit_path: &str,
    opts: &Opts,
    now: u64,
) -> String {
    let mut out = Vec::new();
    out.push(format!("faber · {audit_path}"));
    out.push(format!(
        "window   last {} · {} line{}{}",
        human_dur(opts.since.as_secs()),
        summary.lines,
        if summary.lines == 1 { "" } else { "s" },
        match &opts.repo {
            Some(r) => format!(" · {r} only"),
            None => String::new(),
        }
    ));

    out.push(match live {
        Some(live) => {
            let state = if live.draining {
                "draining"
            } else if live.standby.is_some() {
                // Up, answering, and claiming nothing until GitHub answers.
                "standby"
            } else if live.paused {
                "paused"
            } else {
                "running"
            };
            format!(
                "daemon   {state} · up {} · {} in flight{}",
                human_dur(live.uptime_s),
                live.inflight.len(),
                match (live.healthy, &live.last_error) {
                    (true, _) => String::new(),
                    (false, Some(e)) => format!(" · UNHEALTHY: {}", field(e)),
                    (false, None) => " · UNHEALTHY".to_string(),
                }
            )
        }
        // Not an error: the log outlives the daemon, and reading it after a crash
        // is the case this command exists for.
        None => format!(
            "daemon   not answering on {} or 127.0.0.1:{} — down, on another port, or its \
             control socket refuses this account",
            crate::platform::control_socket().map_or_else(
                || "its control socket".to_string(),
                |p| p.display().to_string()
            ),
            opts.port
        ),
    });

    if let Some(line) = live
        .and_then(|l| l.credential.as_ref())
        .and_then(credential_line)
    {
        out.push(line);
    }
    if let Some(vm) = live
        .and_then(|l| l.budget.as_ref())
        .and_then(|b| b.get("vm"))
    {
        out.extend(vm_lines(vm));
    }
    // ADR-0061 §2: the Linux VM's disk, while a grow is pending, blocked or
    // faulted.
    if let Some(line) = live
        .and_then(|l| l.budget.as_ref())
        .and_then(|b| b.pointer("/disk/vm_grow"))
        .and_then(|g| serde_json::from_value::<crate::vm::grow::GrowView>(g.clone()).ok())
        .and_then(|g| g.line())
    {
        out.push(format!("disk     {}", field(&line)));
    }
    out.push(format!("runs     {}", tally(&summary.runs, "none")));
    out.push(format!("gate     {}", tally(&summary.decisions, "none")));
    // Named only when non-zero: "0 drift checks" is a line about nothing, and
    // every such line is one more thing between a reader and the failures below.
    let also: Vec<String> = [
        (summary.retries, "auto-retried"),
        (summary.drift, "drift checks"),
    ]
    .iter()
    .filter(|(n, _)| *n > 0)
    .map(|(n, what)| format!("{n} {what}"))
    .collect();
    if !also.is_empty() {
        out.push(format!("also     {}", also.join(" · ")));
    }

    if !summary.faults.is_empty() {
        out.push(format!(
            "\nfaber faults — {} (faber itself, not a job)",
            summary.faults.len()
        ));
        for f in &summary.faults {
            let line = format!(
                "  {:>10}  {:<10} {:<10} {}",
                ago(now, f.ts_ms),
                f.result.as_deref().unwrap_or("?"),
                f.action.as_deref().unwrap_or("?"),
                field(f.cause.as_deref().unwrap_or("")),
            );
            // A `recovered` line carries no cause, and the padding above would
            // otherwise leave it trailing eleven spaces.
            out.push(line.trim_end().to_string());
        }
    }

    if summary.failures.is_empty() {
        out.push("\nno failed runs in this window.".to_string());
        return out.join("\n");
    }

    let shown = summary.failures.len().min(opts.limit);
    out.push(format!(
        "\nfailed runs — {}{}",
        summary.failures.len(),
        if shown < summary.failures.len() {
            format!(" (showing {shown}; --limit for more)")
        } else {
            String::new()
        }
    ));
    for f in summary.failures.iter().take(shown) {
        out.push(format!(
            "  {:>10}  {:<11}  {}",
            ago(now, f.ts_ms),
            f.result.as_deref().unwrap_or("?"),
            field(
                &f.label
                    .clone()
                    .unwrap_or_else(|| format!("{} job {}", f.repo, f.job_id))
            ),
        ));
        out.push(format!("{INDENT}{}", detail(f)));
        if let Some(cause) = &f.cause {
            out.push(format!("{INDENT}{}", field(cause)));
        }
    }
    out.join("\n")
}

/// The one-line context under a failed run: what faber minted for it, which
/// attempt it was, and where the elapsed time went.
fn detail(f: &Entry) -> String {
    let mut parts = Vec::new();
    if let Some(p) = &f.profile {
        parts.push(p.clone());
    }
    if let Some(a) = f.attempt.filter(|a| *a > 1) {
        parts.push(format!("attempt {a}"));
    }
    if let Some(d) = f.duration_ms {
        // The parked share only earns its words when it is a real part of the
        // elapsed time — otherwise every line carries "(0s parked)".
        let parked = match f.wait_ms.filter(|w| *w >= 5_000) {
            Some(w) => format!(" ({} parked)", human_dur(w / 1000)),
            None => String::new(),
        };
        parts.push(format!("ran {}{parked}", human_dur(d / 1000)));
    }
    match (f.mem_peak_runner_mib, f.mem_peak_dind_mib) {
        (None, None) => {}
        (r, d) => parts.push(format!(
            "peak {}/{} MiB{}",
            r.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
            d.map(|v| v.to_string()).unwrap_or_else(|| "?".into()),
            match (f.oom_runner, f.oom_dind) {
                (Some(true), _) => " OOM:runner",
                (_, Some(true)) => " OOM:dind",
                _ => "",
            }
        )),
    }
    if let Some(r) = &f.runner {
        parts.push(r.clone());
    }
    parts.join(" · ")
}

/// `"36 success · 2 failure"`, or `empty` when there is nothing to tally.
fn tally(counts: &BTreeMap<String, u64>, empty: &str) -> String {
    if counts.is_empty() {
        return empty.to_string();
    }
    let total: u64 = counts.values().sum();
    let parts: Vec<String> = counts.iter().map(|(k, n)| format!("{n} {k}")).collect();
    format!("{total} · {}", parts.join(" · "))
}

/// Seconds as `45s` / `12m30s` / `3h12m` / `4d`. Coarse on purpose: this is a
/// report about minutes and hours, and a fourth significant figure on a job
/// duration is noise. A zero remainder is dropped rather than printed — `24h`
/// reading back as `1d0h` is the kind of small wrongness that makes a reader stop
/// trusting the rest of the numbers.
fn human_dur(secs: u64) -> String {
    let (whole, unit, rest) = match secs {
        s if s < 60 => return format!("{s}s"),
        s if s < 3600 => (s / 60, 'm', s % 60),
        s if s < 86_400 => (s / 3600, 'h', (s % 3600) / 60),
        s => (s / 86_400, 'd', (s % 86_400) / 3600),
    };
    let sub = match unit {
        'm' => 's',
        'h' => 'm',
        _ => 'h',
    };
    match rest {
        0 => format!("{whole}{unit}"),
        r => format!("{whole}{unit}{r}{sub}"),
    }
}

/// How long ago `ts_ms` was, relative to `now`.
///
/// Relative rather than wall-clock, and deliberately: faber depends on no date
/// library, and hand-rolling a civil-calendar conversion — with the local UTC
/// offset it would need to not be misleading — is a great deal of surface for a
/// column whose only job is "how recent is this". The exact stamp is one
/// `--json` away for anyone correlating against GitHub's own timeline.
fn ago(now: u64, ts_ms: u64) -> String {
    format!("{} ago", human_dur(now.saturating_sub(ts_ms) / 1000))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: &str, ts_ms: u64) -> Entry {
        Entry {
            ts_ms,
            kind: kind.to_string(),
            repo: "o/r".to_string(),
            job_id: 1,
            run_id: None,
            event: None,
            dind: None,
            actor: None,
            verdict: None,
            result: None,
            duration_ms: None,
            label: None,
            profile: None,
            runner: None,
            wait_ms: None,
            attempt: None,
            cause: None,
            end: None,
            executed_job_id: None,
            class: None,
            action: None,
            mem_peak_runner_mib: None,
            mem_peak_dind_mib: None,
            oom_runner: None,
            oom_dind: None,
            reason: None,
            cooloff_ms: None,
            until_ms: None,
            armed_ms: None,
            services: None,
        }
    }

    fn outcome(result: &str, ts_ms: u64) -> Entry {
        Entry {
            result: Some(result.to_string()),
            ..entry("outcome", ts_ms)
        }
    }

    #[test]
    fn since_needs_a_unit() {
        assert_eq!(parse_since("24h").unwrap(), Duration::from_secs(86_400));
        assert_eq!(parse_since("90s").unwrap(), Duration::from_secs(90));
        assert_eq!(parse_since("7d").unwrap(), Duration::from_secs(604_800));
        // A bare number is ambiguous enough that guessing would silently report
        // the wrong window — which looks exactly like a correct report.
        assert!(parse_since("24").is_err());
        assert!(parse_since("24w").is_err());
        assert!(parse_since("h").is_err());
    }

    #[test]
    fn failures_exclude_deliberate_stand_downs() {
        // A reclaim is faber choosing not to run a job that no longer needs one.
        // Counting it as a failure would put a steady stream of correct behaviour
        // at the top of a report whose whole purpose is to show the incorrect kind.
        let s = summarize(
            vec![
                outcome("success", 10),
                outcome("reclaimed", 20),
                outcome("infra_error", 30),
                outcome("failure", 40),
            ],
            None,
            0,
        );
        assert_eq!(s.runs["success"], 1);
        assert_eq!(s.runs["reclaimed"], 1);
        let results: Vec<_> = s
            .failures
            .iter()
            .map(|f| f.result.clone().unwrap())
            .collect();
        assert_eq!(results, vec!["failure", "infra_error"], "newest first");
    }

    #[test]
    fn verdicts_group_by_reason_not_by_actor() {
        // The actor is unbounded — one line per bot would bury the one fact that
        // matters, which is that actor-refusals are happening at all.
        let s = summarize(
            vec![
                Entry {
                    verdict: Some("refuse:actor:dependabot[bot]".to_string()),
                    ..entry("decision", 1)
                },
                Entry {
                    verdict: Some("refuse:actor:someone-else".to_string()),
                    ..entry("decision", 2)
                },
                Entry {
                    verdict: Some("provision".to_string()),
                    ..entry("decision", 3)
                },
            ],
            None,
            0,
        );
        assert_eq!(s.decisions["refuse:actor"], 2);
        assert_eq!(s.decisions["provision"], 1);
    }

    #[test]
    fn the_repo_filter_does_not_hide_how_much_log_was_read() {
        // "0 lines" would read as "faber recorded nothing" — a very different
        // (and alarming) claim than "nothing about this repo".
        let s = summarize(
            vec![outcome("success", 1), outcome("failure", 2)],
            Some("other/repo"),
            0,
        );
        assert_eq!(s.lines, 2, "the window's line count is pre-filter");
        assert!(s.runs.is_empty(), "but nothing matched the repo");
    }

    #[test]
    fn faults_keep_both_edges_of_an_outage() {
        // The pair is the point: a `failed` with no `recovered` after it is a
        // daemon still blind right now, and that has to be readable at a glance.
        let s = summarize(
            vec![
                Entry {
                    result: Some("discovery".to_string()),
                    action: Some("failed".to_string()),
                    cause: Some("http transport error".to_string()),
                    ..entry("fault", 1)
                },
                Entry {
                    result: Some("discovery".to_string()),
                    action: Some("recovered".to_string()),
                    ..entry("fault", 2)
                },
            ],
            None,
            0,
        );
        assert_eq!(s.faults.len(), 2);
        assert_eq!(s.faults[0].action.as_deref(), Some("failed"));
        assert_eq!(s.faults[1].action.as_deref(), Some("recovered"));
    }

    #[test]
    fn a_parked_run_says_where_its_time_went() {
        let f = Entry {
            profile: Some("linux-docker".to_string()),
            attempt: Some(3),
            duration_ms: Some(600_000),
            wait_ms: Some(540_000),
            runner: Some("faber-77-1000".to_string()),
            ..outcome("timeout", 1)
        };
        let d = detail(&f);
        assert!(d.contains("attempt 3"), "{d}");
        assert!(d.contains("ran 10m (9m parked)"), "{d}");
        assert!(d.contains("faber-77-1000"), "{d}");
    }

    #[test]
    fn a_negligible_park_is_not_mentioned() {
        let f = Entry {
            duration_ms: Some(60_000),
            wait_ms: Some(200),
            ..outcome("failure", 1)
        };
        assert_eq!(detail(&f), "ran 1m");
    }

    // --- ADR-0025: reading a SYSTEM host's log --------------------------------

    /// Under SYSTEM the log is `_faber`'s, readable through the operator
    /// group. A permission error there names the fix; anywhere else it is
    /// reported plainly, never guessed at.
    #[test]
    fn an_unreadable_system_log_says_how_to_get_read_access() {
        let denied = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let path = "/Library/Logs/faber/audit.jsonl";
        let hint = "your account needs group _faber_operator: open a new terminal after \
                    `sudo faber setup`";
        let system = unreadable(path, &denied, true);
        assert!(
            system.starts_with("cannot read /Library/Logs/faber/audit.jsonl: "),
            "{system}"
        );
        assert!(system.ends_with(hint), "{system}");
        // A LEGACY log, or one passed with --log: the error as it is.
        assert!(!unreadable(path, &denied, false).contains("_faber"));
        // Under SYSTEM, a different error is not a group problem.
        let other = std::io::Error::other("disk on fire");
        assert!(!unreadable(path, &other, true).contains("_faber"));
    }

    /// No log at all is an empty report, as it always was: a host where faber
    /// never ran has nothing to say, and that is not an error.
    #[test]
    fn a_missing_log_is_readable_as_empty() {
        let missing = std::env::temp_dir().join(format!(
            "faber-status-missing-{}/audit.jsonl",
            std::process::id()
        ));
        assert!(readable(&missing).is_ok());
    }

    /// The `token` line names the credential's state, whose it is, its expiry
    /// and why, and only counts missing permissions, which the dashboard lists.
    #[test]
    fn the_token_line_says_the_state_owner_expiry_and_why() {
        let ms = crate::retry::parse_rfc3339_to_ms("2026-12-31T12:00:00Z").unwrap();
        let line = credential_line(&serde_json::json!({
            "status": "expiring",
            "owner": "owner",
            "expires_at_ms": ms,
            "reason": "the GitHub token expires in 3 days",
            "missing_permissions": [{"repo": "owner/demo"}],
        }))
        .unwrap();
        assert_eq!(
            line,
            "token    expiring · owner · expires 2026-12-31 · the GitHub token expires in 3 \
             days · 1 missing permission (see the dashboard)"
        );
        assert_eq!(
            credential_line(&serde_json::json!({"status": "missing", "owner": null})).unwrap(),
            "token    missing"
        );
        assert_eq!(credential_line(&serde_json::json!({})), None);
        assert_eq!(
            credential_line(&serde_json::json!({
                "status": "stored",
                "file_warning": "the token file is owned by _faber",
            }))
            .unwrap(),
            "token    stored · the token file is owned by _faber"
        );
    }

    /// A daemon in standby answers, and says so rather than "running".
    #[test]
    fn a_daemon_in_standby_reads_as_standby() {
        let live: Live = serde_json::from_value(serde_json::json!({
            "paused": false,
            "draining": false,
            "uptime_s": 90,
            "healthy": false,
            "last_error": "standby: could not get a GitHub credential",
            "inflight": [],
            "standby": {"since_ms": 1}
        }))
        .unwrap();
        let opts = Opts {
            since: Duration::from_secs(3600),
            repo: None,
            limit: 10,
            port: 8787,
            json: false,
            system_log: true,
        };
        let text = render(&Summary::default(), Some(&live), "audit.jsonl", &opts, 0);
        assert!(
            text.contains("daemon   standby · up 1m30s · 0 in flight · UNHEALTHY: standby: could not get a GitHub credential"),
            "{text}"
        );
    }

    /// The runbook's step-12.1 check reads the wall and the Linux image off this
    /// command: a green wall and a current image print as such, anything else says
    /// why, and a daemon without the VM view prints neither line.
    #[test]
    fn a_pending_grow_of_the_linux_vm_disk_has_a_line() {
        let live: Live = serde_json::from_value(serde_json::json!({
            "paused": false, "draining": false, "uptime_s": 5, "healthy": true,
            "budget": {"disk": {"vm_grow": {
                "disk_bytes": 64u64 << 30, "disk_allocated_bytes": 60u64 << 30,
                "max_bytes": 128u64 << 30, "pending_to_bytes": 96u64 << 30,
                "waiting_for": "waiting for an idle moment", "fault": null
            }}},
        }))
        .unwrap();
        let text = render(
            &Summary::default(),
            Some(&live),
            "audit.jsonl",
            &opts(false),
            0,
        );
        assert!(
            text.contains(
                "\ndisk     linux VM 64 GiB of at most 128 GiB; growing to 96 GiB — waiting for \
                 an idle moment\n"
            ),
            "{text}"
        );
    }

    #[test]
    fn the_wall_and_the_linux_image_have_a_line_each() {
        let at = |vm: serde_json::Value| -> String {
            let live: Live = serde_json::from_value(serde_json::json!({
                "paused": false, "draining": false, "uptime_s": 5, "healthy": true,
                "budget": {"vm": vm},
            }))
            .unwrap();
            render(
                &Summary::default(),
                Some(&live),
                "audit.jsonl",
                &opts(false),
                0,
            )
        };
        let green = at(serde_json::json!({
            "wall": "open", "wall_detail": null,
            "images": {"linux": "current", "macos": "current"}, "runner_image": null,
        }));
        assert!(green.contains("\nwall     green\n"), "{green}");
        assert!(green.contains("\nimage    linux current\n"), "{green}");

        let stale = at(serde_json::json!({
            "wall": "red",
            "wall_detail": "no wall verdict for 400s: the poller has stopped",
            "images": {"linux": "out_of_date", "macos": "unknown"},
            "runner_image": "missing from the Linux VM",
        }));
        assert!(
            stale.contains("wall     red · no wall verdict for 400s: the poller has stopped"),
            "{stale}"
        );
        assert!(
            stale.contains(
                "image    linux out of date · run `sudo faber setup` · runner image: missing from the Linux VM"
            ),
            "{stale}"
        );

        let unjudged = at(serde_json::json!({
            "wall": "out_of_date", "wall_detail": "run `sudo faber setup`",
            "images": {"linux": "unknown", "macos": "unknown"},
        }));
        assert!(
            unjudged.contains("wall     out of date · run"),
            "{unjudged}"
        );
        assert!(
            unjudged.contains("image    linux not judged yet"),
            "{unjudged}"
        );
        assert!(!unjudged.contains("\nsession "), "{unjudged}");

        // ADR-0051's session, where the record names the account.
        let down = at(serde_json::json!({
            "wall": "open", "session": "down",
            "session_detail": "log _faber_mac in: the supervisor's socket is not there",
        }));
        assert!(
            down.contains(
                "\nsession  down · log _faber_mac in: the supervisor's socket is not there\n"
            ),
            "{down}"
        );
        // ADR-0049 §7: a golden image's standing, and what to do about it.
        let golden = at(serde_json::json!({
            "wall": "open",
            "golden": [
                {"image": "xcode-27.0", "standing": "verified", "detail": null},
                {"image": "xcode-26.4", "standing": "failed:filevault",
                 "detail": "the golden image xcode-26.4 failed verification (filevault)"},
            ],
        }));
        assert!(
            golden.contains("\nimage    xcode-27.0 verified\n"),
            "{golden}"
        );
        assert!(
            golden.contains(
                "\nimage    xcode-26.4 failed:filevault · the golden image xcode-26.4 failed \
                 verification (filevault)\n"
            ),
            "{golden}"
        );
        let up = at(serde_json::json!({"wall": "open", "session": "up"}));
        assert!(up.contains("\nsession  up\n"), "{up}");
        let ungated = at(serde_json::json!({"wall": "open", "session": "not_required"}));
        assert!(!ungated.contains("\nsession "), "{ungated}");

        let old: Live = serde_json::from_value(serde_json::json!({
            "paused": false, "draining": false, "uptime_s": 5, "healthy": true,
        }))
        .unwrap();
        let text = render(
            &Summary::default(),
            Some(&old),
            "audit.jsonl",
            &opts(false),
            0,
        );
        assert!(
            !text.contains("\nwall ") && !text.contains("\nimage "),
            "{text}"
        );
    }

    // --- text faber did not write, on the operator's terminal ----------------

    /// A character a terminal would act on, anywhere in `text`.
    fn acts_on_terminal(text: &str) -> bool {
        text.chars().any(|c| {
            (c.is_control() && c != '\n' && c != '\t') || ('\u{202a}'..='\u{202e}').contains(&c)
        })
    }

    fn opts(json: bool) -> Opts {
        Opts {
            since: Duration::from_secs(3600),
            repo: None,
            limit: 10,
            port: 8787,
            json,
            system_log: false,
        }
    }

    /// Everything a log writer, GitHub or the daemon chose reaches the report as
    /// visible text: a job label (a workflow's own name), causes, the repo, a
    /// fault, and the daemon's `last_error`. The report's own layout survives.
    fn hostile() -> (Summary, Live) {
        let osc52 = "\u{1b}]52;c;cm0gLXJmIH4=\u{7}";
        let failure = Entry {
            label: Some(format!("deploy {osc52}\u{1b}[2K\rok")),
            cause: Some("exit 1\u{9b}31m\u{202e}txet".to_string()),
            repo: "o/r\u{7f}".to_string(),
            runner: Some("faber-1\u{1b}[8m".to_string()),
            ..outcome("failure", 5)
        };
        let fault = Entry {
            result: Some("discovery".to_string()),
            action: Some("failed".to_string()),
            cause: Some(format!("http {osc52}")),
            ..entry("fault", 4)
        };
        let summary = summarize(vec![fault, failure], None, 0);
        let live: Live = serde_json::from_value(serde_json::json!({
            "paused": false,
            "draining": false,
            "uptime_s": 1,
            "healthy": false,
            "last_error": format!("{osc52}\rPassword:"),
        }))
        .unwrap();
        (summary, live)
    }

    #[test]
    fn the_report_prints_hostile_fields_as_text() {
        let (summary, live) = hostile();
        let text = report(&summary, Some(&live), "audit.jsonl", &opts(false), 10_000);
        assert!(!acts_on_terminal(&text), "{text:?}");
        for visible in [
            "deploy \\x1b]52;c;cm0gLXJmIH4=\\x07\\x1b[2K\\x0dok",
            "exit 1\\x9b31m\\u{202e}txet",
            "http \\x1b]52;c;cm0gLXJmIH4=\\x07",
            "UNHEALTHY: \\x1b]52;c;cm0gLXJmIH4=\\x07\\x0dPassword:",
            "faber-1\\x1b[8m",
        ] {
            assert!(text.contains(visible), "{visible} in {text}");
        }
        assert!(text.contains("\nfailed runs — 1"), "{text}");
    }

    #[test]
    fn a_huge_field_is_cut() {
        let (mut summary, _) = hostile();
        summary.failures[0].cause = Some("z".repeat(1 << 20));
        let text = report(&summary, None, "audit.jsonl", &opts(false), 10_000);
        assert!(text.len() < 16 * 1024, "{} bytes", text.len());
        assert!(text.contains(crate::term::TRUNCATED));
    }

    #[test]
    fn the_json_report_stays_json_and_prints_nothing_a_terminal_acts_on() {
        let (summary, _) = hostile();
        let json = json_report(&summary).unwrap();
        assert!(!acts_on_terminal(&json), "{json}");
        let back: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(
            back["failures"][0]["cause"], "exit 1\u{9b}31m\u{202e}txet",
            "the escapes decode to what the log said"
        );
    }
}
