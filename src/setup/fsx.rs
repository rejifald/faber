//! Writing into directories another account controls, as root, without
//! following anything that account planted (ADR-0025's root invariant).
//!
//! `_faber` owns its state directory, and the daemon runs as `_faber`, so any
//! entry in that directory may have been replaced by a symlink before setup
//! runs again. A path-based write (`std::fs::write`, `rename`) resolves every
//! component afresh and would follow such a link wherever it points, with
//! root's rights. So setup holds each directory open by descriptor ([`Dir`])
//! and works relative to it:
//!
//! - a directory is opened with `O_DIRECTORY | O_NOFOLLOW`, so a symlink in its
//!   place is refused rather than followed
//! - a file is written as a new temp file (`O_CREAT | O_EXCL | O_NOFOLLOW`),
//!   hashed back through its own descriptor, then `fchown`, `fchmod`, `fsync`,
//!   and `renameat` over the final name, which replaces the directory entry
//!   and never follows it
//! - a file is read with `O_NOFOLLOW`, and only if it is a regular file
//!
//! Top-level directories are opened through their parent, which is a system
//! directory, so only the last component needs the guard.
//!
//! The `*at` calls are declared by hand, as `crate::lock` declares `flock`, so
//! faber keeps no `libc` dependency. Their flag values differ by OS, and on
//! Linux by architecture, which is why [`flags`] is spelled out per target;
//! the tests below check each flag's behaviour, not just its value.

use std::ffi::{CString, OsStr};
use std::fs::{File, Metadata, Permissions};
use std::io::{self, Read, Seek, SeekFrom};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::raw::{c_char, c_int, c_uint};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

pub use crate::platform::record::Account;

/// Root, as an owner: uid 0 and gid 0 (`wheel` on macOS, `root` on Linux).
pub const ROOT: Account = Account { uid: 0, gid: 0 };

#[cfg(target_os = "macos")]
mod flags {
    use std::os::raw::c_int;
    pub const O_RDONLY: c_int = 0x0000;
    pub const O_WRONLY: c_int = 0x0001;
    pub const O_RDWR: c_int = 0x0002;
    pub const O_NONBLOCK: c_int = 0x0004;
    pub const O_APPEND: c_int = 0x0008;
    pub const O_NOFOLLOW: c_int = 0x0100;
    pub const O_CREAT: c_int = 0x0200;
    pub const O_EXCL: c_int = 0x0800;
    pub const O_DIRECTORY: c_int = 0x0010_0000;
    pub const O_CLOEXEC: c_int = 0x0100_0000;
    pub const AT_FDCWD: c_int = -2;
    pub const AT_REMOVEDIR: c_int = 0x0080;
    pub type Mode = u16;
}

#[cfg(target_os = "linux")]
mod flags {
    use std::os::raw::c_int;
    pub const O_RDONLY: c_int = 0;
    pub const O_WRONLY: c_int = 0o1;
    pub const O_RDWR: c_int = 0o2;
    pub const O_CREAT: c_int = 0o100;
    pub const O_EXCL: c_int = 0o200;
    pub const O_APPEND: c_int = 0o2000;
    pub const O_NONBLOCK: c_int = 0o4000;
    pub const O_CLOEXEC: c_int = 0o2_000_000;
    // The generic values; arm and aarch64 define these two differently.
    #[cfg(any(target_arch = "x86_64", target_arch = "x86", target_arch = "riscv64"))]
    pub const O_DIRECTORY: c_int = 0o200_000;
    #[cfg(any(target_arch = "x86_64", target_arch = "x86", target_arch = "riscv64"))]
    pub const O_NOFOLLOW: c_int = 0o400_000;
    #[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
    pub const O_DIRECTORY: c_int = 0o40_000;
    #[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
    pub const O_NOFOLLOW: c_int = 0o100_000;
    pub const AT_FDCWD: c_int = -100;
    pub const AT_REMOVEDIR: c_int = 0x200;
    pub type Mode = u32;
}

#[cfg(all(
    target_os = "linux",
    not(any(
        target_arch = "x86_64",
        target_arch = "x86",
        target_arch = "riscv64",
        target_arch = "aarch64",
        target_arch = "arm"
    ))
))]
compile_error!("setup::fsx: check this architecture's O_DIRECTORY and O_NOFOLLOW values");

/// The flags for opening, outside setup, a file root wrote: `O_NOFOLLOW`, so
/// a symlink in its place fails the open, and `O_NONBLOCK`, so a FIFO there
/// does not block it. The caller judges what it opened by its own `fstat`.
pub(crate) const READ_NOFOLLOW: c_int = flags::O_NOFOLLOW | flags::O_NONBLOCK;
/// For `OpenOptions::custom_flags` when writing: never through a symlink.
pub(crate) const WRITE_NOFOLLOW: c_int = flags::O_NOFOLLOW;

extern "C" {
    /// POSIX. Variadic in C: the mode is only read with `O_CREAT`, and it must
    /// be declared variadic here too, because Apple's arm64 ABI passes variadic
    /// arguments on the stack rather than in registers.
    fn openat(dirfd: c_int, path: *const c_char, oflag: c_int, ...) -> c_int;
    fn mkdirat(dirfd: c_int, path: *const c_char, mode: flags::Mode) -> c_int;
    fn renameat(fromfd: c_int, from: *const c_char, tofd: c_int, to: *const c_char) -> c_int;
    fn unlinkat(dirfd: c_int, path: *const c_char, flag: c_int) -> c_int;
}

fn cstring(s: &OsStr) -> io::Result<CString> {
    CString::new(s.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "a path contains a NUL byte"))
}

/// `name` as a single path component: never empty, `.`, `..`, or containing a
/// `/`, so an `*at` call cannot walk out of its directory.
fn component(name: &str) -> io::Result<CString> {
    if name.is_empty() || name == "." || name == ".." || name.contains('/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{name:?} is not a single file name"),
        ));
    }
    cstring(OsStr::new(name))
}

fn check(rc: c_int) -> io::Result<c_int> {
    if rc < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(rc)
    }
}

/// Own a descriptor an `*at` call just returned.
fn owned(fd: c_int) -> File {
    // SAFETY: `fd` was just returned by a successful `openat`, and nothing else
    // holds it, so the `File` becomes its only owner.
    File::from(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// A directory held open by descriptor. Every operation on it takes a single
/// file name, resolved relative to the descriptor.
#[derive(Debug)]
pub struct Dir {
    file: File,
    path: PathBuf,
}

impl Dir {
    /// Open `path` as a directory, following symlinks on the way: for system
    /// directories, which only root can replace.
    pub fn open(path: &Path) -> io::Result<Dir> {
        Self::open_with(path, 0)
    }

    /// Open `path` as a directory, refusing a symlink in its last component.
    pub fn open_nofollow(path: &Path) -> io::Result<Dir> {
        Self::open_with(path, flags::O_NOFOLLOW)
    }

    fn open_with(path: &Path, extra: c_int) -> io::Result<Dir> {
        let c = cstring(path.as_os_str())?;
        // SAFETY: `c` is NUL-terminated and outlives the call; `openat` reads it
        // and keeps nothing.
        let fd = check(unsafe {
            openat(
                flags::AT_FDCWD,
                c.as_ptr(),
                flags::O_RDONLY | flags::O_DIRECTORY | flags::O_CLOEXEC | extra,
            )
        })?;
        Ok(Dir {
            file: owned(fd),
            path: path.to_path_buf(),
        })
    }

    /// Where this directory was opened from, for messages.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn at(&self, name: &str, oflag: c_int, mode: c_uint) -> io::Result<File> {
        let c = component(name)?;
        // SAFETY: the descriptor is open for the life of `self`, `c` is
        // NUL-terminated and outlives the call, and the mode is passed as the
        // C variadic `unsigned int` the ABI expects.
        let fd = check(unsafe {
            openat(
                self.file.as_raw_fd(),
                c.as_ptr(),
                oflag | flags::O_CLOEXEC,
                mode,
            )
        })?;
        Ok(owned(fd))
    }

    /// The subdirectory `name`, refusing a symlink in its place.
    pub fn subdir(&self, name: &str) -> io::Result<Dir> {
        let file = self.at(
            name,
            flags::O_RDONLY | flags::O_DIRECTORY | flags::O_NOFOLLOW,
            0,
        )?;
        Ok(Dir {
            file,
            path: self.path.join(name),
        })
    }

    /// The names in this directory, read through its descriptor (`fdopendir`),
    /// so nothing is looked up by path. Without `.` and `..`. macOS only: the
    /// `dirent` layout is the Darwin one.
    #[cfg(target_os = "macos")]
    pub fn names(&self) -> io::Result<Vec<String>> {
        #[repr(C)]
        struct Dirent {
            d_ino: u64,
            d_seekoff: u64,
            d_reclen: u16,
            d_namlen: u16,
            d_type: u8,
            d_name: [c_char; 1024],
        }
        extern "C" {
            #[cfg_attr(target_arch = "x86_64", link_name = "fdopendir$INODE64")]
            fn fdopendir(fd: c_int) -> *mut std::ffi::c_void;
            #[cfg_attr(target_arch = "x86_64", link_name = "readdir$INODE64")]
            fn readdir(dir: *mut std::ffi::c_void) -> *mut Dirent;
            fn rewinddir(dir: *mut std::ffi::c_void);
            fn closedir(dir: *mut std::ffi::c_void) -> c_int;
            /// Darwin's errno accessor.
            fn __error() -> *mut c_int;
        }
        let dup = self.file.try_clone()?;
        let fd = std::os::fd::IntoRawFd::into_raw_fd(dup);
        // SAFETY: `fd` is a fresh descriptor this call owns; on success the
        // stream owns it and `closedir` closes it, on failure it is closed here.
        let dir = unsafe { fdopendir(fd) };
        if dir.is_null() {
            let e = io::Error::last_os_error();
            drop(unsafe { OwnedFd::from_raw_fd(fd) });
            return Err(e);
        }
        // The dup'd descriptor shares its offset with `self.file`, so a
        // listing already made would leave this one at the end.
        // SAFETY: `dir` is an open stream.
        unsafe { rewinddir(dir) };
        let mut out = Vec::new();
        loop {
            // SAFETY: `dir` is open until `closedir`; the entry is read before
            // the next `readdir` call and its `d_name` holds `d_namlen` bytes.
            // errno is cleared first so NULL with errno set is told from the end.
            let ent = unsafe {
                *__error() = 0;
                readdir(dir)
            };
            if ent.is_null() {
                let errno = unsafe { *__error() };
                if errno != 0 {
                    let e = io::Error::from_raw_os_error(errno);
                    // SAFETY: closes the stream and its descriptor, once.
                    unsafe { closedir(dir) };
                    return Err(e);
                }
                break;
            }
            let ent = unsafe { &*ent };
            let bytes: Vec<u8> = ent.d_name[..usize::from(ent.d_namlen)]
                .iter()
                .map(|&c| c as u8)
                .collect();
            if bytes != b"." && bytes != b".." {
                if let Ok(n) = String::from_utf8(bytes) {
                    out.push(n);
                }
            }
        }
        // SAFETY: closes the stream and its descriptor, once.
        unsafe { closedir(dir) };
        Ok(out)
    }

    /// The names in this directory, read through its descriptor, so nothing is
    /// looked up by path again. Without `.` and `..`. Linux reads it as
    /// `/proc/self/fd/<fd>`, which opens the directory the descriptor holds, a
    /// fresh stream from the start, even if its path has since been replaced.
    #[cfg(target_os = "linux")]
    pub fn names(&self) -> io::Result<Vec<String>> {
        let mut out = Vec::new();
        for entry in std::fs::read_dir(format!("/proc/self/fd/{}", self.file.as_raw_fd()))? {
            if let Ok(n) = entry?.file_name().into_string() {
                out.push(n);
            }
        }
        Ok(out)
    }

    /// Create the subdirectory `name` at mode 0700. `Ok(false)` when something
    /// is already there, which [`Dir::subdir`] then judges.
    pub fn mkdir(&self, name: &str) -> io::Result<bool> {
        let c = component(name)?;
        // SAFETY: as in `at`.
        let rc = unsafe { mkdirat(self.file.as_raw_fd(), c.as_ptr(), 0o700) };
        match check(rc) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Open `name` for reading without following a symlink there (the open
    /// fails) and without blocking on a FIFO. The caller checks that what it
    /// got is a regular file.
    pub fn open_file(&self, name: &str) -> io::Result<File> {
        self.at(
            name,
            flags::O_RDONLY | flags::O_NOFOLLOW | flags::O_NONBLOCK,
            0,
        )
    }

    /// Create the regular file `name` exclusively (`O_EXCL | O_NOFOLLOW`), at
    /// `mode` before the umask, open for reading and writing. Anything already
    /// there, a symlink included, is an error.
    pub fn create_new(&self, name: &str, mode: u32) -> io::Result<File> {
        self.at(
            name,
            flags::O_RDWR | flags::O_CREAT | flags::O_EXCL | flags::O_NOFOLLOW,
            mode,
        )
    }

    /// Whether anything is at `name`, a dangling symlink included. Anything
    /// but a plain "not found" counts as present, so a caller that must never
    /// overwrite errs toward leaving things alone.
    pub fn exists(&self, name: &str) -> io::Result<bool> {
        component(name)?;
        match self.open_file(name) {
            Ok(_) => Ok(true),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(_) => Ok(true),
        }
    }

    /// Write everything `src` yields to `name`, owned by `owner` at `mode`,
    /// through a new temp file that is renamed over `name` only once complete
    /// and on disk. Returns the SHA-256 of the bytes written, in hex.
    pub fn write_atomic(
        &self,
        name: &str,
        src: &mut dyn Read,
        owner: Account,
        mode: u32,
    ) -> io::Result<String> {
        self.write_checked(name, src, owner, mode, None)
    }

    /// [`Dir::write_atomic`] for a copy that must be exactly the bytes a plan
    /// was made with: the temp file is hashed back through its own descriptor
    /// before anything else, and unless that is `want` it is removed and
    /// nothing lands at `name`. A source swapped after it was hashed (while
    /// the operator read the plan, say) never reaches the destination, not
    /// even for the moment before the comparison.
    pub fn write_atomic_expecting(
        &self,
        name: &str,
        src: &mut dyn Read,
        owner: Account,
        mode: u32,
        want: &str,
    ) -> io::Result<String> {
        self.write_checked(name, src, owner, mode, Some(want))
    }

    fn write_checked(
        &self,
        name: &str,
        src: &mut dyn Read,
        owner: Account,
        mode: u32,
        want: Option<&str>,
    ) -> io::Result<String> {
        component(name)?;
        let (tmp_name, mut tmp) = self.create_temp(name)?;
        let written = (|| {
            io::copy(src, &mut tmp)?;
            // What is on disk, read back through the descriptor that wrote
            // it, while the file is still root's own 0600: not what `src`
            // claimed to yield.
            tmp.seek(SeekFrom::Start(0))?;
            let sha = sha256_of(&mut tmp)?;
            if let Some(want) = want.filter(|want| *want != sha) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("the copy's SHA-256 is {sha}, not the {want} it was planned with"),
                ));
            }
            std::os::unix::fs::fchown(&tmp, Some(owner.uid), Some(owner.gid))?;
            tmp.set_permissions(Permissions::from_mode(mode))?;
            tmp.sync_all()?;
            self.rename(&tmp_name, name)?;
            Ok(sha)
        })();
        match written {
            Ok(sha) => {
                // Persist the rename itself. Best effort: not every filesystem
                // syncs a directory descriptor, and the data is already on disk.
                let _ = self.file.sync_all();
                Ok(sha)
            }
            Err(e) => {
                let _ = self.unlink(&tmp_name);
                Err(e)
            }
        }
    }

    /// A fresh, exclusively created temp file beside `name`.
    fn create_temp(&self, name: &str) -> io::Result<(String, File)> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        for _ in 0..64 {
            let tmp = format!(
                ".{name}.faber-tmp-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            );
            // Read and write: the copy is hashed back through this descriptor.
            match self.at(
                &tmp,
                flags::O_RDWR | flags::O_CREAT | flags::O_EXCL | flags::O_NOFOLLOW,
                0o600,
            ) {
                Ok(file) => return Ok((tmp, file)),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("no free temp name beside {name} in {}", self.path.display()),
        ))
    }

    /// Rename `from` to `to` within this directory. Replaces `to` if it
    /// exists, and follows neither name.
    pub fn rename(&self, from: &str, to: &str) -> io::Result<()> {
        let (f, t) = (component(from)?, component(to)?);
        let fd = self.file.as_raw_fd();
        // SAFETY: as in `at`, for both names.
        check(unsafe { renameat(fd, f.as_ptr(), fd, t.as_ptr()) }).map(|_| ())
    }

    /// Give the regular file `from` its final `owner` and `mode` through its
    /// own descriptor, put it on disk, and rename it over `to` in this
    /// directory: the last step of installing a file staged here, so what
    /// lands at `to` is exactly the staged bytes. Follows neither name.
    pub fn promote(&self, from: &str, to: &str, owner: Account, mode: u32) -> io::Result<()> {
        let file = self.open_file(from)?;
        if !file.metadata()?.file_type().is_file() {
            return Err(io::Error::other(format!(
                "{} is not a regular file",
                self.path.join(from).display()
            )));
        }
        std::os::unix::fs::fchown(&file, Some(owner.uid), Some(owner.gid))?;
        file.set_permissions(Permissions::from_mode(mode))?;
        file.sync_all()?;
        self.rename(from, to)?;
        // As in `write_checked`: best effort, the data is already on disk.
        let _ = self.file.sync_all();
        Ok(())
    }

    /// Remove the file (or symlink) `name`. Never follows it.
    pub fn unlink(&self, name: &str) -> io::Result<()> {
        let c = component(name)?;
        // SAFETY: as in `at`.
        check(unsafe { unlinkat(self.file.as_raw_fd(), c.as_ptr(), 0) }).map(|_| ())
    }

    /// This directory's own metadata, from its descriptor.
    pub fn metadata(&self) -> io::Result<Metadata> {
        self.file.metadata()
    }

    /// Create the regular file `name` empty if it is missing, or take the one
    /// there if it is a regular file owned by one of `prior`, and set its owner
    /// and mode through its descriptor. A symlink or anything else in its place
    /// is refused, as is a file some other account planted. Returns whether it
    /// was created.
    pub fn ensure_file(
        &self,
        name: &str,
        owner: Account,
        mode: u32,
        prior: &[u32],
    ) -> io::Result<bool> {
        let path = self.path.join(name);
        let (file, created) = match self.at(
            name,
            flags::O_WRONLY | flags::O_CREAT | flags::O_EXCL | flags::O_NOFOLLOW,
            0o600,
        ) {
            Ok(file) => (file, true),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let file = self.open_file(name).map_err(|e| {
                    io::Error::new(
                        e.kind(),
                        format!(
                            "{} is not a regular file ({e}); remove it (`sudo rm \"{}\"`) and \
                             re-run",
                            path.display(),
                            path.display()
                        ),
                    )
                })?;
                (file, false)
            }
            Err(e) => return Err(e),
        };
        let meta = file.metadata()?;
        if !meta.file_type().is_file() {
            return Err(io::Error::other(format!(
                "{} is not a regular file; remove it (`sudo rm -r \"{}\"`) and re-run",
                path.display(),
                path.display()
            )));
        }
        if !created && !prior.contains(&meta.uid()) {
            return Err(planted(&path, meta.uid(), "rm"));
        }
        if meta.uid() != owner.uid || meta.gid() != owner.gid {
            std::os::unix::fs::fchown(&file, Some(owner.uid), Some(owner.gid))?;
        }
        if meta.mode() & 0o7777 != mode {
            file.set_permissions(Permissions::from_mode(mode))?;
        }
        Ok(created)
    }

    /// [`Dir::ensure_file`] for a file setup never creates: give the regular
    /// file `name`, if it is there, `owner` and `mode` through its descriptor,
    /// taking it over only if one of `prior` owns it. A symlink or anything
    /// else in its place is refused, as is a file some other account planted.
    /// Returns whether it was there.
    pub fn fix_file(
        &self,
        name: &str,
        owner: Account,
        mode: u32,
        prior: &[u32],
    ) -> io::Result<bool> {
        let path = self.path.join(name);
        let file = match self.open_file(name) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(e) => {
                return Err(io::Error::new(
                    e.kind(),
                    format!(
                        "{} is not a regular file ({e}); remove it (`sudo rm \"{}\"`) and re-run",
                        path.display(),
                        path.display()
                    ),
                ))
            }
        };
        let meta = file.metadata()?;
        if !meta.file_type().is_file() {
            return Err(io::Error::other(format!(
                "{} is not a regular file; remove it (`sudo rm -r \"{}\"`) and re-run",
                path.display(),
                path.display()
            )));
        }
        if !prior.contains(&meta.uid()) {
            return Err(planted(&path, meta.uid(), "rm"));
        }
        if meta.uid() != owner.uid || meta.gid() != owner.gid {
            std::os::unix::fs::fchown(&file, Some(owner.uid), Some(owner.gid))?;
        }
        if meta.mode() & 0o7777 != mode {
            file.set_permissions(Permissions::from_mode(mode))?;
        }
        Ok(true)
    }

    /// Set this directory's owner and mode, from its descriptor. Only this
    /// directory: never anything inside it.
    pub fn set_owner_mode(&self, owner: Account, mode: u32) -> io::Result<()> {
        let meta = self.metadata()?;
        if meta.uid() != owner.uid || meta.gid() != owner.gid {
            std::os::unix::fs::fchown(&self.file, Some(owner.uid), Some(owner.gid))?;
        }
        if meta.mode() & 0o7777 != mode {
            self.file.set_permissions(Permissions::from_mode(mode))?;
        }
        Ok(())
    }

    /// Rename `name` to `aside` in this directory, refusing if `aside` is
    /// already taken: a backup is never overwritten. Follows neither name.
    /// The one way setup moves anything aside: an old definition, a copy
    /// `--remigrate` replaces, the daemon binary, a legacy directory.
    pub fn move_aside(&self, name: &str, aside: &str) -> io::Result<()> {
        if self.exists(aside)? {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "{} exists already; not overwriting a backup",
                    self.path.join(aside).display()
                ),
            ));
        }
        self.rename(name, aside)
    }

    /// Append `bytes` to the regular file `name`, which belongs to `owner`:
    /// for root writing into a log another account owns, in that account's
    /// directory.
    ///
    /// The file is opened `O_APPEND | O_NOFOLLOW | O_NONBLOCK`, so a symlink in
    /// its place is refused and a FIFO cannot stall root. A missing file is
    /// created exclusively and given to `owner` at `mode` through its
    /// descriptor. Then the descriptor itself is judged ([`append_problem`]):
    /// a regular file, owned by `owner`, with one link. The last matters most:
    /// the account can make a hard link to any file it can see, a root file
    /// included, and a write through that name would land in root's file.
    pub fn append_owned(
        &self,
        name: &str,
        bytes: &[u8],
        owner: Account,
        mode: u32,
    ) -> io::Result<()> {
        use std::io::Write;
        let existing = flags::O_WRONLY | flags::O_APPEND | flags::O_NOFOLLOW | flags::O_NONBLOCK;
        let file = match self.at(name, existing, 0) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                match self.at(name, existing | flags::O_CREAT | flags::O_EXCL, 0o600) {
                    Ok(file) => {
                        std::os::unix::fs::fchown(&file, Some(owner.uid), Some(owner.gid))?;
                        file.set_permissions(Permissions::from_mode(mode))?;
                        file
                    }
                    // Created between the two opens: judge that one.
                    Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                        self.at(name, existing, 0)?
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        };
        let meta = file.metadata()?;
        if let Some(problem) = append_problem(
            &self.path.join(name),
            meta.file_type().is_file(),
            meta.uid(),
            meta.nlink(),
            owner.uid,
        ) {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, problem));
        }
        (&file).write_all(bytes)
    }

    /// Remove the empty directory `name`. Never follows it.
    pub fn rmdir(&self, name: &str) -> io::Result<()> {
        let c = component(name)?;
        // SAFETY: as in `at`.
        check(unsafe { unlinkat(self.file.as_raw_fd(), c.as_ptr(), flags::AT_REMOVEDIR) })
            .map(|_| ())
    }

    /// Move `from` in this directory to `to` in `dest`, never replacing
    /// anything: an entry already at `to`, a symlink included, is an error
    /// (`EEXIST`). Follows neither name. For root handing something it
    /// finished in its own directory to a directory another account can
    /// write: the inode moves as it is, so nothing is reopened by name there.
    /// The Linux one is below.
    #[cfg(target_os = "macos")]
    pub fn move_new(&self, from: &str, dest: &Dir, to: &str) -> io::Result<()> {
        /// `<stdio.h>`: fail rather than replace an existing `to`.
        const RENAME_EXCL: c_uint = 0x0000_0004;
        extern "C" {
            fn renameatx_np(
                fromfd: c_int,
                from: *const c_char,
                tofd: c_int,
                to: *const c_char,
                flags: c_uint,
            ) -> c_int;
        }
        let (f, t) = (component(from)?, component(to)?);
        // SAFETY: both descriptors are open for the life of `self` and
        // `dest`, both names are NUL-terminated and outlive the call, and the
        // call keeps nothing.
        check(unsafe {
            renameatx_np(
                self.file.as_raw_fd(),
                f.as_ptr(),
                dest.file.as_raw_fd(),
                t.as_ptr(),
                RENAME_EXCL,
            )
        })?;
        let _ = dest.file.sync_all();
        Ok(())
    }

    /// [`Dir::move_new`] on Linux: `renameat2(RENAME_NOREPLACE)` (glibc 2.28,
    /// Linux 3.15), which fails with `EEXIST` rather than replace `to`.
    #[cfg(target_os = "linux")]
    pub fn move_new(&self, from: &str, dest: &Dir, to: &str) -> io::Result<()> {
        /// `<linux/fs.h>`: fail rather than replace an existing `to`.
        const RENAME_NOREPLACE: c_uint = 1;
        extern "C" {
            fn renameat2(
                fromfd: c_int,
                from: *const c_char,
                tofd: c_int,
                to: *const c_char,
                flags: c_uint,
            ) -> c_int;
        }
        let (f, t) = (component(from)?, component(to)?);
        // SAFETY: as in the macOS `move_new`.
        check(unsafe {
            renameat2(
                self.file.as_raw_fd(),
                f.as_ptr(),
                dest.file.as_raw_fd(),
                t.as_ptr(),
                RENAME_NOREPLACE,
            )
        })?;
        let _ = dest.file.sync_all();
        Ok(())
    }
}

/// Why root may not create a name in `dir` and then reopen it: another
/// account could swap the name between the two, and root would act on
/// whatever inode it found. `None` when it is safe. Pure, for the tests.
///
/// The accounts this guards against are faber's own (ADR-0025), which own
/// the directories they can write (`_faber`'s state directory, `_faber_vm`'s
/// home): so `dir` must be this process's own. A world-writable `dir` must be
/// sticky (`/Library/Caches` is 1777): there nobody else can rename or remove
/// what root just made. A group-writable one is accepted (Ubuntu's `/var/log`
/// is `root:syslog` 0775) unless its group is one of `role_gids`, the role
/// accounts' own: whoever is in that group could swap names under root, so a
/// `root:_faber 0770` parent is refused.
pub fn caller_only_problem(
    path: &Path,
    uid: u32,
    gid: u32,
    mode: u32,
    euid: u32,
    role_gids: &[u32],
) -> Option<String> {
    const STICKY: u32 = 0o1000;
    let shown = path.display();
    if uid != euid {
        Some(format!(
            "{shown} is owned by uid {uid}, which can swap any name in it"
        ))
    } else if mode & 0o002 != 0 && mode & STICKY == 0 {
        Some(format!(
            "{shown} is world-writable and not sticky (mode {:o})",
            mode & 0o7777
        ))
    } else if mode & 0o020 != 0 && role_gids.contains(&gid) {
        Some(format!(
            "{shown} is group-writable (mode {:o}) by gid {gid}, a faber role account's group, \
             which can swap any name in it",
            mode & 0o7777
        ))
    } else {
        None
    }
}

/// The role accounts' gids, set once setup has planned them ([`set_role_gids`]).
static ROLE_GIDS: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

/// Tell the create-then-reopen guard which gids are `_faber`'s and
/// `_faber_vm`'s, planned or existing. Called before any directory is made.
pub fn set_role_gids(gids: &[u32]) {
    *ROLE_GIDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = gids.to_vec();
}

fn caller_only(dir: &Dir) -> io::Result<()> {
    let meta = dir.metadata()?;
    match caller_only_problem(
        dir.path(),
        meta.uid(),
        meta.gid(),
        meta.mode(),
        crate::platform::record::current_euid(),
        &ROLE_GIDS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    ) {
        None => Ok(()),
        Some(problem) => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{problem}: setup will not create and reopen a name there; this is a bug in \
                 faber, not something to fix on the host"
            ),
        )),
    }
}

/// Root's own empty directory, made fresh at mode 0700 in a directory only
/// root can write, for building something before it is moved into a directory
/// another account can write ([`place_new_dir`]). Removed when dropped, if it
/// is empty again by then.
#[derive(Debug)]
pub struct Staging {
    above: Dir,
    name: String,
    dir: Dir,
}

impl Staging {
    /// A fresh staging directory beside `path`, in its parent: on the same
    /// volume, so a move out of it is a rename. The parent must be root's
    /// alone ([`caller_only_problem`]).
    pub fn beside(path: &Path) -> io::Result<Staging> {
        let (parent, _) = split(path)?;
        Self::new_in(Dir::open(parent)?)
    }

    /// A fresh staging directory in `above`, which must be root's alone.
    pub fn new_in(above: Dir) -> io::Result<Staging> {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        caller_only(&above)?;
        for _ in 0..64 {
            let name = format!(
                ".faber-setup-staging-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            );
            if above.mkdir(&name)? {
                // `above` is root's alone: the name is the directory just made.
                let dir = above.subdir(&name)?;
                return Ok(Staging { above, name, dir });
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("no free staging name in {}", above.path().display()),
        ))
    }

    pub fn dir(&self) -> &Dir {
        &self.dir
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = self.above.rmdir(&self.name);
    }
}

/// Make the directory `name` in `dest`, a directory another account can
/// write, as `owner`'s at `mode`, without root ever reopening a name in
/// `dest` (#228): it is made in root's own `staging`, given away through its
/// own descriptor there (on macOS stripped of any ACL it inherited first), and
/// moved in with [`Dir::move_new`], which fails rather than replace anything
/// `dest`'s owner put at `name` meanwhile. Returns the directory root made,
/// still held by the descriptor it was made through.
pub fn place_new_dir(
    staging: &Staging,
    dest: &Dir,
    name: &str,
    owner: Account,
    mode: u32,
) -> io::Result<Dir> {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    component(name)?;
    let tmp = format!("{name}.{}", SEQ.fetch_add(1, Ordering::Relaxed));
    if !staging.dir.mkdir(&tmp)? {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!(
                "{} is already in root's staging",
                staging.dir.path().join(&tmp).display()
            ),
        ));
    }
    // The staging directory is root's alone, so `tmp` is the directory just made.
    let dir = staging.dir.subdir(&tmp)?;
    let placed = (|| {
        #[cfg(target_os = "macos")]
        acl::strip(&dir)?;
        dir.set_owner_mode(owner, mode)?;
        staging.dir.move_new(&tmp, dest, name)
    })();
    if let Err(e) = placed {
        let _ = staging.dir.rmdir(&tmp);
        let shown = dest.path().join(name);
        return Err(match e.kind() {
            io::ErrorKind::AlreadyExists => io::Error::new(
                e.kind(),
                format!(
                    "{} appeared while setup was making it; move it aside and re-run",
                    shown.display()
                ),
            ),
            _ => io::Error::new(e.kind(), format!("moving into {}: {e}", shown.display())),
        });
    }
    Ok(Dir {
        file: dir.file,
        path: dest.path.join(name),
    })
}

/// [`ensure_dir`] for a `dest` another account can write, where root may
/// create nothing and reopen it. A directory already at `name` must be
/// `owner`'s own (by uid): root's is refused as well, since the account could
/// have moved one of root's there, and root would give it away. That one has
/// its own owner and mode set through the descriptor just judged, which is
/// nothing the account could not do to its own directory. A missing one is
/// [`place_new_dir`]'d. Returns the open directory and whether it was created.
pub fn ensure_dir_beneath(
    staging: &Staging,
    dest: &Dir,
    name: &str,
    owner: Account,
    mode: u32,
) -> io::Result<(Dir, bool)> {
    let path = dest.path().join(name);
    match dest.subdir(name) {
        Ok(dir) => {
            let uid = dir.metadata()?.uid();
            if uid != owner.uid {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    format!(
                        "{} already exists, owned by uid {uid}, not uid {}: in a directory \
                         another account can write, setup takes over only that account's own \
                         directory, since a root-owned one may have been moved there. If \
                         nothing in it is yours, remove it (`sudo rm -rf \"{}\"`) and re-run",
                        path.display(),
                        owner.uid,
                        path.display()
                    ),
                ));
            }
            dir.set_owner_mode(owner, mode)?;
            Ok((dir, false))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Ok((place_new_dir(staging, dest, name, owner, mode)?, true))
        }
        Err(e) => Err(io::Error::new(
            e.kind(),
            format!(
                "{} is not a real directory ({e}); move it aside and re-run",
                path.display()
            ),
        )),
    }
}

/// Why a file root just created in its own staging directory may not be given
/// away through its descriptor: it must be a regular file, still root's, with
/// exactly one link. Anything else is not the file root made, and a `fchown`
/// on it would hand some other inode to the account.
pub fn fresh_root_file_problem(path: &Path, is_file: bool, uid: u32, nlink: u64) -> Option<String> {
    append_problem(path, is_file, uid, nlink, ROOT.uid)
}

/// Clone the file open at `src` as `name` in `dir` (macOS `fclonefileat`; APFS
/// shares the blocks, so a sparse disk stays sparse). Fails with `EEXIST`
/// rather than replacing anything, and with `EXDEV` or `ENOTSUP` where the
/// volume cannot clone: what to do then is the caller's.
///
/// The clone belongs to the caller, never to the source's owner
/// (`CLONE_NOOWNERCOPY`: without it a clone root makes of `_faber_vm`'s file
/// stays `_faber_vm`'s). It does carry the source's ACL and extended
/// attributes, which [`acl::strip`] clears; `dir` must be root's own, since
/// the new file is reopened by name afterwards.
#[cfg(target_os = "macos")]
pub fn clone_file(src: &File, dir: &Dir, name: &str) -> io::Result<()> {
    extern "C" {
        fn fclonefileat(srcfd: c_int, dst_dirfd: c_int, dst: *const c_char, flags: c_uint)
            -> c_int;
    }
    /// `<sys/clonefile.h>`.
    const CLONE_NOFOLLOW: c_uint = 0x0001;
    const CLONE_NOOWNERCOPY: c_uint = 0x0002;
    let c = component(name)?;
    // SAFETY: both descriptors are open for the call and `c` is
    // NUL-terminated and outlives it; the call keeps nothing.
    check(unsafe {
        fclonefileat(
            src.as_raw_fd(),
            dir.as_raw_fd(),
            c.as_ptr(),
            CLONE_NOFOLLOW | CLONE_NOOWNERCOPY,
        )
    })
    .map(|_| ())
}

/// Extended ACLs and extended attributes, through a descriptor (macOS).
///
/// `fclonefileat` and `tar` carry both over from their source, so a file
/// another account wrote, cloned into root's staging, would keep an ACL
/// entry such as `user:_faber_vm allow write` through a later `chmod 0444`.
#[cfg(target_os = "macos")]
pub mod acl {
    use std::ffi::{c_void, CStr};
    use std::io;
    use std::os::fd::AsRawFd;
    use std::os::raw::{c_char, c_int};

    /// `<sys/acl.h>`: macOS's NFSv4-style ACL, the only kind it has.
    const ACL_TYPE_EXTENDED: c_int = 0x0000_0100;
    const ACL_FIRST_ENTRY: c_int = 0;
    const ENOENT: i32 = 2;
    const ENOTSUP: i32 = 45;
    const EOPNOTSUPP: i32 = 102;
    const ENOATTR: i32 = 93;
    /// Set by the system on what some processes write, and not always
    /// removable, even by root. It grants nothing.
    pub(super) const PROVENANCE: &str = "com.apple.provenance";

    extern "C" {
        fn acl_init(count: c_int) -> *mut c_void;
        fn acl_free(obj: *mut c_void) -> c_int;
        fn acl_get_fd_np(fd: c_int, kind: c_int) -> *mut c_void;
        fn acl_set_fd_np(fd: c_int, acl: *mut c_void, kind: c_int) -> c_int;
        fn acl_get_entry(acl: *mut c_void, entry_id: c_int, entry: *mut *mut c_void) -> c_int;
        fn flistxattr(fd: c_int, names: *mut c_char, size: usize, options: c_int) -> isize;
        fn fremovexattr(fd: c_int, name: *const c_char, options: c_int) -> c_int;
    }

    fn unsupported(e: &io::Error) -> bool {
        matches!(e.raw_os_error(), Some(ENOTSUP | EOPNOTSUPP))
    }

    /// Whether the file open at `file` carries an extended ACL with at least
    /// one entry. A volume without ACLs has none.
    pub fn has_extended(file: &impl AsRawFd) -> io::Result<bool> {
        // SAFETY: the descriptor is open for the call; the call returns a new
        // ACL the caller frees, or NULL.
        let acl = unsafe { acl_get_fd_np(file.as_raw_fd(), ACL_TYPE_EXTENDED) };
        if acl.is_null() {
            let e = io::Error::last_os_error();
            return if e.raw_os_error() == Some(ENOENT) || unsupported(&e) {
                Ok(false)
            } else {
                Err(e)
            };
        }
        let mut entry: *mut c_void = std::ptr::null_mut();
        // SAFETY: `acl` is the live ACL from above, and `entry` a valid out
        // pointer; the entry points into `acl`, which is freed after.
        let rc = unsafe { acl_get_entry(acl, ACL_FIRST_ENTRY, &mut entry) };
        // SAFETY: `acl` came from `acl_get_fd_np` and is freed once.
        unsafe { acl_free(acl) };
        Ok(rc == 0)
    }

    /// Clear the extended ACL of the file (or directory) open at `file`, and
    /// remove every extended attribute it carries, through the descriptor.
    pub fn strip(file: &impl AsRawFd) -> io::Result<()> {
        let fd = file.as_raw_fd();
        // SAFETY: `acl_init` returns a new, empty ACL or NULL.
        let empty = unsafe { acl_init(0) };
        if empty.is_null() {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the descriptor is open and `empty` is live for the call.
        let rc = unsafe { acl_set_fd_np(fd, empty, ACL_TYPE_EXTENDED) };
        let set = (rc != 0).then(io::Error::last_os_error);
        // SAFETY: `empty` came from `acl_init` and is freed once.
        unsafe { acl_free(empty) };
        if let Some(e) = set.filter(|e| !unsupported(e)) {
            return Err(e);
        }
        if has_extended(file)? {
            return Err(io::Error::other("the ACL is still there after clearing it"));
        }
        for name in xattr_names(fd)? {
            // SAFETY: the descriptor is open and `name` is NUL-terminated.
            if unsafe { fremovexattr(fd, name.as_ptr(), 0) } != 0 {
                let e = io::Error::last_os_error();
                let gone = e.raw_os_error() == Some(ENOATTR);
                if !gone && name.to_bytes() != PROVENANCE.as_bytes() {
                    return Err(io::Error::new(
                        e.kind(),
                        format!("removing the attribute {name:?}: {e}"),
                    ));
                }
            }
        }
        Ok(())
    }

    /// The names of the extended attributes on `fd`.
    pub(in crate::setup) fn xattr_names(fd: c_int) -> io::Result<Vec<std::ffi::CString>> {
        loop {
            // SAFETY: a NULL buffer of size 0 asks only for the size.
            let size = unsafe { flistxattr(fd, std::ptr::null_mut(), 0, 0) };
            if size < 0 {
                let e = io::Error::last_os_error();
                return if unsupported(&e) {
                    Ok(Vec::new())
                } else {
                    Err(e)
                };
            }
            if size == 0 {
                return Ok(Vec::new());
            }
            let mut buf = vec![0u8; size as usize];
            // SAFETY: `buf` is writable for `buf.len()` bytes for the call.
            let got = unsafe { flistxattr(fd, buf.as_mut_ptr().cast(), buf.len(), 0) };
            if got < 0 {
                let e = io::Error::last_os_error();
                // ERANGE: an attribute appeared between the two calls.
                if e.raw_os_error() == Some(34) {
                    continue;
                }
                return Err(e);
            }
            buf.truncate(got as usize);
            return Ok(buf
                .split_inclusive(|&b| b == 0)
                .filter_map(|n| CStr::from_bytes_with_nul(n).ok())
                .filter(|n| !n.is_empty())
                .map(CStr::to_owned)
                .collect());
        }
    }
}

/// The descriptor, for `*at` calls a caller declares itself (macOS's
/// `fclonefileat`). It stays owned by the [`Dir`].
impl AsRawFd for Dir {
    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        self.file.as_raw_fd()
    }
}

/// The refusal for something already at a path setup owns, created by an
/// account that is neither root nor the one setup gives it to. `remove` is the
/// command that clears it.
fn planted(path: &Path, uid: u32, remove: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!(
            "{} already exists, owned by uid {uid}: neither root nor the account setup gives \
             it to, so something else put it there. If nothing in it is yours, remove it \
             (`sudo {remove} \"{}\"`) and re-run",
            path.display(),
            path.display()
        ),
    )
}

/// Create the directory `name` in `parent` if it is missing, refuse a symlink
/// or anything else in its place, and set its own owner and mode. A directory
/// that was already there must be owned by one of `prior` (root, or the account
/// it belongs to): anything else was planted, and is refused rather than
/// adopted. Returns the open directory and whether it was created. Never
/// recursive (ADR-0025).
///
/// It creates `name` and then reopens it, so `parent` must be this process's
/// alone ([`caller_only_problem`]), or it refuses: in a directory another
/// account can write, use [`ensure_dir_beneath`].
pub fn ensure_dir(
    parent: &Dir,
    name: &str,
    owner: Account,
    mode: u32,
    prior: &[u32],
) -> io::Result<(Dir, bool)> {
    caller_only(parent)?;
    let created = parent.mkdir(name)?;
    let path = parent.path().join(name);
    let dir = parent.subdir(name).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!(
                "{} is not a real directory ({e}); move it aside and re-run",
                path.display()
            ),
        )
    })?;
    if !created {
        let uid = dir.metadata()?.uid();
        if !prior.contains(&uid) {
            return Err(planted(&path, uid, "rm -rf"));
        }
    }
    dir.set_owner_mode(owner, mode)?;
    Ok((dir, created))
}

/// Why the file at `path` is not one root may append to on `owner`'s behalf:
/// it must be a regular file, owned by `owner`, with exactly one link.
pub fn append_problem(
    path: &Path,
    is_file: bool,
    uid: u32,
    nlink: u64,
    owner: u32,
) -> Option<String> {
    let shown = path.display();
    if !is_file {
        Some(format!("{shown} is not a regular file"))
    } else if uid != owner {
        Some(format!("{shown} is owned by uid {uid}, not uid {owner}"))
    } else if nlink != 1 {
        Some(format!(
            "{shown} has {nlink} links: another name for it could be a file root owns"
        ))
    } else {
        None
    }
}

/// Why a directory on the way to a file root runs is not fit for it: it must
/// be a real directory, owned by root, and writable by nobody else. Otherwise
/// any process running as its owner, or in its group, could replace what root
/// later executes. `mode` is the full `st_mode`.
pub fn root_dir_problem(path: &Path, is_dir: bool, uid: u32, mode: u32) -> Option<String> {
    let shown = path.display();
    if !is_dir {
        Some(format!(
            "{shown} is not a real directory (a symlink or a file)"
        ))
    } else if uid != 0 {
        Some(format!("{shown} is owned by uid {uid}, not root"))
    } else if mode & 0o022 != 0 {
        Some(format!(
            "{shown} is group- or world-writable (mode {:o})",
            mode & 0o7777
        ))
    } else {
        None
    }
}

/// Open the directory `path` for root to install into, walking every
/// component from `/` without following a symlink: each one that exists must
/// pass [`root_dir_problem`], and each one that is missing is created
/// root-owned, mode 0755.
pub fn root_dir_chain(path: &Path) -> io::Result<Dir> {
    walk_root_chain(path, true).map(|d| d.expect("a created chain is never missing"))
}

/// [`root_dir_chain`] for removing: the same walk and the same refusals, but
/// nothing is created. `Ok(None)` when a component is missing.
pub fn root_dir_chain_existing(path: &Path) -> io::Result<Option<Dir>> {
    walk_root_chain(path, false)
}

fn walk_root_chain(path: &Path, create: bool) -> io::Result<Option<Dir>> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not absolute", path.display()),
        ));
    }
    let mut dir = Dir::open_nofollow(Path::new("/"))?;
    for part in path.components().skip(1) {
        let name = part.as_os_str().to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not UTF-8", path.display()),
            )
        })?;
        let created = create && dir.mkdir(name)?;
        let next = match dir.subdir(name) {
            Err(e) if !create && e.kind() == io::ErrorKind::NotFound => return Ok(None),
            other => other,
        };
        let next = next.map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "{} is not a real directory ({e})",
                    dir.path().join(name).display()
                ),
            )
        })?;
        if created {
            next.set_owner_mode(ROOT, 0o755)?;
        }
        let meta = next.metadata()?;
        if let Some(problem) = root_dir_problem(next.path(), meta.is_dir(), meta.uid(), meta.mode())
        {
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, problem));
        }
        dir = next;
    }
    Ok(Some(dir))
}

/// Open the directory at `path` through its parent: the parent may be reached
/// through symlinks (it is a system directory), the directory itself may not.
pub fn open_leaf_dir(path: &Path) -> io::Result<Dir> {
    let (parent, name) = split(path)?;
    Dir::open(parent)?.subdir(name)
}

/// `path`'s parent and its last component, which must be valid UTF-8.
pub fn split(path: &Path) -> io::Result<(&Path, &str)> {
    let bad = || {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} has no parent directory or file name", path.display()),
        )
    };
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(bad)?;
    let name = path.file_name().and_then(|n| n.to_str()).ok_or_else(bad)?;
    Ok((parent, name))
}

/// Why a source file was not read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceRefusal {
    Missing,
    Symlink,
    NotRegular,
    WrongOwner { uid: u32 },
    Unreadable(String),
}

impl std::fmt::Display for SourceRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SourceRefusal::Missing => write!(f, "is not there"),
            SourceRefusal::Symlink => {
                write!(f, "is a symlink, and setup copies regular files only")
            }
            SourceRefusal::NotRegular => write!(f, "is not a regular file"),
            SourceRefusal::WrongOwner { uid } => {
                write!(f, "is owned by uid {uid}, not its account")
            }
            SourceRefusal::Unreadable(e) => write!(f, "could not be read: {e}"),
        }
    }
}

/// Open the regular file `name` in `dir` for reading: never through a symlink,
/// never anything but a regular file, and only if `owner` owns it. Owning the
/// file is what makes copying it safe as root: whatever path tricks got here,
/// root only ever reads a file the account it is migrating could read and
/// copy itself.
pub fn open_source(dir: &Dir, name: &str, owner: u32) -> Result<(File, Metadata), SourceRefusal> {
    let file = match dir.open_file(name) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(SourceRefusal::Missing),
        Err(e) => {
            // ELOOP from O_NOFOLLOW on a symlink. Confirm rather than decode
            // the errno, which differs by OS.
            let full = dir.path().join(name);
            if std::fs::symlink_metadata(&full).is_ok_and(|m| m.file_type().is_symlink()) {
                return Err(SourceRefusal::Symlink);
            }
            return Err(SourceRefusal::Unreadable(e.to_string()));
        }
    };
    let meta = file
        .metadata()
        .map_err(|e| SourceRefusal::Unreadable(e.to_string()))?;
    if !meta.file_type().is_file() {
        return Err(SourceRefusal::NotRegular);
    }
    if meta.uid() != owner {
        return Err(SourceRefusal::WrongOwner { uid: meta.uid() });
    }
    Ok((file, meta))
}

/// The SHA-256 of everything `src` yields, in hex.
pub fn sha256_of(src: &mut dyn Read) -> io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// The most setup reads of a binary. A faber build is tens of megabytes; a
/// planted file of any size must not stall setup.
pub const MAX_BINARY: u64 = 1 << 30;

/// What reading a binary's bytes tells setup, without ever running it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDigest {
    pub sha256: String,
    /// The version in its [`super::marker::MARKER`], if it has one: an
    /// untrusted hint, already validated and sanitized.
    pub version: Option<String>,
}

/// The SHA-256 of the regular file at `path`, and the version its marker
/// claims, read without following a symlink in its last component or in its
/// directory's: `_faber` owns the daemon copy's directory, and root reads that
/// copy. At most [`MAX_BINARY`] bytes. `Ok(None)` when nothing is there.
pub fn file_digest(path: &Path) -> io::Result<Option<FileDigest>> {
    let (parent, name) = split(path)?;
    let dir = match open_leaf_dir(parent) {
        Ok(dir) => dir,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let file = match dir.open_file(name) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let meta = file.metadata()?;
    if !meta.file_type().is_file() {
        return Err(io::Error::other(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    if meta.len() > MAX_BINARY {
        return Err(io::Error::other(format!(
            "{} is {} bytes, more than a faber binary could be",
            path.display(),
            meta.len()
        )));
    }
    let mut src = file.take(MAX_BINARY);
    let mut hasher = Sha256::new();
    let mut scanner = super::marker::Scanner::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        scanner.feed(&buf[..n]);
    }
    Ok(Some(FileDigest {
        sha256: format!("{:x}", hasher.finalize()),
        version: scanner.finish(),
    }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A scratch directory, removed on drop. Never under a real SYSTEM path.
    pub(crate) struct Scratch(pub PathBuf);

    impl Scratch {
        pub(crate) fn new(name: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "fbr-setup-{name}-{}-{}",
                std::process::id(),
                SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Scratch(dir)
        }

        pub(crate) fn path(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }

        pub(crate) fn dir(&self) -> Dir {
            Dir::open_nofollow(&self.0).unwrap()
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// This process's uid and primary gid, read from a file it creates, so no
    /// test spawns `id` (issue #47: a fork holds `crate::lock`'s test locks).
    pub(crate) fn me(scratch: &Scratch) -> Account {
        let probe = scratch.path(".me");
        std::fs::write(&probe, b"").unwrap();
        let meta = std::fs::metadata(&probe).unwrap();
        std::fs::remove_file(&probe).unwrap();
        Account {
            uid: meta.uid(),
            gid: meta.gid(),
        }
    }

    #[test]
    fn nofollow_refuses_a_symlinked_directory_and_follow_accepts_it() {
        let s = Scratch::new("dirlink");
        std::fs::create_dir(s.path("real")).unwrap();
        symlink(s.path("real"), s.path("link")).unwrap();
        assert!(Dir::open_nofollow(&s.path("link")).is_err());
        assert!(Dir::open(&s.path("link")).is_ok());
        assert!(s.dir().subdir("link").is_err());
        assert!(s.dir().subdir("real").is_ok());
    }

    #[test]
    fn a_file_is_not_a_directory() {
        let s = Scratch::new("notdir");
        std::fs::write(s.path("file"), b"x").unwrap();
        assert!(s.dir().subdir("file").is_err());
        assert!(Dir::open_nofollow(&s.path("file")).is_err());
    }

    #[test]
    fn names_lists_the_same_directory_twice() {
        let s = Scratch::new("names-twice");
        std::fs::write(s.path("a"), b"x").unwrap();
        std::fs::write(s.path("b"), b"y").unwrap();
        let d = s.dir();
        let sorted = |mut v: Vec<String>| {
            v.sort();
            v
        };
        let first = sorted(d.names().unwrap());
        let second = sorted(d.names().unwrap());
        assert_eq!(first, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(first, second);
    }

    #[test]
    fn names_are_single_components() {
        let s = Scratch::new("names");
        let d = s.dir();
        for bad in ["", ".", "..", "a/b", "../x"] {
            assert!(d.exists(bad).is_err(), "{bad:?}");
            assert!(d.subdir(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn write_atomic_sets_owner_mode_and_returns_the_sha256() {
        let s = Scratch::new("write");
        let owner = me(&s);
        let d = s.dir();
        let sha = d
            .write_atomic("out", &mut &b"hello\n"[..], owner, 0o640)
            .unwrap();
        assert_eq!(
            sha,
            "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03"
        );
        let meta = std::fs::symlink_metadata(s.path("out")).unwrap();
        assert!(meta.file_type().is_file());
        assert_eq!(meta.mode() & 0o7777, 0o640);
        assert_eq!((meta.uid(), meta.gid()), (owner.uid, owner.gid));
        assert_eq!(std::fs::read(s.path("out")).unwrap(), b"hello\n");
        // No temp file is left behind.
        let names: Vec<String> = std::fs::read_dir(&s.0)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["out".to_string()]);
    }

    /// Fix for a review finding: a copy whose bytes are not the ones planned
    /// is hashed and removed before the rename, so nothing lands in place: not
    /// over an existing file, not where there was none, and no temp file is
    /// left behind.
    #[test]
    fn a_copy_that_is_not_the_planned_bytes_lands_nowhere() {
        let s = Scratch::new("expect");
        let owner = me(&s);
        let d = s.dir();
        let hello = "5891b5b522d5df086d0ff0b110fbd9d21bb4fc7163af34d08286a2e846f6be03";
        let names = || {
            let mut names: Vec<String> = std::fs::read_dir(&s.0)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        };

        let err = d
            .write_atomic_expecting("bin", &mut &b"swapped\n"[..], owner, 0o755, hello)
            .unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains(hello), "{err}");
        assert!(names().is_empty(), "{:?}", names());

        std::fs::write(s.path("bin"), b"the old one").unwrap();
        d.write_atomic_expecting("bin", &mut &b"swapped\n"[..], owner, 0o755, hello)
            .unwrap_err();
        assert_eq!(std::fs::read(s.path("bin")).unwrap(), b"the old one");
        assert_eq!(names(), vec!["bin".to_string()]);

        let sha = d
            .write_atomic_expecting("bin", &mut &b"hello\n"[..], owner, 0o755, hello)
            .unwrap();
        assert_eq!(sha, hello);
        assert_eq!(std::fs::read(s.path("bin")).unwrap(), b"hello\n");
        assert_eq!(names(), vec!["bin".to_string()]);
    }

    /// A symlink planted at the destination is replaced, not written through.
    #[test]
    fn write_atomic_replaces_a_planted_symlink_instead_of_following_it() {
        let s = Scratch::new("plant");
        let owner = me(&s);
        std::fs::write(s.path("victim"), b"untouched").unwrap();
        symlink(s.path("victim"), s.path("out")).unwrap();
        s.dir()
            .write_atomic("out", &mut &b"new"[..], owner, 0o644)
            .unwrap();
        assert_eq!(std::fs::read(s.path("victim")).unwrap(), b"untouched");
        assert!(std::fs::symlink_metadata(s.path("out"))
            .unwrap()
            .file_type()
            .is_file());
    }

    #[test]
    fn exists_sees_files_dirs_and_dangling_symlinks() {
        let s = Scratch::new("exists");
        std::fs::write(s.path("f"), b"").unwrap();
        std::fs::create_dir(s.path("d")).unwrap();
        symlink(s.path("nowhere"), s.path("dangling")).unwrap();
        let d = s.dir();
        assert!(d.exists("f").unwrap());
        assert!(d.exists("d").unwrap());
        assert!(d.exists("dangling").unwrap());
        assert!(!d.exists("absent").unwrap());
    }

    #[test]
    fn ensure_dir_creates_then_leaves_alone_and_refuses_a_symlink() {
        let s = Scratch::new("ensure");
        let owner = me(&s);
        let uid = owner.uid;
        let d = s.dir();
        let (_, created) = ensure_dir(&d, "state", owner, 0o750, &[0, uid]).unwrap();
        assert!(created);
        let meta = std::fs::symlink_metadata(s.path("state")).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o750);
        let (_, created) = ensure_dir(&d, "state", owner, 0o700, &[0, uid]).unwrap();
        assert!(!created);
        let meta = std::fs::symlink_metadata(s.path("state")).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o700, "the top level is fixed");

        // A directory already there, owned by an account that is neither root
        // nor the one it is meant for, is refused, not adopted.
        let err = ensure_dir(&d, "state", owner, 0o750, &[0, uid + 1]).unwrap_err();
        assert!(err.to_string().contains("sudo rm -rf"), "{err}");
        let meta = std::fs::symlink_metadata(s.path("state")).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o700, "and left as it was");

        std::fs::create_dir(s.path("elsewhere")).unwrap();
        symlink(s.path("elsewhere"), s.path("planted")).unwrap();
        let err = ensure_dir(&d, "planted", owner, 0o750, &[0, uid]).unwrap_err();
        assert!(err.to_string().contains("not a real directory"), "{err}");
        // The link's target was never touched.
        let target = std::fs::symlink_metadata(s.path("elsewhere")).unwrap();
        assert_ne!(target.mode() & 0o7777, 0o750);
    }

    #[test]
    fn open_source_reads_only_regular_files_owned_by_the_account() {
        let s = Scratch::new("source");
        let uid = me(&s).uid;
        std::fs::write(s.path("config.toml"), b"topic = \"faber\"\n").unwrap();
        symlink(s.path("config.toml"), s.path("linked.toml")).unwrap();
        std::fs::create_dir(s.path("adir")).unwrap();
        let d = s.dir();
        assert!(open_source(&d, "config.toml", uid).is_ok());
        assert_eq!(
            open_source(&d, "linked.toml", uid).unwrap_err(),
            SourceRefusal::Symlink
        );
        assert_eq!(
            open_source(&d, "adir", uid).unwrap_err(),
            SourceRefusal::NotRegular
        );
        assert_eq!(
            open_source(&d, "absent", uid).unwrap_err(),
            SourceRefusal::Missing
        );
        assert_eq!(
            open_source(&d, "config.toml", uid + 1).unwrap_err(),
            SourceRefusal::WrongOwner { uid }
        );
    }

    /// A staged file lands under its final name with the owner and mode given,
    /// byte for byte, replacing what was there; a symlink is never promoted.
    #[test]
    fn promote_installs_the_staged_file_in_place() {
        let s = Scratch::new("promote");
        let owner = me(&s);
        let d = s.dir();
        std::fs::write(s.path("faber"), b"old").unwrap();
        d.write_atomic(".stage", &mut &b"new bytes"[..], owner, 0o600)
            .unwrap();
        d.promote(".stage", "faber", owner, 0o755).unwrap();
        assert_eq!(std::fs::read(s.path("faber")).unwrap(), b"new bytes");
        let meta = std::fs::symlink_metadata(s.path("faber")).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o755);
        assert!(!s.path(".stage").exists());

        std::fs::write(s.path("victim"), b"x").unwrap();
        symlink(s.path("victim"), s.path(".link")).unwrap();
        assert!(d.promote(".link", "faber", owner, 0o755).is_err());
        assert_eq!(std::fs::read(s.path("faber")).unwrap(), b"new bytes");
    }

    #[test]
    fn rename_and_unlink_stay_inside_the_directory() {
        let s = Scratch::new("rename");
        std::fs::write(s.path("a"), b"1").unwrap();
        let d = s.dir();
        d.rename("a", "b").unwrap();
        assert!(!s.path("a").exists());
        assert_eq!(std::fs::read(s.path("b")).unwrap(), b"1");
        assert!(d.rename("b", "../escape").is_err());
        d.unlink("b").unwrap();
        assert!(!s.path("b").exists());
    }

    #[test]
    fn file_digest_hashes_or_reports_absence() {
        let s = Scratch::new("sha");
        std::fs::write(s.path("f"), b"").unwrap();
        assert_eq!(
            file_digest(&s.path("f")).unwrap(),
            Some(FileDigest {
                sha256: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855".into(),
                version: None,
            })
        );
        assert_eq!(file_digest(&s.path("absent")).unwrap(), None);
        assert_eq!(file_digest(&s.path("nodir/f")).unwrap(), None);
    }

    /// The version comes from the bytes, never from running the file: a
    /// "binary" that is only a marker still reports its version.
    #[test]
    fn file_digest_reads_the_marker_without_running_anything() {
        let s = Scratch::new("marker");
        let mut bytes = b"#!/bin/sh\nexit 1\n".to_vec();
        bytes.extend(crate::setup::marker::needle());
        bytes.extend_from_slice(b"9.9.9+gplanted\0");
        std::fs::write(s.path("faber"), &bytes).unwrap();
        let d = file_digest(&s.path("faber")).unwrap().unwrap();
        assert_eq!(d.version.as_deref(), Some("9.9.9+gplanted"));
    }

    #[test]
    fn ensure_file_creates_takes_over_and_refuses() {
        let s = Scratch::new("ensure-file");
        let owner = me(&s);
        let uid = owner.uid;
        let d = s.dir();
        assert!(d
            .ensure_file("stderr.log", owner, 0o640, &[0, uid])
            .unwrap());
        let meta = std::fs::symlink_metadata(s.path("stderr.log")).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o640);
        std::fs::write(s.path("stderr.log"), b"kept").unwrap();
        assert!(!d
            .ensure_file("stderr.log", owner, 0o600, &[0, uid])
            .unwrap());
        assert_eq!(std::fs::read(s.path("stderr.log")).unwrap(), b"kept");

        // Owned by an account that is neither root nor the intended one.
        let err = d
            .ensure_file("stderr.log", owner, 0o640, &[0, uid + 1])
            .unwrap_err();
        assert!(err.to_string().contains("sudo rm"), "{err}");

        std::fs::write(s.path("victim"), b"x").unwrap();
        symlink(s.path("victim"), s.path("stdout.log")).unwrap();
        let err = d
            .ensure_file("stdout.log", owner, 0o640, &[0, uid])
            .unwrap_err();
        assert!(err.to_string().contains("not a regular file"), "{err}");
    }

    /// The one move-aside helper: renames in place, and never overwrites an
    /// existing backup.
    #[test]
    fn move_aside_never_overwrites_a_backup() {
        let s = Scratch::new("aside");
        std::fs::write(s.path("config.toml"), b"live").unwrap();
        let d = s.dir();
        d.move_aside("config.toml", "config.toml.bak").unwrap();
        assert_eq!(std::fs::read(s.path("config.toml.bak")).unwrap(), b"live");
        std::fs::write(s.path("config.toml"), b"again").unwrap();
        let err = d.move_aside("config.toml", "config.toml.bak").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(s.path("config.toml.bak")).unwrap(), b"live");
        assert_eq!(std::fs::read(s.path("config.toml")).unwrap(), b"again");
    }

    #[test]
    fn append_owned_creates_appends_and_keeps_the_owner() {
        use std::os::unix::fs::MetadataExt;
        let s = Scratch::new("append");
        let me = me(&s);
        let dir = s.dir();
        dir.append_owned("audit.jsonl", b"one\n", me, 0o644)
            .unwrap();
        dir.append_owned("audit.jsonl", b"two\n", me, 0o644)
            .unwrap();
        assert_eq!(std::fs::read(s.path("audit.jsonl")).unwrap(), b"one\ntwo\n");
        let meta = std::fs::metadata(s.path("audit.jsonl")).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o644);
        assert_eq!(meta.uid(), me.uid);
    }

    #[test]
    fn append_owned_refuses_a_symlink_a_hard_link_and_another_owner() {
        let s = Scratch::new("append-refuse");
        let me = me(&s);
        let dir = s.dir();
        std::fs::write(s.path("target"), b"root's\n").unwrap();
        symlink(s.path("target"), s.path("link.jsonl")).unwrap();
        assert!(dir.append_owned("link.jsonl", b"x\n", me, 0o644).is_err());
        std::fs::hard_link(s.path("target"), s.path("hard.jsonl")).unwrap();
        let e = dir
            .append_owned("hard.jsonl", b"x\n", me, 0o644)
            .unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::PermissionDenied);
        assert!(e.to_string().contains("2 links"), "{e}");
        let other = Account {
            uid: me.uid.wrapping_add(1),
            gid: me.gid,
        };
        std::fs::write(s.path("mine.jsonl"), b"").unwrap();
        let e = dir
            .append_owned("mine.jsonl", b"x\n", other, 0o644)
            .unwrap_err();
        assert!(e.to_string().contains("not uid"), "{e}");
        std::fs::create_dir(s.path("d.jsonl")).unwrap();
        assert!(dir.append_owned("d.jsonl", b"x\n", me, 0o644).is_err());
        // Nothing reached the file behind the links.
        assert_eq!(std::fs::read(s.path("target")).unwrap(), b"root's\n");
    }

    #[test]
    fn append_problem_truth_table() {
        let p = Path::new("/x/audit.jsonl");
        assert_eq!(append_problem(p, true, 7, 1, 7), None);
        assert!(append_problem(p, false, 7, 1, 7).is_some());
        assert!(append_problem(p, true, 0, 1, 7).is_some());
        assert!(append_problem(p, true, 7, 2, 7).is_some());
        assert!(append_problem(p, true, 7, 0, 7).is_some());
    }

    #[test]
    fn rmdir_removes_only_an_empty_directory() {
        let s = Scratch::new("rmdir");
        std::fs::create_dir(s.path("empty")).unwrap();
        std::fs::create_dir(s.path("full")).unwrap();
        std::fs::write(s.path("full/f"), b"").unwrap();
        let d = s.dir();
        d.rmdir("empty").unwrap();
        assert!(!s.path("empty").exists());
        assert!(d.rmdir("full").is_err());
        assert!(s.path("full/f").exists());
    }

    /// A directory root installs into must be a real directory, root's, and
    /// writable by nobody else, all the way up.
    #[test]
    fn root_dir_problem_truth_table() {
        let p = Path::new("/usr/local/bin");
        assert_eq!(root_dir_problem(p, true, 0, 0o040_755), None);
        assert_eq!(root_dir_problem(p, true, 0, 0o040_711), None);
        assert!(root_dir_problem(p, true, 501, 0o040_755)
            .unwrap()
            .contains("uid 501"));
        assert!(root_dir_problem(p, true, 0, 0o040_775)
            .unwrap()
            .contains("group- or world-writable"));
        assert!(root_dir_problem(p, true, 0, 0o041_777).is_some());
        assert!(root_dir_problem(p, false, 0, 0o120_755)
            .unwrap()
            .contains("not a real directory"));
    }

    /// Walking a chain the test account owns fails at its first component
    /// that is not root's, and creates nothing past it. Run as root, the
    /// scratch tree is root's, and the walk succeeds instead.
    #[test]
    fn root_dir_chain_refuses_a_directory_root_does_not_own() {
        let s = Scratch::new("chain");
        // Canonical, so the walk meets no system symlink such as macOS's
        // `/var` on the way to the scratch directory.
        let target = std::fs::canonicalize(&s.0).unwrap().join("a/b");
        match root_dir_chain(&target) {
            Ok(dir) => {
                assert_eq!(me(&s).uid, 0, "only root owns the scratch chain");
                assert_eq!(dir.path(), target);
            }
            Err(e) => {
                assert_ne!(me(&s).uid, 0);
                assert_eq!(e.kind(), io::ErrorKind::PermissionDenied, "{e}");
                assert!(
                    !target.parent().unwrap().exists(),
                    "nothing is created below it"
                );
            }
        }
        assert!(root_dir_chain(Path::new("relative/x")).is_err());
    }

    /// The verdict root acts on before it gives a file it just made away
    /// through the descriptor: a regular file, root's, with one link.
    #[test]
    fn fresh_root_file_problem_truth_table() {
        let p = Path::new("/x/disk.raw");
        assert_eq!(fresh_root_file_problem(p, true, 0, 1), None);
        assert!(fresh_root_file_problem(p, false, 0, 1).is_some());
        // Not root's: some other account's file sits under that name.
        assert!(fresh_root_file_problem(p, true, 501, 1).is_some());
        // Another name for it may be a system file.
        assert!(fresh_root_file_problem(p, true, 0, 2).is_some());
        assert!(fresh_root_file_problem(p, true, 0, 0).is_some());
    }

    /// The guard on create-then-reopen: only a directory this process's own
    /// account owns, and nobody else can write.
    #[test]
    fn caller_only_judges_owner_and_mode() {
        let p = Path::new("/x");
        assert!(caller_only_problem(p, 0, 0, 0o040_755, 0, &[]).is_none());
        assert!(caller_only_problem(p, 0, 0, 0o040_700, 0, &[]).is_none());
        assert!(caller_only_problem(p, 451, 0, 0o040_700, 0, &[])
            .unwrap()
            .contains("uid 451"));
        assert!(caller_only_problem(p, 0, 0, 0o040_777, 0, &[])
            .unwrap()
            .contains("not sticky"));
        // Sticky and world-writable (`/Library/Caches`): only root can rename
        // or remove root's entry in it.
        assert!(caller_only_problem(p, 0, 0, 0o041_777, 0, &[]).is_none());
        // Group-writable by a host group (Ubuntu's `/var/log`).
        assert!(caller_only_problem(p, 0, 0, 0o040_775, 0, &[]).is_none());
        // But not when the group is a role account's: `root:_faber 0770`.
        assert!(caller_only_problem(p, 0, 451, 0o040_770, 0, &[450, 451])
            .unwrap()
            .contains("role account"));
        assert!(caller_only_problem(p, 0, 451, 0o040_755, 0, &[451]).is_none());
        assert!(caller_only_problem(p, 0, 133, 0o040_775, 0, &[450, 451]).is_none());
    }

    /// The guard passes the real parent of every top-level SYSTEM directory
    /// on this host, as root would see it: a guard that refuses a real setup
    /// is a dead setup (a review finding: `/Library/Caches` is 1777).
    #[test]
    fn the_guard_passes_every_real_system_parent() {
        use crate::platform::layout::{Os, SystemLayout};
        let layout = SystemLayout::for_os(Os::HOST);
        for spec in crate::setup::plan::dir_specs(Os::HOST, &layout) {
            let parent = if spec.in_state_dir {
                continue;
            } else {
                spec.path.parent().unwrap().to_path_buf()
            };
            // Resolved as `Dir::open` resolves it (`/etc` is a symlink on macOS).
            let Ok(meta) = std::fs::metadata(&parent) else {
                continue;
            };
            if meta.uid() != 0 {
                continue;
            }
            assert_eq!(
                caller_only_problem(&parent, meta.uid(), meta.gid(), meta.mode(), 0, &[]),
                None,
                "{}",
                parent.display()
            );
        }
        // And beside the state directory and the VM home, where staging goes.
        for path in [&layout.state_dir, &layout.vm_home] {
            let parent = path.parent().unwrap();
            if let Ok(meta) = std::fs::metadata(parent) {
                if meta.uid() == 0 {
                    assert_eq!(
                        caller_only_problem(parent, 0, meta.gid(), meta.mode(), 0, &[]),
                        None,
                        "{}",
                        parent.display()
                    );
                }
            }
        }
    }

    /// `ensure_dir` creates and reopens, so it refuses a parent anyone can
    /// write and that is not sticky, before creating anything.
    #[test]
    fn ensure_dir_refuses_a_parent_others_can_write() {
        let s = Scratch::new("ensure-guard");
        let owner = me(&s);
        std::fs::create_dir(s.path("theirs")).unwrap();
        std::fs::set_permissions(s.path("theirs"), Permissions::from_mode(0o777)).unwrap();
        let theirs = Dir::open_nofollow(&s.path("theirs")).unwrap();
        let err = ensure_dir(&theirs, "bin", owner, 0o755, &[owner.uid]).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("not sticky"), "{err}");
        assert!(!s.path("theirs/bin").exists(), "nothing was created");
        // Sticky: allowed, as `/Library/Caches` must be.
        std::fs::set_permissions(s.path("theirs"), Permissions::from_mode(0o1777)).unwrap();
        let theirs = Dir::open_nofollow(&s.path("theirs")).unwrap();
        ensure_dir(&theirs, "bin", owner, 0o755, &[owner.uid]).unwrap();
    }

    /// Staging is made 0700 in a directory only this account can write, is
    /// refused anywhere else, and goes away once empty.
    #[test]
    fn staging_is_private_and_cleans_up() {
        let s = Scratch::new("staging");
        std::fs::create_dir(s.path("state")).unwrap();
        let staging = Staging::beside(&s.path("state")).unwrap();
        let path = staging.dir().path().to_path_buf();
        assert_eq!(path.parent().unwrap(), s.0.as_path());
        let meta = std::fs::symlink_metadata(&path).unwrap();
        assert!(meta.is_dir());
        assert_eq!(meta.mode() & 0o777, 0o700);
        drop(staging);
        assert!(!path.exists());

        std::fs::create_dir(s.path("open")).unwrap();
        std::fs::set_permissions(s.path("open"), Permissions::from_mode(0o777)).unwrap();
        let err = Staging::new_in(Dir::open_nofollow(&s.path("open")).unwrap()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read_dir(s.path("open")).unwrap().count(), 0);
    }

    /// #228: a directory for another account's directory is made in root's
    /// staging and moved in. Nothing is ever replaced: whatever the account
    /// put at the name first (a directory, a symlink) stays, and the staged
    /// one is removed. An existing directory is taken only if it is the
    /// owner's own, and a symlink in its place is refused.
    #[test]
    fn a_directory_beneath_another_account_is_placed_never_reopened() {
        let s = Scratch::new("beneath");
        let owner = me(&s);
        std::fs::create_dir(s.path("state")).unwrap();
        let dest = Dir::open_nofollow(&s.path("state")).unwrap();
        let staging = Staging::beside(&s.path("state")).unwrap();

        let (dir, created) = ensure_dir_beneath(&staging, &dest, "bin", owner, 0o750).unwrap();
        assert!(created);
        assert_eq!(dir.path(), s.path("state/bin"));
        let meta = std::fs::symlink_metadata(s.path("state/bin")).unwrap();
        assert!(meta.is_dir());
        assert_eq!(meta.mode() & 0o7777, 0o750);
        // The descriptor returned is the directory now at the name.
        assert_eq!(dir.metadata().unwrap().ino(), meta.ino());
        assert_eq!(std::fs::read_dir(staging.dir().path()).unwrap().count(), 0);

        // Already there and the owner's: kept, its mode set.
        let (_, created) = ensure_dir_beneath(&staging, &dest, "bin", owner, 0o700).unwrap();
        assert!(!created);
        let meta = std::fs::symlink_metadata(s.path("state/bin")).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o700);

        // Already there, but not the owner's (here: meant for another uid).
        let other = Account {
            uid: owner.uid + 1,
            gid: owner.gid,
        };
        let err = ensure_dir_beneath(&staging, &dest, "bin", other, 0o750).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("sudo rm -rf"), "{err}");
        assert_eq!(
            std::fs::symlink_metadata(s.path("state/bin"))
                .unwrap()
                .mode()
                & 0o7777,
            0o700,
            "and left as it was"
        );

        // A symlink in its place is refused, its target untouched.
        std::fs::create_dir(s.path("elsewhere")).unwrap();
        symlink(s.path("elsewhere"), s.path("state/planted")).unwrap();
        let err = ensure_dir_beneath(&staging, &dest, "planted", owner, 0o750).unwrap_err();
        assert!(err.to_string().contains("not a real directory"), "{err}");
        assert_ne!(
            std::fs::symlink_metadata(s.path("elsewhere"))
                .unwrap()
                .mode()
                & 0o7777,
            0o750
        );

        // The race #228 is about: the account puts something at the name
        // after setup looked. `place_new_dir` replaces neither a directory
        // nor a symlink, and leaves nothing behind in staging.
        std::fs::create_dir(s.path("state/raced")).unwrap();
        let err = place_new_dir(&staging, &dest, "raced", owner, 0o750).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert!(err.to_string().contains("appeared"), "{err}");
        symlink(s.path("elsewhere"), s.path("state/linked")).unwrap();
        assert!(place_new_dir(&staging, &dest, "linked", owner, 0o750).is_err());
        assert!(std::fs::symlink_metadata(s.path("state/linked"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_dir(staging.dir().path()).unwrap().count(), 0);
        assert!(place_new_dir(&staging, &dest, "../x", owner, 0o750).is_err());
    }

    #[test]
    fn move_new_moves_but_never_replaces() {
        let s = Scratch::new("move-new");
        std::fs::create_dir(s.path("from")).unwrap();
        std::fs::create_dir(s.path("to")).unwrap();
        std::fs::write(s.path("from/a"), b"a").unwrap();
        std::fs::create_dir(s.path("from/d")).unwrap();
        std::fs::write(s.path("to/b"), b"theirs").unwrap();
        symlink("/nonexistent", s.path("to/link")).unwrap();
        let (from, to) = (
            Dir::open_nofollow(&s.path("from")).unwrap(),
            Dir::open_nofollow(&s.path("to")).unwrap(),
        );
        from.move_new("a", &to, "a").unwrap();
        from.move_new("d", &to, "d").unwrap();
        assert_eq!(std::fs::read(s.path("to/a")).unwrap(), b"a");
        assert!(s.path("to/d").is_dir());
        std::fs::write(s.path("from/b"), b"mine").unwrap();
        let e = from.move_new("b", &to, "b").unwrap_err();
        assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(s.path("to/b")).unwrap(), b"theirs");
        std::fs::write(s.path("from/link"), b"mine").unwrap();
        assert!(from.move_new("link", &to, "link").is_err());
        assert!(std::fs::symlink_metadata(s.path("to/link"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(from.move_new("b", &to, "../b").is_err());
    }

    #[cfg(target_os = "macos")]
    mod plant {
        use std::ffi::c_void;
        use std::os::raw::{c_char, c_int};
        extern "C" {
            pub fn acl_from_text(text: *const c_char) -> *mut c_void;
            pub fn acl_set_fd_np(fd: c_int, acl: *mut c_void, kind: c_int) -> c_int;
            pub fn acl_free(obj: *mut c_void) -> c_int;
            pub fn fsetxattr(
                fd: c_int,
                name: *const c_char,
                value: *const c_void,
                size: usize,
                position: u32,
                options: c_int,
            ) -> c_int;
        }
    }

    /// An ACL entry and an attribute planted on a file, as `fclonefileat`
    /// would carry them from another account's file, are seen and cleared.
    #[cfg(target_os = "macos")]
    #[test]
    fn acl_strip_clears_the_acl_and_every_attribute() {
        use std::ffi::CString;
        let s = Scratch::new("acl");
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(s.path("f"))
            .unwrap();
        let fd = file.as_raw_fd();
        assert!(!acl::has_extended(&file).unwrap());
        // `everyone`'s well-known group UUID, so the parse looks nothing up.
        let text = CString::new(
            "!#acl 1\ngroup:ABCDEFAB-CDEF-ABCD-EFAB-CDEF0000000C:everyone:12:allow:write\n",
        )
        .unwrap();
        // SAFETY: test-only FFI on a descriptor and strings that outlive it.
        unsafe {
            let a = plant::acl_from_text(text.as_ptr());
            assert!(!a.is_null(), "{}", io::Error::last_os_error());
            assert_eq!(
                plant::acl_set_fd_np(fd, a, 0x100),
                0,
                "{}",
                io::Error::last_os_error()
            );
            plant::acl_free(a);
            for name in ["org.example.one", "org.example.two"] {
                let n = CString::new(name).unwrap();
                let rc = plant::fsetxattr(fd, n.as_ptr(), b"v".as_ptr().cast(), 1, 0, 0);
                assert_eq!(rc, 0, "{}", io::Error::last_os_error());
            }
        }
        assert!(acl::has_extended(&file).unwrap());
        acl::strip(&file).unwrap();
        assert!(!acl::has_extended(&file).unwrap());
        // The system's provenance tag may stay; it grants nothing.
        let left = acl::xattr_names(fd).unwrap();
        assert!(
            left.iter()
                .all(|n| n.to_bytes() == acl::PROVENANCE.as_bytes()),
            "attributes left: {left:?}"
        );
        // A file with nothing to strip is fine, as is a directory.
        acl::strip(&file).unwrap();
        acl::strip(&s.dir()).unwrap();
    }
}

/// The one clone both image imports use.
#[cfg(all(test, target_os = "macos"))]
mod clone_tests {
    use super::tests::Scratch;
    use super::*;

    /// A clone has the source's bytes under a new name, never replaces a name
    /// that is taken, and is a new inode.
    #[test]
    fn a_clone_copies_the_bytes_and_never_replaces() {
        let s = Scratch::new("clone-file");
        std::fs::write(s.path("src"), b"bytes").unwrap();
        let src = File::open(s.path("src")).unwrap();
        let dir = s.dir();
        match clone_file(&src, &dir, "dst") {
            Ok(()) => {
                assert_eq!(std::fs::read(s.path("dst")).unwrap(), b"bytes");
                let (a, b) = (
                    std::fs::metadata(s.path("src")).unwrap(),
                    std::fs::metadata(s.path("dst")).unwrap(),
                );
                assert_ne!(a.ino(), b.ino());
                let e = clone_file(&src, &dir, "dst").unwrap_err();
                assert_eq!(e.kind(), io::ErrorKind::AlreadyExists);
            }
            // A volume without clones is the caller's fallback.
            Err(e) => assert!(matches!(e.raw_os_error(), Some(18 | 45)), "{e}"),
        }
        let e = clone_file(&src, &dir, "a/b").unwrap_err();
        assert!(e.kind() != io::ErrorKind::AlreadyExists, "{e}");
    }
}
