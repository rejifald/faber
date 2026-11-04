//! The host lock — exactly one faber may provision on a machine (ADR-0011 §4).
//!
//! Before adoption there was no lock and none was needed: a second `faber run`
//! failed to bind the dashboard port, that error was only logged, and the second
//! reconcile loop ran on regardless. The damage was double-*minting* — untidy,
//! and self-healing under [ADR-0004](../docs/adrs/0004-demand-based-provisioning.md),
//! since GitHub stays the single job broker.
//!
//! Adoption changes the stakes. Two processes rebuilding claims from the same
//! containers is double-*watching*: two teardowns of one sandbox, two outcome
//! records for one job, and one reservation released twice. So the invariant
//! stops being a happy accident of `launchctl` and becomes something faber
//! enforces.
//!
//! It is deliberately *not* the dashboard port. The right to mint runners and the
//! ability to serve a UI are unrelated concerns, and a `--port` override must not
//! be able to hand a second process provisioning authority.
//!
//! # Why an advisory file lock rather than a pid file
//!
//! The lock is held by an open file descriptor, so the kernel releases it when
//! the process ends — however it ends. There is no stale-lock class to detect,
//! no pid to validate against a recycled one, and nothing to clean up after a
//! crash. A pid file would have to answer "is 4821 still faber, or is it now
//! somebody else's process?", and every answer to that is a guess.
//!
//! `flock(2)` is declared here rather than pulled in with a `libc` dependency:
//! it is two constants and one call, and faber is a self-contained single binary
//! by design. It is also the portable choice — macOS's `O_EXLOCK` open flag
//! would do the same thing in one syscall, but those flag bits mean nothing on
//! Linux, where the open would silently succeed **unlocked**. faber ships for
//! macOS and runs its tests on Linux, so a macOS-only mechanism is one whose
//! test can only ever pass by accident.

use std::fs::{File, OpenOptions};
use std::os::raw::c_int;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

extern "C" {
    /// POSIX advisory whole-file lock. Present with these semantics on both
    /// macOS and Linux.
    fn flock(fd: c_int, operation: c_int) -> c_int;
}

/// `LOCK_EX` — an exclusive lock.
const LOCK_EX: c_int = 2;
/// `LOCK_NB` — fail with `EWOULDBLOCK` rather than waiting for the holder.
///
/// Waiting would be worse than failing: `launchd` restarts faber on exit, so a
/// blocked second process would sit invisibly holding a startup it never
/// completes, instead of saying plainly that another faber owns this host.
const LOCK_NB: c_int = 4;

/// `EWOULDBLOCK`/`EAGAIN` — 35 on macOS, 11 on Linux. The one error that means
/// "someone else holds it" rather than "something went wrong".
const EWOULDBLOCK: &[i32] = &[35, 11];

/// The lock file's mode: the daemon's alone. `flock` needs only a descriptor,
/// and a read-only one will do, so anyone who could open the file could take
/// the lock and keep the daemon from starting. Under SYSTEM the state
/// directory is the operator group's to enter, so a group-readable lock would
/// hand that to every process the operator runs.
const LOCK_MODE: u32 = 0o600;

/// The held lock. Dropping it (or exiting) releases it; nothing else does.
///
/// The `File` is never read or written — its existence as an open descriptor
/// *is* the lock — so it is deliberately unused rather than accidentally so.
#[derive(Debug)]
pub struct HostLock {
    _file: File,
    path: PathBuf,
}

impl HostLock {
    /// The lock file's path, for logging.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// `~/Library/Application Support/faber/faber.lock` — beside the config and the
/// update directory, the other things exactly one faber owns.
pub fn default_path() -> Option<PathBuf> {
    crate::platform::lock_path()
}

/// Take the host lock, or explain who has it.
///
/// The error is the whole point of the function, so it names the file rather
/// than reporting a bare `EWOULDBLOCK`: an operator who ran `faber run` next to
/// the LaunchAgent needs to be told that is what happened, not that a syscall
/// returned -1.
///
/// The file is created at [`LOCK_MODE`] and set to it through its descriptor
/// whatever it was, so one left more open by an older release is closed too.
pub fn acquire(path: &Path) -> anyhow::Result<HostLock> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .mode(LOCK_MODE)
        .open(path)
        .map_err(|e| anyhow::anyhow!("opening {}: {e}", path.display()))?;
    // Only a file this account does not own refuses: said, not fatal, since
    // a daemon that will not start is the worse outcome (AGENTS.md).
    if let Err(e) = file.set_permissions(std::fs::Permissions::from_mode(LOCK_MODE)) {
        tracing::warn!(
            "host lock {}: could not set it to mode 600 ({e}); whoever can open it can keep \
             faber from starting",
            path.display()
        );
    }

    // SAFETY: `fd` is the descriptor of a `File` this function owns and keeps
    // alive past the call, and `flock` neither retains it nor writes through it.
    let rc = unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error().is_some_and(|e| EWOULDBLOCK.contains(&e)) {
            anyhow::bail!(
                "another faber already holds {} — only one may provision on a host \
                 (ADR-0011). Stop the running one first, or use `faber status` to \
                 talk to it.",
                path.display()
            );
        }
        return Err(anyhow::anyhow!("locking {}: {err}", path.display()));
    }
    Ok(HostLock {
        _file: file,
        path: path.to_path_buf(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("faber-lock-{name}-{}", std::process::id()))
    }

    /// Acquire, tolerating a *transient* refusal from a concurrent fork in this
    /// same process — see [`a_second_acquire_is_refused_while_the_first_is_held`]
    /// for the mechanism. The window it waits out is microseconds wide; the
    /// budget is generous only so a loaded CI runner cannot turn it into a
    /// failure.
    fn acquire_within(path: &Path, budget: Duration) -> Result<HostLock, String> {
        let deadline = std::time::Instant::now() + budget;
        loop {
            match acquire(path) {
                Ok(lock) => return Ok(lock),
                Err(e) if std::time::Instant::now() >= deadline => {
                    return Err(format!(
                        "lock never became acquirable within {budget:?}: {e}"
                    ))
                }
                Err(_) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
    }

    fn acquire_soon(path: &Path) -> HostLock {
        acquire_within(path, Duration::from_secs(5)).expect("acquire after release")
    }

    #[test]
    fn a_second_acquire_is_refused_while_the_first_is_held() {
        let path = tmp("held");
        let _ = std::fs::remove_file(&path);
        let first = acquire(&path).expect("first acquire");
        let second = acquire(&path);
        assert!(second.is_err(), "a second holder must be refused");
        assert!(second
            .unwrap_err()
            .to_string()
            .contains("only one may provision"));
        drop(first);
        // Released by the close: the next process gets it with no cleanup step,
        // which is the property that makes a crash indistinguishable from an exit.
        //
        // Bounded rather than immediate, and the reason is worth writing down
        // because it looks like a papered-over flake and is not (issue #47). An
        // `flock` belongs to the open file *description*, which `fork` shares
        // rather than copies, so a child holds the lock until the last descriptor
        // referring to that description closes. `std::process::Command` forks
        // before it execs, and `O_CLOEXEC` only closes the child's copy *at*
        // `exec` — so for the width of that window, any unrelated test in this
        // binary that spawns a process is holding this lock, and the parent's
        // `drop` above does not release it.
        //
        // Measured: with `platform::service`'s two `id -u` spawns as co-runners,
        // an immediate re-acquire failed 25/30 runs; the two pure tests in the
        // same module never provoked it. Verified independently by forking and
        // having the child linger, which blocks the re-acquire deterministically.
        //
        // Production is unaffected — faber acquires this once at startup and
        // holds it for the process lifetime, and nothing re-acquires while a
        // holder is alive. A future "reload without restarting" that re-acquired
        // would meet exactly this, which is why the mechanism is recorded here
        // rather than just worked around.
        drop(acquire_soon(&path));
        let _ = std::fs::remove_file(&path);
    }

    /// The retry above must not be able to hide a real regression: if releasing
    /// ever stops working, the bound has to expire and report, not spin. Held
    /// deliberately for the whole budget, so this asserts the give-up path
    /// itself rather than trusting that it exists.
    #[test]
    fn the_bounded_retry_gives_up_on_a_holder_that_never_lets_go() {
        let path = tmp("stuck");
        let _ = std::fs::remove_file(&path);
        let _held = acquire(&path).expect("first acquire");
        let err = acquire_within(&path, Duration::from_millis(50))
            .expect_err("a live holder must not be waited out");
        assert!(err.contains("never became acquirable"), "{err}");
        assert!(err.contains("only one may provision"), "{err}");
        drop(_held);
        let _ = std::fs::remove_file(&path);
    }

    fn mode_of(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// Nobody but the daemon may open the lock, read-only included: a lock
    /// file made new is 0600, and so is one an older release left 0644.
    #[test]
    fn the_lock_file_is_the_daemons_alone() {
        let path = tmp("mode");
        let _ = std::fs::remove_file(&path);
        drop(acquire_soon(&path));
        assert_eq!(mode_of(&path), 0o600, "created");

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let held = acquire_soon(&path);
        assert_eq!(mode_of(&path), 0o600, "existing");
        drop(held);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_lock_file_survives_being_taken_twice_in_sequence() {
        let path = tmp("seq");
        let _ = std::fs::remove_file(&path);
        // Same close-then-reacquire shape as above, so the same bounded wait.
        for _ in 0..3 {
            let l = acquire_soon(&path);
            assert_eq!(l.path(), path);
        }
        let _ = std::fs::remove_file(&path);
    }
}
