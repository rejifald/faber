//! Every program `sudo faber setup` and `sudo faber uninstall` run, by absolute
//! path, and the seam that keeps tests from running any of them.
//!
//! These commands run as root. A bare program name is resolved through `PATH`,
//! which under sudo is whatever the operator's environment and sudoers leave
//! behind, and any directory on it that someone else can write is a way to run
//! their program as root (ADR-0025's root invariant). So nothing is looked up:
//! each tool is a [`Tool`], whose path is a constant, and the source scan in
//! this module's tests fails the build on a spawn that names anything else.
//!
//! There are no staged scripts either. The faber binary itself is the
//! privileged process, and it runs each tool directly with an argument vector,
//! so no shell ever parses a path or a name.
//!
//! [`Cmd`] is a command as data. Builders elsewhere in `setup` return `Cmd`s, so
//! a test can assert the exact command an action produces (never `-admin`,
//! always `-roleAccount`) without running it, and [`Runner`] is the one place a
//! `Cmd` becomes a process.

use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

use crate::platform::record::Account;

/// The `PATH` every child process gets: nothing an operator can write.
pub const SAFE_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// A program setup or uninstall runs. See the module doc for why each is an
/// absolute constant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tool {
    Sysadminctl,
    Dseditgroup,
    Dscl,
    Launchctl,
    Plutil,
    Pmset,
    Id,
    Useradd,
    Groupadd,
    Usermod,
    Userdel,
    Groupdel,
    /// Linux: to take an account out of a group (`gpasswd -d`), which `usermod`
    /// cannot do on every distribution setup supports.
    Gpasswd,
    Systemctl,
    Getent,
    /// macOS: only to flush the wall's anchor on uninstall. The checker
    /// itself runs `pfctl` through [`crate::wall::sys`].
    Pfctl,
    /// macOS: whether FileVault is on (ADR-0051's reboot modes).
    Fdesetup,
    /// macOS: the login window's auto-login and "Other…" settings (ADR-0051).
    Defaults,
    /// macOS: `apfs updatePreboot /` once a SecureToken changes (ADR-0051).
    Diskutil,
    /// macOS: to quit the operator's menu-bar app before setup replaces it.
    Pkill,
}

impl Tool {
    /// Every tool, for the test that pins their paths.
    pub const ALL: [Tool; 20] = [
        Tool::Sysadminctl,
        Tool::Dseditgroup,
        Tool::Dscl,
        Tool::Launchctl,
        Tool::Plutil,
        Tool::Pmset,
        Tool::Id,
        Tool::Useradd,
        Tool::Groupadd,
        Tool::Usermod,
        Tool::Userdel,
        Tool::Groupdel,
        Tool::Gpasswd,
        Tool::Systemctl,
        Tool::Getent,
        Tool::Pfctl,
        Tool::Fdesetup,
        Tool::Defaults,
        Tool::Diskutil,
        Tool::Pkill,
    ];

    /// The tool's absolute path.
    pub const fn path(self) -> &'static str {
        match self {
            Tool::Sysadminctl => "/usr/sbin/sysadminctl",
            Tool::Dseditgroup => "/usr/sbin/dseditgroup",
            Tool::Dscl => "/usr/bin/dscl",
            Tool::Launchctl => "/bin/launchctl",
            Tool::Plutil => "/usr/bin/plutil",
            Tool::Pmset => "/usr/bin/pmset",
            Tool::Id => "/usr/bin/id",
            Tool::Useradd => "/usr/sbin/useradd",
            Tool::Groupadd => "/usr/sbin/groupadd",
            Tool::Usermod => "/usr/sbin/usermod",
            Tool::Userdel => "/usr/sbin/userdel",
            Tool::Groupdel => "/usr/sbin/groupdel",
            Tool::Gpasswd => "/usr/bin/gpasswd",
            Tool::Systemctl => "/usr/bin/systemctl",
            Tool::Getent => "/usr/bin/getent",
            Tool::Pfctl => "/sbin/pfctl",
            Tool::Fdesetup => "/usr/bin/fdesetup",
            Tool::Defaults => "/usr/bin/defaults",
            Tool::Diskutil => "/usr/sbin/diskutil",
            Tool::Pkill => "/usr/bin/pkill",
        }
    }
}

/// A command as data: a [`Tool`] and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cmd {
    pub tool: Tool,
    pub args: Vec<String>,
}

impl Cmd {
    pub fn new<I, S>(tool: Tool, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            tool,
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    /// The command as a person would type it, for plans and error messages.
    pub fn display(&self) -> String {
        let mut out = self.tool.path().to_string();
        for arg in &self.args {
            out.push(' ');
            out.push_str(&quote(arg));
        }
        out
    }
}

/// `text` with every character but printable ASCII, newline and tab replaced
/// by `?`, cut to at most `max` characters: what a child setup ran as another
/// account printed, made safe to print to the operator's terminal. Stricter
/// than [`crate::term::sanitize`], which keeps printable Unicode: nothing a
/// less trusted account wrote needs more than ASCII to be read.
pub fn sanitize_child_output(text: &str, max: usize) -> String {
    let mut out: String = text
        .chars()
        .take(max)
        .map(|c| {
            if c == '\n' || c == '\t' || (' '..='~').contains(&c) {
                c
            } else {
                '?'
            }
        })
        .collect();
    if text.chars().count() > max {
        out.push_str("\n(output cut)");
    }
    out
}

/// `arg`, double-quoted if a shell would split or expand it.
pub fn quote(arg: &str) -> String {
    let plain = !arg.is_empty()
        && arg
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./:=@,+%".contains(c));
    if plain {
        arg.to_string()
    } else {
        format!("\"{}\"", arg.replace('\\', "\\\\").replace('"', "\\\""))
    }
}

/// What a finished command reported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Output {
    pub success: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    /// Both streams, safe to show a person: every character but printable
    /// ASCII, newline and tab replaced with `?` (no terminal escapes), and at
    /// most `max` characters.
    pub fn sanitized(&self, max: usize) -> String {
        sanitize_child_output(&self.text(), max)
    }

    /// Both streams, trimmed, for an error message.
    pub fn text(&self) -> String {
        let mut text = self.stdout.trim().to_string();
        let err = self.stderr.trim();
        if !err.is_empty() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(err);
        }
        text
    }
}

/// Runs [`Cmd`]s. [`SystemRunner`] spawns them; tests substitute a recorder,
/// so no test ever runs `sysadminctl`, `launchctl` or `systemctl`.
pub trait Runner {
    fn run(&mut self, cmd: &Cmd) -> Result<Output>;

    /// Run `cmd` and fail, naming the command and what it printed, unless it
    /// exits 0.
    fn run_ok(&mut self, cmd: &Cmd) -> Result<Output> {
        let out = self.run(cmd)?;
        if !out.success {
            bail!(
                "`{}` failed ({}): {}",
                cmd.display(),
                out.code
                    .map_or_else(|| "killed by a signal".to_string(), |c| format!("exit {c}")),
                out.text()
            );
        }
        Ok(out)
    }

    /// Run `cmd` on the operator's own terminal: the tool prompts for and
    /// reads passwords itself (`sysadminctl -password -`, `dscl -passwd`), so
    /// faber never sees them. Only the exit status comes back. The test runner
    /// records it like [`Runner::run`].
    fn run_interactive(&mut self, cmd: &Cmd) -> Result<Output> {
        self.run(cmd)
    }

    /// Run `cmd` with `input`, and nothing else, on its stdin: a generated
    /// password goes to `dscl`'s command loop this way, never into an
    /// argument list `ps` shows. The test runner records the command, not the
    /// input.
    fn run_with_input(&mut self, cmd: &Cmd, input: &[u8]) -> Result<Output> {
        let _ = input;
        self.run(cmd)
    }

    /// Run the faber binary `bin` with `args` as `account` ([`run_as`]): a
    /// method of the runner so a test host records the spawn instead of
    /// making it.
    #[cfg(unix)]
    fn run_faber_as(
        &mut self,
        bin: &Path,
        args: &[&std::ffi::OsStr],
        account: Account,
        home: &Path,
        limit: Duration,
    ) -> Result<Output> {
        run_as(bin, args, None, Some(account), home, limit)
    }
}

/// The production [`Runner`]: spawns the tool with a cleared environment and
/// [`SAFE_PATH`], no stdin, and both output streams captured.
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&mut self, cmd: &Cmd) -> Result<Output> {
        let out = Command::new(cmd.tool.path())
            .args(&cmd.args)
            .env_clear()
            .env("PATH", SAFE_PATH)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("running {}", cmd.tool.path()))?;
        Ok(Output {
            success: out.status.success(),
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }

    fn run_interactive(&mut self, cmd: &Cmd) -> Result<Output> {
        let status = Command::new(cmd.tool.path())
            .args(&cmd.args)
            .env_clear()
            .env("PATH", SAFE_PATH)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .with_context(|| format!("running {}", cmd.tool.path()))?;
        Ok(Output {
            success: status.success(),
            code: status.code(),
            ..Output::default()
        })
    }

    fn run_with_input(&mut self, cmd: &Cmd, input: &[u8]) -> Result<Output> {
        let mut child = Command::new(cmd.tool.path())
            .args(&cmd.args)
            .env_clear()
            .env("PATH", SAFE_PATH)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("running {}", cmd.tool.path()))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input)?;
        }
        let out = child.wait_with_output()?;
        Ok(Output {
            success: out.status.success(),
            code: out.status.code(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

#[cfg(unix)]
extern "C" {
    /// POSIX. Async-signal-safe, which is what a `pre_exec` hook may call.
    fn setsid() -> std::os::raw::c_int;
    fn geteuid() -> u32;
    fn setgid(gid: u32) -> std::os::raw::c_int;
    fn setuid(uid: u32) -> std::os::raw::c_int;
}

// `setgroups`' count is an `int` on macOS and a `size_t` on Linux (glibc, musl).
#[cfg(target_os = "macos")]
extern "C" {
    fn setgroups(n: std::os::raw::c_int, groups: *const u32) -> std::os::raw::c_int;
}
#[cfg(all(unix, not(target_os = "macos")))]
extern "C" {
    fn setgroups(n: usize, groups: *const u32) -> std::os::raw::c_int;
}

/// Run `cmd`'s child as `account` and nobody else: its user id, and its group as both the
/// effective group and the only supplementary one, dropped by hand in the child.
///
/// Not std's `uid` and `gid`: from root, std calls `setgroups(0, NULL)` after `setgid`, and
/// on macOS that leaves the group list `{0}`, whose first entry is the effective group. The
/// child then runs as `wheel`, not as `account`'s group: it reads what `wheel` reads, and
/// cannot enter a directory shared with it through its own group (a golden build's
/// `root:_faber_vm 0750` staging). Without an effective root only the ids change, as std does.
#[cfg(unix)]
pub fn drop_to(cmd: &mut Command, account: Account) {
    use std::os::unix::process::CommandExt;
    let (uid, gid) = (account.uid, account.gid);
    // SAFETY: the hook runs in the forked child before exec, where only
    // async-signal-safe calls are allowed; these four are, and nothing allocates.
    unsafe {
        cmd.pre_exec(move || {
            if geteuid() == 0 && setgroups(1, &gid) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if setgid(gid) != 0 || setuid(uid) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
}

/// Run a faber binary as another account, and collect what it printed.
///
/// The one spawn whose program is not a [`Tool`], used for `check-config`
/// only: the root-owned CLI copy checking a migrated config as `_faber`, and
/// this binary checking the source config as the operator before anything is
/// drained. Setup never executes a file a role account can write (the daemon
/// copy's version comes from its bytes, [`super::marker`]), and whatever it
/// runs is shut out of the operator's sudo session:
///
/// - uid, gid and supplementary groups dropped to `account` ([`drop_to`]: the
///   account's group is the only one); `None` runs as this process's own account,
///   for a preview without sudo. A cleared environment, `/` as its directory
/// - `setsid()` before exec: a session of its own, so no controlling
///   terminal to type into (`TIOCSTI`) or read the operator's answers from
/// - stdin from `/dev/null`, or a pipe that carries `input` and nothing else,
///   which the child reads through the descriptor it inherits (`check-config
///   --config -`), never by reopening it by name; stdout and stderr piped,
///   never the terminal, each read to at most 1 MiB, and the whole run
///   bounded by `limit`
///
/// Print what it returns only through [`Output::sanitized`].
#[cfg(unix)]
pub fn run_as(
    bin: &Path,
    args: &[&std::ffi::OsStr],
    input: Option<&[u8]>,
    account: Option<Account>,
    home: &Path,
    limit: Duration,
) -> Result<Output> {
    run_as_until(bin, args, input, account, home, limit, None)
}

/// [`run_as`], also killed as soon as `stop` is set: for the Linux image's
/// build VM, which setup kills the moment the wall stops covering it (#243).
/// A child killed this way is not an error: its [`Output`] has no exit code.
#[cfg(unix)]
pub fn run_as_until(
    bin: &Path,
    args: &[&std::ffi::OsStr],
    input: Option<&[u8]>,
    account: Option<Account>,
    home: &Path,
    limit: Duration,
    stop: Option<&std::sync::atomic::AtomicBool>,
) -> Result<Output> {
    use std::os::unix::process::CommandExt;

    if !bin.is_absolute() {
        bail!("refusing to run {}: not an absolute path", bin.display());
    }
    let mut cmd = Command::new(bin); // absolute: checked above
    if let Some(account) = account {
        drop_to(&mut cmd, account);
    }
    cmd.args(args)
        .env_clear()
        .env("HOME", home)
        .env("PATH", SAFE_PATH)
        .current_dir("/")
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        });
    // SAFETY: the hook runs in the forked child before exec, where only
    // async-signal-safe calls are allowed: `setsid` is one, and building an
    // `io::Error` from errno allocates nothing.
    unsafe {
        cmd.pre_exec(|| {
            if setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    fed_within(cmd, input.map(<[u8]>::to_vec), limit, stop)
}

/// Run `cmd` to completion, killing it if it outlives `limit`. For children
/// whose honesty is not assumed: a planted binary must not hang setup.
pub fn output_within(cmd: Command, limit: Duration) -> Result<Output> {
    fed_within(cmd, None, limit, None)
}

/// [`output_within`], with `input` written to the pipe on the child's stdin
/// and the pipe closed after it, so the child reads to end of file.
///
/// A child that exits 0 without having taken all of `input` is an error,
/// deliberately: a check that passed on part of a config proves nothing about
/// the rest. It is not a race for a child that reads to end of file, as
/// `check-config --config -` does: end of file comes only once the last byte
/// is written and the pipe closed, so such a child cannot exit 0 before the
/// write has finished. Only a child that stops reading early can trip it.
fn fed_within(
    mut cmd: Command,
    input: Option<Vec<u8>>,
    limit: Duration,
    stop: Option<&std::sync::atomic::AtomicBool>,
) -> Result<Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    // The binary may have been written a moment ago: see `retry_text_file_busy`.
    let mut child =
        crate::update::retry_text_file_busy_blocking(|| cmd.spawn()).context("spawning")?;
    let writer = match (input, child.stdin.take()) {
        (None, _) => None,
        // The pipe is dropped, and closed, when the write ends.
        (Some(bytes), Some(mut pipe)) => Some(std::thread::spawn(move || pipe.write_all(&bytes))),
        (Some(_), None) => {
            let _ = child.kill();
            let _ = child.wait();
            bail!("its stdin is not a pipe to write the input to");
        }
    };
    let mut stdout = child.stdout.take();
    let mut stderr = child.stderr.take();
    let out_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(s) = stdout.as_mut() {
            let _ = s.take(1 << 20).read_to_end(&mut buf);
        }
        buf
    });
    let err_reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(s) = stderr.as_mut() {
            let _ = s.take(1 << 20).read_to_end(&mut buf);
        }
        buf
    });
    let deadline = Instant::now() + limit;
    let status = loop {
        if let Some(status) = child.try_wait().context("waiting")? {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("did not finish within {limit:?}; killed");
        }
        if stop.is_some_and(|s| s.load(std::sync::atomic::Ordering::SeqCst)) {
            let _ = child.kill();
            break child.wait().context("waiting for the killed child")?;
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = out_reader.join().unwrap_or_default();
    let stderr = err_reader.join().unwrap_or_default();
    if let Some(writer) = writer {
        let fed = writer
            .join()
            .unwrap_or_else(|_| Err(std::io::Error::other("the writer panicked")));
        if let Err(e) = fed {
            if status.success() {
                bail!("exited 0 without reading all of its input: {e}");
            }
        }
    }
    Ok(Output {
        success: status.success(),
        code: status.code(),
        stdout: String::from_utf8_lossy(&stdout).into_owned(),
        stderr: String::from_utf8_lossy(&stderr).into_owned(),
    })
}

/// A [`Runner`] that records every command and answers from a script, for
/// tests of the code that drives the tools.
#[cfg(test)]
#[derive(Default)]
pub struct FakeRunner {
    pub ran: Vec<Cmd>,
    /// Each [`Runner::run_faber_as`]: the binary, its arguments and the uid
    /// it was to run as.
    pub ran_as: Vec<(std::path::PathBuf, Vec<String>, u32)>,
    /// What [`Runner::run_faber_as`] answers (success when `None`).
    pub ran_as_answer: Option<Output>,
    /// Answers by exact [`Cmd::display`] text; anything else succeeds with no
    /// output. A command listed more than once takes its answers in order,
    /// and the last one sticks.
    pub answers: Vec<(String, Output)>,
}

#[cfg(test)]
impl Runner for FakeRunner {
    fn run_faber_as(
        &mut self,
        bin: &Path,
        args: &[&std::ffi::OsStr],
        account: Account,
        _home: &Path,
        _limit: Duration,
    ) -> Result<Output> {
        self.ran_as.push((
            bin.to_path_buf(),
            args.iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect(),
            account.uid,
        ));
        Ok(self.ran_as_answer.clone().unwrap_or(Output {
            success: true,
            code: Some(0),
            ..Output::default()
        }))
    }

    fn run(&mut self, cmd: &Cmd) -> Result<Output> {
        self.ran.push(cmd.clone());
        let shown = cmd.display();
        let Some(i) = self.answers.iter().position(|(k, _)| *k == shown) else {
            return Ok(Output {
                success: true,
                code: Some(0),
                ..Output::default()
            });
        };
        let out = self.answers[i].1.clone();
        if self.answers[i + 1..].iter().any(|(k, _)| *k == shown) {
            self.answers.remove(i);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    #[test]
    fn every_tool_is_an_absolute_path() {
        for tool in Tool::ALL {
            assert!(tool.path().starts_with('/'), "{tool:?}");
            assert!(!tool.path().contains(".."), "{tool:?}");
        }
    }

    /// The paths ADR-0025 §3 names, pinned: a change here changes what root
    /// executes.
    #[test]
    fn tool_paths_are_the_documented_ones() {
        assert_eq!(Tool::Sysadminctl.path(), "/usr/sbin/sysadminctl");
        assert_eq!(Tool::Dseditgroup.path(), "/usr/sbin/dseditgroup");
        assert_eq!(Tool::Dscl.path(), "/usr/bin/dscl");
        assert_eq!(Tool::Launchctl.path(), "/bin/launchctl");
        assert_eq!(Tool::Plutil.path(), "/usr/bin/plutil");
        assert_eq!(Tool::Pmset.path(), "/usr/bin/pmset");
        assert_eq!(Tool::Id.path(), "/usr/bin/id");
        assert_eq!(Tool::Useradd.path(), "/usr/sbin/useradd");
        assert_eq!(Tool::Groupadd.path(), "/usr/sbin/groupadd");
        assert_eq!(Tool::Usermod.path(), "/usr/sbin/usermod");
        assert_eq!(Tool::Userdel.path(), "/usr/sbin/userdel");
        assert_eq!(Tool::Groupdel.path(), "/usr/sbin/groupdel");
        assert_eq!(Tool::Gpasswd.path(), "/usr/bin/gpasswd");
        assert_eq!(Tool::Systemctl.path(), "/usr/bin/systemctl");
        assert_eq!(Tool::Getent.path(), "/usr/bin/getent");
        assert_eq!(Tool::Pfctl.path(), "/sbin/pfctl");
        assert_eq!(Tool::Fdesetup.path(), "/usr/bin/fdesetup");
        assert_eq!(Tool::Defaults.path(), "/usr/bin/defaults");
        assert_eq!(Tool::Diskutil.path(), "/usr/sbin/diskutil");
        assert_eq!(Tool::Pkill.path(), "/usr/bin/pkill");
    }

    #[test]
    fn display_quotes_only_what_a_shell_would_split() {
        let cmd = Cmd::new(
            Tool::Sysadminctl,
            [
                "-addUser",
                "_faber",
                "-home",
                "/Library/Application Support/faber",
            ],
        );
        assert_eq!(
            cmd.display(),
            "/usr/sbin/sysadminctl -addUser _faber -home \"/Library/Application Support/faber\""
        );
        assert_eq!(quote(""), "\"\"");
        assert_eq!(quote("a\"b"), "\"a\\\"b\"");
    }

    /// Every file whose spawns run as root: the setup module and the service
    /// manager seam.
    fn scanned_sources() -> Vec<(String, String)> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut files = vec![root.join("src/platform/service.rs")];
        let mut setup: Vec<PathBuf> = std::fs::read_dir(root.join("src/setup"))
            .expect("src/setup exists")
            .map(|e| e.expect("readable entry").path())
            .filter(|p| p.extension().is_some_and(|x| x == "rs"))
            .collect();
        setup.sort();
        files.extend(setup);
        files
            .into_iter()
            .map(|p| {
                let text = std::fs::read_to_string(&p).expect("readable source");
                (p.display().to_string(), text)
            })
            .collect()
    }

    /// Every process constructor in `src/setup/` and `src/platform/service.rs`
    /// names an absolute program: a string literal starting with `/`, a
    /// [`Tool`]'s path, or the one runtime-checked faber spawn in
    /// [`faber_as`]. A bare name would be resolved through root's `PATH`.
    /// (The needle is assembled at run time so this comment and the test's own
    /// source never match it.)
    #[test]
    fn every_spawn_names_an_absolute_program() {
        let needle = ["Command", "::new("].concat();
        let mut sites = BTreeMap::new();
        let mut problems = Vec::new();
        for (file, text) in scanned_sources() {
            for (n, line) in text.lines().enumerate() {
                let mut rest = line;
                while let Some(at) = rest.find(&needle) {
                    let after = &rest[at + needle.len()..];
                    let arg = after.split(')').next().unwrap_or("").trim();
                    let ok = (arg.starts_with("\"/") && !arg.starts_with("\"//"))
                        || arg.ends_with(".tool.path(")
                        || arg.ends_with("tool.path(")
                        || (arg == "bin" && line.contains("// absolute: checked above"));
                    if !ok {
                        problems.push(format!("{file}:{}: `{}`", n + 1, line.trim()));
                    }
                    *sites.entry(file.clone()).or_insert(0) += 1;
                    rest = after;
                }
            }
        }
        assert!(
            problems.is_empty(),
            "spawns that do not name an absolute program:\n  {}",
            problems.join("\n  ")
        );
        // The scan must see the spawns it exists to police, or a refactor
        // that renamed the constructor would pass it vacuously.
        assert!(
            sites.values().sum::<usize>() >= 2,
            "the scan found too few spawns to be looking in the right place: {sites:?}"
        );
    }

    #[test]
    fn run_ok_names_the_command_and_its_output_on_failure() {
        struct Failing;
        impl Runner for Failing {
            fn run(&mut self, _cmd: &Cmd) -> Result<Output> {
                Ok(Output {
                    success: false,
                    code: Some(56),
                    stdout: String::new(),
                    stderr: "DS Error: -14136 (eDSRecordNotFound)\n".into(),
                })
            }
        }
        let cmd = Cmd::new(Tool::Dscl, [".", "-read", "/Groups/_faber"]);
        let err = Failing.run_ok(&cmd).unwrap_err().to_string();
        assert!(
            err.contains("/usr/bin/dscl . -read /Groups/_faber"),
            "{err}"
        );
        assert!(err.contains("exit 56"), "{err}");
        assert!(err.contains("eDSRecordNotFound"), "{err}");
    }

    #[test]
    fn output_is_sanitized_before_it_is_shown() {
        assert_eq!(sanitize_child_output("ok\n\tdone", 100), "ok\n\tdone");
        assert_eq!(
            sanitize_child_output("\u{1b}[2Jgone\u{7}", 100),
            "?[2Jgone?"
        );
        assert_eq!(
            sanitize_child_output("r\u{e9}sum\u{e9} \u{202e}", 100),
            "r?sum? ?"
        );
        assert_eq!(sanitize_child_output("abcdef", 3), "abc\n(output cut)");
    }

    /// A process run as another account leads a session of its own: no
    /// controlling terminal, so it cannot type into the operator's (`TIOCSTI`).
    /// Checked through its process group, which `setsid` makes its own. Runs
    /// as this test's own account, so no privilege is needed.
    #[test]
    fn run_as_starts_a_new_session_with_no_terminal() {
        use std::os::unix::fs::MetadataExt;
        let probe = std::env::temp_dir().join(format!("fbr-runas-{}", std::process::id()));
        std::fs::write(&probe, b"").unwrap();
        let meta = std::fs::metadata(&probe).unwrap();
        let _ = std::fs::remove_file(&probe);
        let out = run_as(
            Path::new("/bin/sh"),
            &[
                std::ffi::OsStr::new("-c"),
                std::ffi::OsStr::new("echo $$ $(ps -o pgid= -p $$); exec 3</dev/tty && echo tty"),
            ],
            None,
            Some(Account {
                uid: meta.uid(),
                gid: meta.gid(),
            }),
            Path::new("/"),
            Duration::from_secs(20),
        )
        .unwrap();
        let words: Vec<&str> = out.stdout.split_whitespace().collect();
        assert!(words.len() >= 2, "{out:?}");
        assert_eq!(words[0], words[1], "its own process group: {out:?}");
        assert!(
            !out.stdout.contains("tty"),
            "no controlling terminal: {out:?}"
        );
    }

    /// A child run as an account has that account's group as its effective group, and
    /// under root as its only group: never root's `wheel`, which std's own drop leaves on
    /// macOS. Without root it can check only the effective group; with `sudo cargo test`
    /// it checks the root path too.
    #[test]
    fn run_as_gives_the_child_the_accounts_group() {
        use std::os::unix::fs::MetadataExt;
        let probe = std::env::temp_dir().join(format!("fbr-runas-gid-{}", std::process::id()));
        std::fs::write(&probe, b"").unwrap();
        let meta = std::fs::metadata(&probe).unwrap();
        let _ = std::fs::remove_file(&probe);
        let root = unsafe { geteuid() } == 0;
        // Under root, a group root is not in: nobody's, so a leftover `wheel` shows.
        let account = if root {
            Account {
                uid: 4242,
                gid: 4242,
            }
        } else {
            Account {
                uid: meta.uid(),
                gid: meta.gid(),
            }
        };
        let out = run_as(
            Path::new("/bin/sh"),
            &[
                std::ffi::OsStr::new("-c"),
                std::ffi::OsStr::new("echo $(/usr/bin/id -u) $(/usr/bin/id -g) $(/usr/bin/id -G)"),
            ],
            None,
            Some(account),
            Path::new("/"),
            Duration::from_secs(20),
        )
        .unwrap();
        let words: Vec<u32> = out
            .stdout
            .split_whitespace()
            .map(|w| w.parse().unwrap())
            .collect();
        assert_eq!(words[0], account.uid, "{out:?}");
        assert_eq!(words[1], account.gid, "effective group: {out:?}");
        if root {
            assert_eq!(&words[2..], &[account.gid], "only group: {out:?}");
        }
    }

    /// Without sudo the source check runs as this process's own account:
    /// no uid switch, but still its own session and piped output.
    #[test]
    fn run_as_without_an_account_still_isolates_the_child() {
        let out = run_as(
            Path::new("/bin/sh"),
            &[
                std::ffi::OsStr::new("-c"),
                std::ffi::OsStr::new("echo $$ $(ps -o pgid= -p $$)"),
            ],
            None,
            None,
            Path::new("/"),
            Duration::from_secs(20),
        )
        .unwrap();
        let words: Vec<&str> = out.stdout.split_whitespace().collect();
        assert!(words.len() >= 2, "{out:?}");
        assert_eq!(words[0], words[1], "its own process group: {out:?}");
    }

    /// The source check hands the config over on stdin, not through a file:
    /// the child reads all of it through the descriptor it inherits, as
    /// `check-config --config -` does, and more than a pipe holds at once.
    /// Run with an account (this test's own) as setup runs it.
    #[test]
    fn run_as_feeds_its_input_through_stdin() {
        use std::os::unix::fs::MetadataExt;
        let probe = std::env::temp_dir().join(format!("fbr-runas-in-{}", std::process::id()));
        std::fs::write(&probe, b"").unwrap();
        let meta = std::fs::metadata(&probe).unwrap();
        let _ = std::fs::remove_file(&probe);
        let input = vec![b'x'; 300 * 1024];
        let out = run_as(
            Path::new("/bin/sh"),
            &[std::ffi::OsStr::new("-c"), std::ffi::OsStr::new("wc -c")],
            Some(&input),
            Some(Account {
                uid: meta.uid(),
                gid: meta.gid(),
            }),
            Path::new("/"),
            Duration::from_secs(20),
        )
        .unwrap();
        assert!(out.success, "{out:?}");
        assert_eq!(out.stdout.trim(), (300 * 1024).to_string(), "{out:?}");
    }

    /// A child that reports success without having read what it was given
    /// checked less than it was asked to: an error, not a pass. Deterministic:
    /// 1 MiB is more than any pipe holds, so the write cannot finish once the
    /// child, the only reader, has exited.
    #[test]
    fn a_child_that_exits_0_without_its_input_is_an_error() {
        let input = vec![b'x'; 1 << 20];
        let err = run_as(
            Path::new("/bin/sh"),
            &[std::ffi::OsStr::new("-c"), std::ffi::OsStr::new("exit 0")],
            Some(&input),
            None,
            Path::new("/"),
            Duration::from_secs(20),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("without reading all of its input"),
            "{err}"
        );
    }

    #[test]
    fn a_child_that_outlives_its_limit_is_killed() {
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("5");
        let started = Instant::now();
        let err = output_within(cmd, Duration::from_millis(200)).unwrap_err();
        assert!(err.to_string().contains("killed"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(4));
    }

    /// #243: setup kills the build VM's launcher the moment it sets the stop
    /// flag, well before the limit, and reports it as an exit with no code.
    #[test]
    fn a_child_is_killed_when_stop_is_set() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;
        let stop = Arc::new(AtomicBool::new(false));
        let setter = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(200));
                stop.store(true, Ordering::SeqCst);
            })
        };
        let started = Instant::now();
        let out = run_as_until(
            Path::new("/bin/sleep"),
            &[std::ffi::OsStr::new("5")],
            None,
            None,
            Path::new("/"),
            Duration::from_secs(30),
            Some(&stop),
        )
        .unwrap();
        setter.join().unwrap();
        assert!(!out.success);
        assert_eq!(out.code, None, "killed by a signal");
        assert!(started.elapsed() < Duration::from_secs(4));
    }
}
