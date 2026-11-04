//! What the two walled build VMs share: the Linux image's (ADR-0026) and the golden
//! macOS image's (ADR-0049). Each boots a bundle as `_faber_vm` on the build network,
//! slot 15, while setup watches the wall around it and kills the VM if the wall stops
//! covering the network. Nothing here is specific to either guest.
//!
//! [`watch_launch`] is the watcher and is plain code. The macOS-only [`mac`] module holds
//! the root-side file handling both builds use to put files in `_faber_vm`'s home without
//! ever giving it a name root then reopens: staged in root's own directory, handed over
//! through a descriptor, and moved in with an exclusive rename.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use super::linux_image::{build_network, Binding, BuildError};
use crate::wall::model::VmNetwork;

/// The timings of one watched launch.
#[derive(Debug, Clone, Copy)]
pub struct WatchParams {
    /// The launcher is killed after this long.
    pub launch_timeout: Duration,
    /// How long after the launcher starts the wall may still list the build
    /// network as pending before the VM is killed (#243).
    pub bind_deadline: Duration,
    /// How often the checker's verdict is read while the VM runs.
    pub wall_poll: Duration,
}

/// Run the build VM while watching the wall around it (#243). The guest's own
/// gate cannot tell an anchor that drops its probe from a host that never
/// answers one, so setup proves the binding from the host: every
/// `wall_poll` it reads the checker's verdict (`covers`), and kills the launcher if
/// the wall stops covering the build network at any point, if the network is
/// still pending `bind_deadline` after the launcher started, or if it
/// goes back to pending once bound. A launcher that exits before setup ever
/// saw the network bound fails the build too. `launch` starts the launcher
/// with its timeout and the flag that kills it. Returns the launcher's exit
/// code.
pub async fn watch_launch<C, CF, L, LF>(
    params: &WatchParams,
    covers: C,
    launch: L,
) -> Result<Option<i32>, BuildError>
where
    C: Fn(VmNetwork) -> CF,
    CF: Future<Output = Result<Binding, BuildError>>,
    L: FnOnce(Duration, Arc<AtomicBool>) -> LF,
    LF: Future<Output = Result<Option<i32>, BuildError>>,
{
    watch_launch_until(params, covers, launch, terminate_signal()).await
}

/// SIG_DFL and SIG_IGN, as `signal(2)` takes and returns them.
#[cfg(test)]
const SIG_DFL: usize = 0;
pub(crate) const SIG_IGN: usize = 1;
const SIGHUP: i32 = 1;
pub(crate) const SIGINT: i32 = 2;
const SIGTERM: i32 = 15;

/// Set `sig`'s disposition to `to` and return the one it had.
pub(crate) fn swap_disposition(sig: i32, to: usize) -> usize {
    extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }
    // SAFETY: `signal` with SIG_DFL, SIG_IGN or an address `signal` itself returned, for a
    // valid signal number, touches no memory of ours.
    unsafe { signal(sig, to) }
}

/// Resolves when this process is asked to stop: SIGINT, SIGTERM or SIGHUP.
///
/// The handlers are installed when this is *called*, not when it is first polled, so a
/// caller that makes it before it starts the launcher has no window in which a signal
/// kills setup and leaves the VM running. A signal that was ignored on entry (a `nohup`
/// run ignores SIGHUP) stays ignored, and is not waited for. Dropping this puts each
/// disposition it changed back exactly as it was found, so the steps after the watch (the
/// Linux setup's, the import) behave as they did.
///
/// One caveat of the runtime's signal registry: it installs its own handler for a signal
/// once per process, so a second call in the same process finds a signal back at the
/// disposition it had and is not caught. Setup builds one image per process.
pub fn terminate_signal() -> TerminateSignal {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigs = Vec::new();
    for (num, kind) in [
        (SIGINT, SignalKind::interrupt()),
        (SIGTERM, SignalKind::terminate()),
        (SIGHUP, SignalKind::hangup()),
    ] {
        // Read the disposition by swapping in SIG_IGN, then put it back at once.
        let before = swap_disposition(num, SIG_IGN);
        swap_disposition(num, before);
        if before == SIG_IGN {
            continue;
        }
        if let Ok(s) = signal(kind) {
            sigs.push((s, num, before));
        }
    }
    TerminateSignal { sigs }
}

/// See [`terminate_signal`].
pub struct TerminateSignal {
    /// Each signal waited for, with its number and the disposition it had before.
    sigs: Vec<(tokio::signal::unix::Signal, i32, usize)>,
}

impl Future for TerminateSignal {
    type Output = ();

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        for (sig, _, _) in self.sigs.iter_mut() {
            if sig.poll_recv(cx).is_ready() {
                return std::task::Poll::Ready(());
            }
        }
        std::task::Poll::Pending
    }
}

impl Drop for TerminateSignal {
    fn drop(&mut self) {
        for (_, num, before) in &self.sigs {
            swap_disposition(*num, *before);
        }
    }
}

/// [`watch_launch`], with `interrupt` standing for the signals. When it resolves the VM is
/// stopped like a lost wall, the launcher is waited for, and the result is
/// [`BuildError::Interrupted`], so the caller removes what it made and lands nothing.
pub async fn watch_launch_until<C, CF, L, LF, I>(
    params: &WatchParams,
    covers: C,
    launch: L,
    interrupt: I,
) -> Result<Option<i32>, BuildError>
where
    I: Future<Output = ()>,
    C: Fn(VmNetwork) -> CF,
    CF: Future<Output = Result<Binding, BuildError>>,
    L: FnOnce(Duration, Arc<AtomicBool>) -> LF,
    LF: Future<Output = Result<Option<i32>, BuildError>>,
{
    let stop = Arc::new(AtomicBool::new(false));
    let done = AtomicBool::new(false);
    let launch = async {
        let r = launch(params.launch_timeout, stop.clone()).await;
        done.store(true, Ordering::SeqCst);
        r
    };
    let watch = async {
        let net = build_network();
        let started = tokio::time::Instant::now();
        let mut bound = false;
        let lost = |why: String| {
            stop.store(true, Ordering::SeqCst);
            Err(BuildError::WallLost(why))
        };
        loop {
            if done.load(Ordering::SeqCst) {
                return if bound {
                    Ok(())
                } else {
                    // Fail closed: the build ran without setup seeing the wall.
                    Err(BuildError::WallLost(format!(
                        "the build VM stopped before the wall was seen binding network {}",
                        net.id
                    )))
                };
            }
            // A read that hangs is a wall setup cannot see: lost, too.
            let polled = tokio::time::timeout(params.bind_deadline, covers(net.clone())).await;
            let Ok(polled) = polled else {
                return lost(format!(
                    "reading the wall's verdict took over {}s",
                    params.bind_deadline.as_secs()
                ));
            };
            match polled {
                Err(e) => return lost(e.to_string()),
                Ok(Binding::Bound) => bound = true,
                Ok(Binding::Pending) if bound => {
                    return lost(format!(
                        "network {} went back to pending after it was bound",
                        net.id
                    ))
                }
                Ok(Binding::Pending) if started.elapsed() >= params.bind_deadline => {
                    return lost(format!(
                        "network {} ({}) was still not bound {}s after the build VM started",
                        net.id,
                        net.subnet,
                        params.bind_deadline.as_secs()
                    ))
                }
                Ok(Binding::Pending) => {}
            }
            tokio::time::sleep(params.wall_poll).await;
        }
    };
    let both = async { tokio::join!(launch, watch) };
    tokio::pin!(both);
    let mut interrupted = false;
    let mut interrupt = std::pin::pin!(interrupt);
    let (code, watched) = loop {
        tokio::select! {
            biased;
            () = &mut interrupt, if !interrupted => {
                interrupted = true;
                stop.store(true, Ordering::SeqCst);
            }
            r = &mut both => break r,
        }
    };
    if interrupted {
        return Err(BuildError::Interrupted);
    }
    watched?;
    code
}

#[cfg(target_os = "macos")]
pub(crate) mod mac {
    use std::ffi::OsString;
    use std::fs::File;
    use std::io::Read;
    use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::Duration;

    use super::super::fsx::{self, Account, Dir, ROOT};
    use super::super::linux_image::{
        launcher_trusted, vm_home_ok, wall_binding, Binding, BuildError, WallEvidence,
        STATE_DIR_NAME,
    };
    use super::super::tools::{self, SAFE_PATH};
    use crate::platform::record::MetaFacts;
    use crate::wall::model::VmNetwork;

    /// How long `tar`, `hdiutil` and the like may take.
    pub(crate) const TOOL_LIMIT: Duration = Duration::from_secs(20 * 60);

    /// What a build runs as, and where: `_faber_vm`'s account and home, and `_faber`'s group.
    pub(crate) struct VmHome {
        pub(crate) vm: Account,
        pub(crate) faber: Account,
        pub(crate) path: PathBuf,
    }

    /// How a launcher run ended.
    pub(crate) struct LauncherRun {
        /// The exit code, `None` when the launcher was killed.
        pub(crate) code: Option<i32>,
        /// The launcher's own words (sanitised, at most 4000 characters) when it did not
        /// exit 0; empty otherwise.
        pub(crate) said: String,
    }

    impl VmHome {
        /// `_faber_vm`'s home, held open: root's directory entry, `_faber_vm`'s
        /// directory, mode 0700 or 0710 with group `_faber`.
        pub(crate) fn home(&self) -> Result<Dir, BuildError> {
            let home = fsx::open_leaf_dir(&self.path)
                .map_err(|e| BuildError::Bundle(format!("{}: {e}", self.path.display())))?;
            let meta = home.metadata()?;
            if !vm_home_ok(
                meta.uid(),
                meta.gid(),
                meta.mode(),
                self.vm.uid,
                self.faber.gid,
            ) {
                return Err(BuildError::Bundle(format!(
                    "{} is not _faber_vm's own directory at mode 0700 or 0710 group _faber",
                    self.path.display()
                )));
            }
            Ok(home)
        }

        /// `_faber_vm`'s state directory in its home: taken as it is when it
        /// is `_faber_vm`'s own (its mode set through the descriptor just
        /// judged), or made in root's `staging` and moved in when missing.
        /// Anything else there is refused, a root-owned directory included:
        /// `_faber_vm` could have moved one in, and root would give it away.
        pub(crate) fn ensure_state_dir(&self, home: &Dir, staging: &Dir) -> Result<(), BuildError> {
            let path = self.path.join(STATE_DIR_NAME);
            match home.subdir(STATE_DIR_NAME) {
                Ok(dir) => {
                    let uid = dir.metadata()?.uid();
                    if uid != self.vm.uid {
                        return Err(BuildError::Bundle(format!(
                            "{} is owned by uid {uid}, not _faber_vm; remove it (`sudo rm -rf \
                             \"{}\"`) and re-run",
                            path.display(),
                            path.display()
                        )));
                    }
                    dir.set_owner_mode(self.vm, 0o700)?;
                    Ok(())
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    place_dir(staging, home, STATE_DIR_NAME, self.vm, 0o700, |_| Ok(()))
                }
                Err(e) => Err(BuildError::Bundle(format!(
                    "{} is not a real directory ({e}); remove it and re-run",
                    path.display()
                ))),
            }
        }

        /// The bundle directory `bundle`, which must be `name` in this home.
        pub(crate) fn bundle_dir(&self, bundle: &Path, name: &str) -> Result<Dir, BuildError> {
            if bundle != self.path.join(name) {
                return Err(BuildError::Bundle("not this host's build bundle".into()));
            }
            self.home()?
                .subdir(name)
                .map_err(|e| BuildError::Bundle(format!("{}: {e}", bundle.display())))
        }

        /// Open `file` in the bundle `name`: `_faber_vm`'s own regular file with
        /// one link, never through a symlink.
        pub(crate) fn bundle_file(
            &self,
            bundle: &Path,
            name: &str,
            file: &str,
        ) -> Result<File, BuildError> {
            let dir = self.bundle_dir(bundle, name)?;
            let f = dir
                .open_file(file)
                .map_err(|e| BuildError::Land(format!("{}: {e}", bundle.join(file).display())))?;
            let meta = f.metadata()?;
            if let Some(problem) = fsx::append_problem(
                &bundle.join(file),
                meta.file_type().is_file(),
                meta.uid(),
                meta.nlink(),
                self.vm.uid,
            ) {
                return Err(BuildError::Land(problem));
            }
            Ok(f)
        }

        /// The tail of `file` in the bundle `name`, at most `cap` bytes.
        pub(crate) fn read_tail(
            &self,
            bundle: &Path,
            name: &str,
            file: &str,
            cap: u64,
        ) -> Result<Vec<u8>, BuildError> {
            let f = self.bundle_file(bundle, name, file)?;
            let len = f.metadata()?.len();
            let start = len.saturating_sub(cap);
            let mut buf = vec![0u8; (len - start) as usize];
            let mut filled = 0;
            while filled < buf.len() {
                let n = f.read_at(&mut buf[filled..], start + filled as u64)?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            buf.truncate(filled);
            Ok(buf)
        }

        /// Run `launcher` with `args` as `_faber_vm` until it exits, `timeout` passes, or
        /// `stop` is set (then it is killed). The launcher is judged trusted again here:
        /// the plan's check was minutes ago. Killing the launcher ends the VM, since
        /// Virtualization.framework tears a VM down with the process that runs it.
        pub(crate) async fn run_launcher(
            &self,
            launcher: &Path,
            args: Vec<OsString>,
            timeout: Duration,
            stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
        ) -> Result<LauncherRun, BuildError> {
            launcher_trusted(launcher, Some(self.faber.uid)).map_err(BuildError::Launcher)?;
            let (launcher, vm, home) = (launcher.to_path_buf(), self.vm, self.path.clone());
            let out = tokio::task::spawn_blocking(move || {
                let refs: Vec<&std::ffi::OsStr> = args.iter().map(OsString::as_os_str).collect();
                tools::run_as_until(
                    &launcher,
                    &refs,
                    None,
                    Some(vm),
                    &home,
                    timeout,
                    Some(&stop),
                )
            })
            .await
            .map_err(|e| BuildError::Launcher(e.to_string()))?
            .map_err(|e| BuildError::Launcher(format!("{e:#}")))?;
            let said = if out.success {
                String::new()
            } else {
                out.sanitized(4000)
            };
            Ok(LauncherRun {
                code: out.code,
                said,
            })
        }
    }

    /// Read the stored rules and the checker's verdict as they are now, and
    /// judge them for `net` ([`wall_binding`]) off the async runtime's threads.
    pub(crate) async fn wall_covers(net: &VmNetwork) -> Result<Binding, BuildError> {
        let net = net.clone();
        tokio::task::spawn_blocking(move || read_and_judge_wall(&net))
            .await
            .map_err(|e| BuildError::Wall(e.to_string()))?
            .map_err(BuildError::Wall)
    }

    /// Clone `src` as `name` in `dir`, or, where the volume cannot clone, copy
    /// it, skipping zero blocks so a sparse disk stays sparse. Either way the
    /// result is a new file (an existing `name` is an error).
    ///
    /// `dir` must be root's own: on a clone the new file is reopened by name,
    /// which is sound only where no other account can swap that name. The
    /// clone carries `src`'s ACL and extended attributes; [`give`] strips them.
    /// It never carries `src`'s owner: [`fsx::clone_file`].
    ///
    /// A source longer than `cap` bytes is refused, at the length the copy
    /// would use, and a clone longer than `cap` is removed and refused: the
    /// source's owner can change its length at any time.
    pub(crate) fn clone_or_copy(
        src: &File,
        dir: &Dir,
        name: &str,
        cap: u64,
    ) -> std::io::Result<File> {
        let too_long = |len: u64| {
            std::io::Error::other(format!(
                "{} would be {len} bytes, more than {cap}; refused",
                dir.path().join(name).display()
            ))
        };
        match fsx::clone_file(src, dir, name) {
            Ok(()) => {
                let dst = dir.open_file(name)?;
                let len = dst.metadata()?.len();
                if len > cap {
                    let _ = dir.unlink(name);
                    return Err(too_long(len));
                }
                return Ok(dst);
            }
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => return Err(err),
            Err(_) => {}
        }
        let len = src.metadata()?.len();
        if len > cap {
            return Err(too_long(len));
        }
        let dst = dir.create_new(name, 0o600)?;
        let mut buf = vec![0u8; 1 << 20];
        let mut at = 0u64;
        while at < len {
            let n = src.read_at(&mut buf, at)?;
            if n == 0 {
                break;
            }
            if buf[..n].iter().any(|&b| b != 0) {
                dst.write_all_at(&buf[..n], at)?;
            }
            at += n as u64;
        }
        dst.set_len(len)?;
        Ok(dst)
    }

    /// Give `file`, which root just made as `name` in its own `dir`, to
    /// `owner` at `mode`, all through the descriptor: judged first (still
    /// root's regular file with one link, [`fsx::fresh_root_file_problem`]),
    /// stripped of any ACL and extended attribute, then `fchown`, `fchmod`,
    /// `fsync`. Nothing here is ever done to an inode opened by name in a
    /// directory another account can write.
    pub(crate) fn give(
        file: &File,
        dir: &Dir,
        name: &str,
        owner: Account,
        mode: u32,
    ) -> Result<(), BuildError> {
        let meta = file.metadata()?;
        if let Some(problem) = fsx::fresh_root_file_problem(
            &dir.path().join(name),
            meta.file_type().is_file(),
            meta.uid(),
            meta.nlink(),
        ) {
            return Err(BuildError::Bundle(problem));
        }
        fsx::acl::strip(file)?;
        std::os::unix::fs::fchown(file, Some(owner.uid), Some(owner.gid))?;
        file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        file.sync_all()?;
        Ok(())
    }

    /// Root's own directory at `path`, held open: uid 0, writable by nobody
    /// else, so a name in it means what root put there.
    pub(crate) fn root_own_dir(path: &Path) -> Result<Dir, BuildError> {
        let dir = Dir::open_nofollow(path)
            .map_err(|e| BuildError::Bundle(format!("{}: {e}", path.display())))?;
        let meta = dir.metadata()?;
        if meta.uid() != ROOT.uid || meta.mode() & 0o077 != 0 {
            return Err(BuildError::Bundle(format!(
                "{} is not root's own directory at mode 0700",
                path.display()
            )));
        }
        Ok(dir)
    }

    /// Make the directory `name` in root's own `staging`, give it to `owner`
    /// at `mode` through its descriptor, and move it into `dest` (another
    /// account's directory) without replacing anything there. `fill` puts
    /// its contents in while it is still out of that account's reach.
    pub(crate) fn place_dir(
        staging: &Dir,
        dest: &Dir,
        name: &str,
        owner: Account,
        mode: u32,
        fill: impl FnOnce(&Dir) -> Result<(), BuildError>,
    ) -> Result<(), BuildError> {
        let shown = || dest.path().join(name).display().to_string();
        if !staging.mkdir(name)? {
            return Err(BuildError::Bundle(format!(
                "{} is already in root's staging",
                staging.path().join(name).display()
            )));
        }
        let dir = staging.subdir(name)?;
        fill(&dir)?;
        dir.set_owner_mode(owner, mode)?;
        fsx::acl::strip(&dir)?;
        staging
            .move_new(name, dest, name)
            .map_err(|e| BuildError::Bundle(format!("moving into {}: {e}", shown())))
    }

    pub(crate) fn run_tool(mut cmd: Command, what: &str) -> Result<(), BuildError> {
        cmd.env_clear()
            .env("PATH", SAFE_PATH)
            .current_dir("/")
            .stdin(Stdio::null());
        let out = tools::output_within(cmd, TOOL_LIMIT)
            .map_err(|e| BuildError::Tool(format!("{what}: {e:#}")))?;
        if !out.success {
            return Err(BuildError::Tool(format!(
                "{what} failed: {}",
                out.sanitized(2000)
            )));
        }
        Ok(())
    }

    /// `path`'s bytes, refused when larger than `cap` (never read truncated:
    /// a cut-off JSON file only fails later, as a misleading parse error). A
    /// file [`wall_verdict`] then judges by its chain: once that passes, only
    /// root could have replaced it.
    pub(crate) fn read_capped(path: &Path, cap: u64) -> Result<Vec<u8>, String> {
        let mut out = Vec::new();
        File::open(path)
            .and_then(|f| f.take(cap + 1).read_to_end(&mut out))
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if out.len() as u64 > cap {
            return Err(format!(
                "{} is larger than {cap} bytes; refusing it",
                path.display()
            ));
        }
        Ok(out)
    }

    /// The [`MetaFacts`] of `path` and its directory, never following a
    /// symlink at `path` itself.
    pub(crate) fn meta_facts(path: &Path) -> Result<MetaFacts, String> {
        let shown = |e: std::io::Error| format!("{}: {e}", path.display());
        let file = std::fs::symlink_metadata(path).map_err(shown)?;
        let parent = path.parent().unwrap_or(Path::new("/"));
        let dir = std::fs::symlink_metadata(parent).map_err(shown)?;
        Ok(MetaFacts {
            is_symlink: file.file_type().is_symlink(),
            is_regular: file.file_type().is_file(),
            uid: file.uid(),
            mode: file.mode(),
            parent_is_dir: dir.file_type().is_dir(),
            parent_uid: dir.uid(),
            parent_mode: dir.mode(),
        })
    }

    /// Read the stored rules and the checker's verdict as they are now, and
    /// judge them for `net` ([`wall_binding`]).
    pub(crate) fn read_and_judge_wall(net: &VmNetwork) -> Result<Binding, String> {
        use crate::wall::paths::{STORED, STORED_READ_CAP, VERDICT_CANONICAL};
        let chain =
            |p: &Path| super::super::exe::chain_of(p).map_err(|e| format!("{}: {e}", p.display()));
        let stored_chain = chain(Path::new(STORED))?;
        let stored_path = stored_chain
            .first()
            .map(|f| f.path.clone())
            .ok_or_else(|| format!("{STORED}: no path facts"))?;
        let stored = read_capped(&stored_path, STORED_READ_CAP)?;
        let verdict_path = Path::new(VERDICT_CANONICAL);
        let verdict_facts = meta_facts(verdict_path)?;
        let verdict_chain = chain(verdict_path)?;
        let verdict = read_capped(verdict_path, STORED_READ_CAP)?;
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| format!("the clock: {e}"))?
            .as_secs();
        wall_binding(
            net,
            &WallEvidence {
                stored: &stored,
                stored_chain: &stored_chain,
                verdict: &verdict,
                verdict_facts: &verdict_facts,
                verdict_chain: &verdict_chain,
                now_unix,
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file over the cap is refused, never read cut off: a truncated
    /// `stored.json` used to fail later as "EOF while parsing" (2026-10-21).
    #[cfg(target_os = "macos")]
    #[test]
    fn read_capped_refuses_rather_than_truncates() {
        let dir = std::env::temp_dir().join(format!("faber-read-capped-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.json");
        std::fs::write(&path, vec![b'x'; 10]).unwrap();
        assert_eq!(mac::read_capped(&path, 10).unwrap().len(), 10);
        let err = mac::read_capped(&path, 9).unwrap_err();
        assert!(err.contains("larger than 9 bytes"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn hup_self() {
        let status = std::process::Command::new("/bin/kill")
            .args(["-HUP", &std::process::id().to_string()])
            .status()
            .unwrap();
        assert!(status.success());
    }

    /// One test, because the dispositions are the process's: a signal ignored on entry
    /// stays ignored and is not waited for; otherwise the handlers exist from the call (a
    /// SIGHUP before the first poll is seen, not fatal), and the disposition found is the
    /// one left.
    #[tokio::test]
    async fn dispositions_are_kept_and_restored() {
        // Ignored before (as under nohup): still ignored during and after.
        swap_disposition(SIGHUP, SIG_IGN);
        let sig = terminate_signal();
        assert_eq!(swap_disposition(SIGHUP, SIG_IGN), SIG_IGN);
        hup_self();
        assert!(tokio::time::timeout(Duration::from_millis(300), sig)
            .await
            .is_err());
        assert_eq!(swap_disposition(SIGHUP, SIG_DFL), SIG_IGN);

        // Default before: caught from the call, and the default is put back.
        let sig = terminate_signal();
        hup_self();
        tokio::time::timeout(Duration::from_secs(5), sig)
            .await
            .expect("the signal was seen");
        assert_eq!(swap_disposition(SIGHUP, SIG_DFL), SIG_DFL);
        assert_eq!(swap_disposition(SIGINT, SIG_DFL), SIG_DFL);
        assert_eq!(swap_disposition(SIGTERM, SIG_DFL), SIG_DFL);
    }
}
