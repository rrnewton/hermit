//! libc's `siginfo_t::si_pid`, which Detcore calls, as a trait method:
//! `siginfo_t` is Reverie's type here, so an inherent method cannot be added.
//! The layout structs are libc 0.2.189's (src/unix/linux_like/linux/gnu/
//! mod.rs), with the attributes its `s_no_extra_traits!` expands to; only the
//! SIGCHLD field Detcore reads is exposed.

use super::*;

#[repr(C)]
#[derive(Clone, Copy)]
struct sifields_sigchld {
    si_pid: pid_t,
    si_uid: uid_t,
    si_status: c_int,
    si_utime: c_long,
    si_stime: c_long,
}

#[repr(C)]
#[derive(Clone, Copy)]
union sifields {
    _align_pointer: *mut c_void,
    sigchld: sifields_sigchld,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct siginfo_f {
    _siginfo_base: [c_int; 3],
    sifields: sifields,
}

/// libc's SIGCHLD accessor on `siginfo_t`.
pub trait SiginfoExt {
    /// The sending process, for SIGCHLD and `waitid`.
    ///
    /// # Safety
    /// As libc's: `self` must hold a SIGCHLD-layout union member.
    unsafe fn si_pid(&self) -> pid_t;
}

impl SiginfoExt for siginfo_t {
    unsafe fn si_pid(&self) -> pid_t {
        // SAFETY: libc's own cast; siginfo_t and siginfo_f share the 128-byte
        // prefix layout, and the caller vouches for the union member.
        unsafe {
            (*(self as *const siginfo_t).cast::<siginfo_f>())
                .sifields
                .sigchld
                .si_pid
        }
    }
}
