//! The group database, read in-process with `getgrgid_r`: the members the
//! system lists for a group. On macOS that is the directory service's
//! `GroupMembership`, on Linux whatever NSS serves (`/etc/group` by default).
//!
//! It is the explicit member list, the one `dseditgroup -a` and `usermod -aG`
//! write. A user whose primary group this is, and on macOS a group nested in
//! it, can use the group too and are not listed.
//!
//! `getgrgid_r` is declared by hand, as `crate::lock` declares `flock`, so
//! faber keeps no `libc` dependency. `struct group` has the same four fields
//! in the same order on macOS and glibc.

use std::ffi::CStr;
use std::io;
use std::os::raw::{c_char, c_int};

/// `struct group`. Only the id and the members are read; the other two fields
/// hold their places.
#[repr(C)]
struct GroupEntry {
    _name: *mut c_char,
    _passwd: *mut c_char,
    gid: u32,
    members: *mut *mut c_char,
}

extern "C" {
    /// POSIX. `gid_t` is a 32-bit unsigned integer on both macOS and Linux.
    fn getgrgid_r(
        gid: u32,
        grp: *mut GroupEntry,
        buf: *mut c_char,
        buflen: usize,
        result: *mut *mut GroupEntry,
    ) -> c_int;
}

/// `ERANGE`, the buffer too small for the entry: 34 on both macOS and Linux.
const ERANGE: c_int = 34;

/// Where the lookup starts, and the most it grows to: room for a group that
/// lists thousands of members.
const FIRST_BUFFER: usize = 4096;
const MAX_BUFFER: usize = 1 << 20;

/// The members the group database lists for `gid`, or `None` when no group
/// has that id. Blocking: on macOS it asks the directory service.
pub fn members(gid: u32) -> io::Result<Option<Vec<String>>> {
    let mut size = FIRST_BUFFER;
    loop {
        let mut buf: Vec<c_char> = vec![0; size];
        let mut entry = GroupEntry {
            _name: std::ptr::null_mut(),
            _passwd: std::ptr::null_mut(),
            gid: 0,
            members: std::ptr::null_mut(),
        };
        let mut found: *mut GroupEntry = std::ptr::null_mut();
        // SAFETY: every pointer is to memory this frame owns, `buf.len()` is
        // the size of `buf`, and the call writes only within `entry`, `buf`
        // and `found`, keeping none of them.
        let rc = unsafe { getgrgid_r(gid, &mut entry, buf.as_mut_ptr(), buf.len(), &mut found) };
        if rc == ERANGE && size < MAX_BUFFER {
            size *= 2;
            continue;
        }
        if rc != 0 {
            return Err(io::Error::from_raw_os_error(rc));
        }
        if found.is_null() {
            return Ok(None);
        }
        if entry.gid != gid {
            return Err(io::Error::other(format!(
                "the group database answered gid {} for gid {gid}",
                entry.gid
            )));
        }
        let mut members = Vec::new();
        let mut at = entry.members;
        // SAFETY: on success `members` is a NULL-terminated array of
        // NUL-terminated strings, all inside `buf`, which outlives this loop.
        unsafe {
            while !at.is_null() && !(*at).is_null() {
                members.push(CStr::from_ptr(*at).to_string_lossy().into_owned());
                at = at.add(1);
            }
        }
        return Ok(Some(members));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// gid 0 is a group on every host faber runs on (`wheel`, `root`), and a
    /// gid near the top of the range is on none.
    #[test]
    fn a_group_is_found_and_a_missing_one_is_none() {
        assert!(members(0).unwrap().is_some());
        assert_eq!(members(4_000_000_001).unwrap(), None);
    }
}
