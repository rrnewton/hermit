//! The original scalar receive tuple, not permission to inspect or write memory.
//!
//! Keep the guest's full capacity for original-entry authentication. The bounded
//! publication maximum is a separate derived quantity and never rewrites the
//! syscall presented to the backend, the saved policy, or a retry.

use reverie::Error;
use reverie::Guest;
use reverie::OriginalReadRangeVerdict;
use reverie::Tool;
use reverie::syscalls::Read;
use reverie::syscalls::Recvfrom;
use reverie::syscalls::Syscall;
use reverie::syscalls::SyscallArgs;
use reverie::syscalls::SyscallInfo;
use reverie::syscalls::Sysno;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ScalarReceive(Original);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Original {
    Read(Read),
    Recvfrom(Recvfrom),
}

impl From<Read> for ScalarReceive {
    fn from(read: Read) -> Self {
        Self(Original::Read(read))
    }
}

impl ScalarReceive {
    pub(crate) fn from_syscall(call: Syscall) -> Result<Self, Error> {
        match call {
            Syscall::Read(read) => Ok(read.into()),
            Syscall::Recvfrom(receive) => {
                let (_, raw) = receive.into_parts();
                // Check the entire raw words, not truncated typed flags. Only
                // this shape has the backend's authenticated scalar contract.
                if raw.arg3 != 0 || raw.arg4 != 0 || raw.arg5 != 0 {
                    return Err(Error::Tool(anyhow::anyhow!(
                        "V4 scalar recvfrom requires flags=0 and no source-address outputs"
                    )));
                }
                // Above native x86 MAX_RW_COUNT, recvfrom's imported iterator
                // may clamp before range checks. Do not infer Read's EFAULT
                // precedence for that unsupported shape or rewrite its count.
                if raw.arg2 > 0x7fff_f000 {
                    return Err(Error::Tool(anyhow::anyhow!(
                        "V4 scalar recvfrom capacity exceeds the native unclamped range contract"
                    )));
                }
                Ok(Self(Original::Recvfrom(receive)))
            }
            _ => Err(Error::Tool(anyhow::anyhow!(
                "V4 scalar receive requires the original Read or Recvfrom"
            ))),
        }
    }

    pub(crate) fn syscall(self) -> Syscall {
        match self.0 {
            Original::Read(read) => read.into(),
            Original::Recvfrom(receive) => receive.into(),
        }
    }

    pub(crate) fn into_parts(self) -> (Sysno, SyscallArgs) {
        self.syscall().into_parts()
    }

    pub(crate) fn fd(self) -> i32 {
        match self.0 {
            Original::Read(read) => read.fd(),
            Original::Recvfrom(receive) => receive.fd(),
        }
    }

    pub(crate) fn destination(self) -> u64 {
        self.into_parts().1.arg1 as u64
    }

    pub(crate) fn capacity(self) -> usize {
        self.into_parts().1.arg2
    }

    pub(crate) fn selected_maximum(self) -> usize {
        self.capacity().min(512)
    }

    /// Keep the existing Read capacity refusal. Only the new genuine Recvfrom
    /// bridge separates its authenticated capacity from bounded admission.
    pub(crate) fn admission_maximum(self) -> usize {
        match self.0 {
            Original::Read(_) => self.capacity(),
            Original::Recvfrom(_) => self.selected_maximum(),
        }
    }

    pub(crate) fn signal_interrupt_errno(self) -> reverie::Errno {
        use crate::syscalls::helpers::NonblockableSyscall;
        match self.0 {
            Original::Read(read) => read.signal_interrupt_errno(),
            Original::Recvfrom(receive) => receive.signal_interrupt_errno(),
        }
    }

    /// The saved original policy may authorize only its own bounded prefix.
    /// Neither this predicate nor construction authenticates the original stop.
    pub(crate) fn matches_selection(self, maximum: usize, destination: u64) -> bool {
        maximum != 0 && maximum == self.selected_maximum() && destination == self.destination()
    }

    pub(crate) fn inspect_original_range<T: Tool, G: Guest<T>>(
        self,
        guest: &G,
    ) -> Result<OriginalReadRangeVerdict, Error> {
        match self.0 {
            Original::Read(read) => guest.inspect_original_read_range(read),
            Original::Recvfrom(receive) => guest.inspect_original_recvfrom_range(receive),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recvfrom_preserves_full_original_tuple_separately_from_selection() {
        let raw = SyscallArgs::new(17, 0x1234_5000, 102_400, 0, 0, 0);
        let original = Syscall::from_raw(Sysno::recvfrom, raw);
        let receive = ScalarReceive::from_syscall(original).unwrap();
        assert_eq!(receive.into_parts(), (Sysno::recvfrom, raw));
        assert_eq!(receive.syscall(), original);
        assert_eq!(receive.fd(), 17);
        assert_eq!(receive.destination(), 0x1234_5000);
        assert_eq!(receive.capacity(), 102_400);
        assert_eq!(receive.selected_maximum(), 512);
        assert_eq!(receive.admission_maximum(), 512);
        assert!(receive.matches_selection(512, 0x1234_5000));
        assert!(!receive.matches_selection(102_400, 0x1234_5000));
        assert!(!receive.matches_selection(511, 0x1234_5000));
        assert!(!receive.matches_selection(512, 0x1234_5001));
        assert_eq!(receive.into_parts(), (Sysno::recvfrom, raw));
    }

    #[test]
    fn recvfrom_rejects_wrong_number_flags_and_each_source_output() {
        let raw = SyscallArgs::new(17, 0x1234_5000, 102_400, 0, 0, 0);
        assert!(ScalarReceive::from_syscall(Syscall::from_raw(Sysno::recvmsg, raw)).is_err());
        for changed in [
            SyscallArgs {
                arg3: libc::MSG_PEEK as usize,
                ..raw
            },
            SyscallArgs {
                arg3: libc::MSG_DONTWAIT as usize,
                ..raw
            },
            SyscallArgs {
                arg3: libc::MSG_WAITALL as usize,
                ..raw
            },
            SyscallArgs {
                arg3: 1usize << 32,
                ..raw
            },
            SyscallArgs {
                arg4: 0x2000,
                ..raw
            },
            SyscallArgs {
                arg5: 0x3000,
                ..raw
            },
        ] {
            let result = ScalarReceive::from_syscall(Syscall::from_raw(Sysno::recvfrom, changed));
            assert!(result.is_err(), "unexpected supported tuple: {changed:?}");
        }
    }

    #[test]
    fn read_keeps_all_six_original_words_and_the_same_bounded_prefix_rule() {
        let raw = SyscallArgs::new(17, 0x1234_5000, 102_400, 23, 24, 25);
        let receive = ScalarReceive::from_syscall(Syscall::from_raw(Sysno::read, raw)).unwrap();
        assert_eq!(receive.into_parts(), (Sysno::read, raw));
        assert_eq!(receive.capacity(), 102_400);
        assert_eq!(receive.selected_maximum(), 512);
        assert!(receive.matches_selection(512, 0x1234_5000));
        for capacity in [0, 1, 511, 512, 513, usize::MAX] {
            let original = Read::from(SyscallArgs {
                arg2: capacity,
                ..raw
            });
            let receive = ScalarReceive::from(original);
            assert_eq!(receive.into_parts(), original.into_parts());
            assert_eq!(receive.capacity(), capacity);
            assert_eq!(receive.selected_maximum(), capacity.min(512));
            assert_eq!(receive.admission_maximum(), capacity);
            assert_eq!(
                receive.matches_selection(capacity.min(512), 0x1234_5000),
                capacity != 0
            );
        }
    }

    #[test]
    fn empty_recvfrom_is_not_converted_to_read_or_a_positive_selection() {
        let raw = SyscallArgs::new(17, 0, 0, 0, 0, 0);
        let receive = ScalarReceive::from_syscall(Syscall::from_raw(Sysno::recvfrom, raw)).unwrap();
        assert_eq!(receive.into_parts(), (Sysno::recvfrom, raw));
        assert_eq!(receive.selected_maximum(), 0);
        assert!(!receive.matches_selection(0, 0));
    }

    #[test]
    fn recvfrom_oversized_import_refuses_without_reclassifying_read_range_errors() {
        let raw = SyscallArgs::new(17, 0x1000, 0x7fff_f000, 0, 0, 0);
        let receive = ScalarReceive::from_syscall(Syscall::from_raw(Sysno::recvfrom, raw)).unwrap();
        assert_eq!(receive.capacity(), 0x7fff_f000);
        assert_eq!(receive.selected_maximum(), 512);
        for capacity in [0x7fff_f001, usize::MAX] {
            let arguments = SyscallArgs {
                arg2: capacity,
                ..raw
            };
            let result = ScalarReceive::from_syscall(Syscall::from_raw(Sysno::recvfrom, arguments));
            assert!(matches!(result, Err(Error::Tool(_))), "{result:?}");
            let read =
                ScalarReceive::from_syscall(Syscall::from_raw(Sysno::read, arguments)).unwrap();
            assert_eq!(read.into_parts(), (Sysno::read, arguments));
        }
    }
}
