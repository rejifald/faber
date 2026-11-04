//! Keeping the machine from idle-sleeping while it runs jobs — issue #90.
//!
//! A host that sleeps with a job in flight loses the job: its runner stops
//! answering GitHub, which fails the job about ten minutes later. `pmset -a
//! sleep 0` (`deploy/keep-awake.sh`, `faber setup`) avoids that by never
//! sleeping at all. This is the narrower tool [`crate::awake`] uses instead: an
//! assertion held only while there is work, so an idle host still sleeps.
//!
//! # macOS: an IOKit power assertion, not a `caffeinate` child
//!
//! The assertion is `PreventUserIdleSystemSleep` taken with
//! `IOPMAssertionCreateWithName` — what `caffeinate -i` takes, and what `pmset -g
//! assertions` lists under faber's name. It is called directly rather than by
//! supervising a `caffeinate -i -w <pid>` child because the direct call has the
//! property that matters by construction: powerd owns the assertion on behalf of
//! this process and drops it when the process exits, crash and `SIGKILL`
//! included, the same way the kernel drops the host lock's `flock`
//! ([`crate::lock`]). A child is a second process to start, reap, and kill on
//! release, and one that failed to start would leave nothing visible here.
//!
//! What it does not prevent, by design of the assertion: closing a laptop's lid
//! without an external display, choosing Sleep from the Apple menu, and the
//! sleep forced by a low battery or heat. It holds off *idle* sleep only.
//!
//! # Elsewhere
//!
//! Not implemented. Linux's equivalent, `systemd-inhibit`, wraps a process rather
//! than handing out a token, and no Linux host has needed it. [`supported`] says
//! so, and [`crate::awake`] warns at boot when the key is set on such a host.

/// Whether this platform can hold the assertion at all.
pub fn supported() -> bool {
    imp::SUPPORTED
}

/// A held no-idle-sleep assertion. Dropping it releases it.
#[derive(Debug)]
pub struct Assertion {
    /// Never read: it is held only so that its drop releases the assertion.
    _handle: imp::Handle,
}

/// Hold off idle system sleep until the returned [`Assertion`] is dropped.
/// `reason` is what `pmset -g assertions` shows next to faber's pid.
pub fn prevent_idle_sleep(reason: &str) -> Result<Assertion, String> {
    imp::take(reason).map(|h| Assertion { _handle: h })
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{c_char, c_void, CString};

    pub const SUPPORTED: bool = true;

    type CFStringRef = *const c_void;

    /// `kCFStringEncodingUTF8`.
    const UTF8: u32 = 0x0800_0100;
    /// `kIOPMAssertionLevelOn`.
    const LEVEL_ON: u32 = 255;
    /// `kIOReturnSuccess`.
    const SUCCESS: i32 = 0;
    /// `kIOPMAssertionTypePreventUserIdleSystemSleep`, spelled out: the header
    /// defines it as a CFSTR macro, so there is no symbol to link against.
    const PREVENT_USER_IDLE_SYSTEM_SLEEP: &str = "PreventUserIdleSystemSleep";

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFStringCreateWithCString(
            alloc: *const c_void,
            c_str: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFRelease(cf: *const c_void);
    }

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOPMAssertionCreateWithName(
            assertion_type: CFStringRef,
            level: u32,
            name: CFStringRef,
            id: *mut u32,
        ) -> i32;
        fn IOPMAssertionRelease(id: u32) -> i32;
    }

    /// An owned CFString, released on drop.
    struct CfString(CFStringRef);

    impl CfString {
        fn new(s: &str) -> Result<Self, String> {
            let c = CString::new(s).map_err(|_| format!("{s:?} contains a NUL byte"))?;
            // SAFETY: `c` is a valid NUL-terminated string that outlives the call,
            // which copies it; a null allocator selects the default one.
            let r = unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), UTF8) };
            if r.is_null() {
                return Err(format!("could not make a CFString of {s:?}"));
            }
            Ok(Self(r))
        }
    }

    impl Drop for CfString {
        fn drop(&mut self) {
            // SAFETY: `self.0` is a non-null CFString this value created and owns.
            unsafe { CFRelease(self.0) }
        }
    }

    #[derive(Debug)]
    pub struct Handle(u32);

    pub fn take(reason: &str) -> Result<Handle, String> {
        let kind = CfString::new(PREVENT_USER_IDLE_SYSTEM_SLEEP)?;
        let name = CfString::new(reason)?;
        let mut id = 0u32;
        // SAFETY: both strings are live CFStrings for the duration of the call,
        // which retains what it keeps, and `id` is a valid place to write the id.
        let rc = unsafe { IOPMAssertionCreateWithName(kind.0, LEVEL_ON, name.0, &mut id) };
        if rc != SUCCESS {
            return Err(format!("IOPMAssertionCreateWithName returned {rc:#x}"));
        }
        Ok(Handle(id))
    }

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: `self.0` is an assertion id this process created and has not
            // released; releasing takes nothing but the id.
            let rc = unsafe { IOPMAssertionRelease(self.0) };
            if rc != SUCCESS {
                tracing::warn!(
                    "power: releasing the no-idle-sleep assertion returned {rc:#x}; it ends \
                     with this process regardless"
                );
            }
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub const SUPPORTED: bool = false;

    /// Uninhabited: nothing here can hold one.
    #[derive(Debug)]
    pub enum Handle {}

    pub fn take(_reason: &str) -> Result<Handle, String> {
        Err("holding off idle sleep is implemented on macOS only".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole FFI round trip, against the real power manager: it links, the
    /// assertion is granted, and dropping it releases it without a warning. Held
    /// for microseconds, so it cannot keep a developer's machine awake.
    #[cfg(target_os = "macos")]
    #[test]
    fn an_assertion_is_taken_and_released() {
        assert!(supported());
        let held = prevent_idle_sleep("faber test: an_assertion_is_taken_and_released")
            .expect("the power manager grants an idle-sleep assertion");
        drop(held);
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn elsewhere_it_is_refused_not_faked() {
        assert!(!supported());
        assert!(prevent_idle_sleep("faber test").is_err());
    }
}
