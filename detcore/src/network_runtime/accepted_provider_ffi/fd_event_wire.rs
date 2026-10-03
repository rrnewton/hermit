//! Exact descriptor-event buffers for authenticated native adapter versions.
//! A legacy DSO never receives the new struct, and cannot supply dispatch proof.

use std::ffi::c_int;
use std::ffi::c_void;

use super::CallStatus;
use super::FdEvent;
use super::Observation;
use crate::network_runtime::ProviderWireFormat;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct LegacyFdEvent {
    sequence: u64,
    kind: u64,
    task: u64,
    task_start: u64,
    table: u64,
    file: u64,
    previous_file: u64,
    dependency: u64,
    accept_command: u64,
    fd: i32,
    returned: i32,
    complete: u64,
    mode: u32,
    status_flags: u32,
    device_major: u32,
    device_minor: u32,
}
const _: () = {
    assert!(std::mem::size_of::<LegacyFdEvent>() == 104);
    assert!(std::mem::offset_of!(LegacyFdEvent, complete) == 80);
    assert!(std::mem::offset_of!(LegacyFdEvent, mode) == 88);
    assert!(std::mem::offset_of!(LegacyFdEvent, device_minor) == 100);
};

impl From<LegacyFdEvent> for FdEvent {
    fn from(raw: LegacyFdEvent) -> Self {
        Self {
            sequence: raw.sequence,
            kind: raw.kind,
            task: raw.task,
            task_start: raw.task_start,
            table: raw.table,
            file: raw.file,
            previous_file: raw.previous_file,
            dependency: raw.dependency,
            accept_command: raw.accept_command,
            fd: raw.fd,
            returned: raw.returned,
            complete: raw.complete,
            mode: raw.mode,
            status_flags: raw.status_flags,
            device_major: raw.device_major,
            device_minor: raw.device_minor,
            source_ioctl_dispatch: super::SOURCE_IOCTL_DISPATCH_UNKNOWN,
        }
    }
}
impl TryFrom<&FdEvent> for LegacyFdEvent {
    type Error = ();
    fn try_from(raw: &FdEvent) -> Result<Self, Self::Error> {
        if raw.source_ioctl_dispatch != super::SOURCE_IOCTL_DISPATCH_UNKNOWN {
            return Err(());
        }
        Ok(Self {
            sequence: raw.sequence,
            kind: raw.kind,
            task: raw.task,
            task_start: raw.task_start,
            table: raw.table,
            file: raw.file,
            previous_file: raw.previous_file,
            dependency: raw.dependency,
            accept_command: raw.accept_command,
            fd: raw.fd,
            returned: raw.returned,
            complete: raw.complete,
            mode: raw.mode,
            status_flags: raw.status_flags,
            device_major: raw.device_major,
            device_minor: raw.device_minor,
        })
    }
}
fn read_legacy(raw: &mut LegacyFdEvent, call: impl FnOnce(*mut c_void) -> c_int) -> CallStatus {
    CallStatus::capture("ap_read_fd_event", call(std::ptr::from_mut(raw).cast()))
}
fn read_current(raw: &mut FdEvent, call: impl FnOnce(*mut c_void) -> c_int) -> CallStatus {
    CallStatus::capture("ap_read_fd_event", call(std::ptr::from_mut(raw).cast()))
}
pub(super) fn read(
    wire: ProviderWireFormat,
    call: impl FnOnce(*mut c_void) -> c_int,
) -> Observation<FdEvent> {
    if wire.has_source_ioctl_dispatch() {
        let mut raw = FdEvent::default();
        let status = read_current(&mut raw, call);
        Observation { status, raw }
    } else {
        let mut raw = LegacyFdEvent::default();
        let status = read_legacy(&mut raw, call);
        Observation {
            status,
            raw: raw.into(),
        }
    }
}
pub(super) fn ack(
    wire: ProviderWireFormat,
    receipt: &FdEvent,
    call: impl FnOnce(*const c_void) -> c_int,
) -> CallStatus {
    if wire.has_source_ioctl_dispatch() {
        CallStatus::capture("ap_ack_fd_event", call(std::ptr::from_ref(receipt).cast()))
    } else {
        let Ok(legacy) = LegacyFdEvent::try_from(receipt) else {
            // Local refusal: no call is issued and no dispatch proof is erased.
            return CallStatus {
                operation: "ap_ack_fd_event",
                returned: -1,
                errno: Some(libc::EPROTO),
            };
        };
        CallStatus::capture("ap_ack_fd_event", call(std::ptr::from_ref(&legacy).cast()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const BEFORE: u64 = 0x1020_3040_5060_7080;
    const AFTER: u64 = 0x9080_7060_5040_3020;
    #[repr(C)]
    struct Guarded<T> {
        before: u64,
        value: T,
        after: u64,
    }
    fn legacy() -> LegacyFdEvent {
        LegacyFdEvent {
            sequence: 1,
            kind: 21,
            task: 3,
            task_start: 5,
            table: 7,
            file: 11,
            previous_file: 13,
            dependency: 17,
            accept_command: 19,
            fd: -23,
            returned: -29,
            complete: 31,
            mode: 37,
            status_flags: 41,
            device_major: 43,
            device_minor: 47,
        }
    }
    #[test]
    fn mismatched_adapter_or_copy_declaration_cannot_reach_event_symbols() {
        let formats = [
            ProviderWireFormat::Abi7Copy4,
            ProviderWireFormat::Abi8Copy5,
            ProviderWireFormat::Abi9Copy4,
            ProviderWireFormat::Abi9Copy5,
            ProviderWireFormat::Abi10Copy4,
            ProviderWireFormat::Abi10Copy5,
        ];
        let topology = crate::network_runtime::ProviderTopology::FtraceV1 {
            contract_sha256: [0x29; 32],
        };
        for expected in formats {
            for actual in formats {
                let mut names = Vec::new();
                let result =
                    super::super::authenticate_provider_declarations(expected, &topology, |name| {
                        names.push(name.to_owned());
                        match name.to_bytes() {
                            b"ap_adapter_abi_version" => Ok(actual.abi_version()),
                            b"ap_adapter_copy_version" => Ok(actual.copy_version()),
                            b"ap_provider_topology_version" => Ok(topology.driver_version()),
                            b"ap_adapter_task_command_size" => Ok(72),
                            _ => panic!(
                                "an event operation resolved before its layout was authenticated"
                            ),
                        }
                    });
                assert_eq!(result.is_ok(), expected == actual);
                if expected.abi_version() != actual.abi_version() {
                    assert_eq!(names, [c"ap_adapter_abi_version".to_owned()]);
                } else if expected.copy_version() != actual.copy_version() {
                    assert_eq!(
                        names,
                        [
                            c"ap_adapter_abi_version".to_owned(),
                            c"ap_adapter_copy_version".to_owned()
                        ]
                    );
                }
            }
        }
    }

    #[test]
    fn exact_legacy_and_current_buffers_preserve_every_field_and_canaries() {
        let mut old = Guarded {
            before: BEFORE,
            value: LegacyFdEvent::default(),
            after: AFTER,
        };
        assert!(
            read_legacy(&mut old.value, |p| {
                // SAFETY: this controlled legacy producer writes its exact104-byte ABI.
                unsafe { p.cast::<LegacyFdEvent>().write(legacy()) };
                0
            })
            .succeeded()
        );
        assert_eq!((old.before, old.after), (BEFORE, AFTER));
        assert_eq!(old.value, legacy());
        let current = FdEvent {
            source_ioctl_dispatch: 2,
            ..FdEvent::from(legacy())
        };
        let mut new = Guarded {
            before: BEFORE,
            value: FdEvent::default(),
            after: AFTER,
        };
        assert!(
            read_current(&mut new.value, |p| {
                // SAFETY: this controlled current producer writes its exact112-byte ABI.
                unsafe { p.cast::<FdEvent>().write(current) };
                0
            })
            .succeeded()
        );
        assert_eq!((new.before, new.after), (BEFORE, AFTER));
        assert_eq!(new.value, current);
        for wire in [ProviderWireFormat::Abi7Copy4, ProviderWireFormat::Abi8Copy5] {
            let out = read(wire, |p| {
                unsafe { p.cast::<LegacyFdEvent>().write(legacy()) };
                0
            });
            assert!(out.status.succeeded());
            assert_eq!(out.raw, FdEvent::from(legacy()));
            assert_eq!(out.raw.source_ioctl_dispatch, 0);
            assert!(
                ack(wire, &out.raw, |p| {
                    assert_eq!(unsafe { p.cast::<LegacyFdEvent>().read() }, legacy());
                    0
                })
                .succeeded()
            );
            let refused = ack(wire, &current, |_| {
                panic!("must not truncate dispatch for old ACK")
            });
            assert_eq!(refused.errno, Some(libc::EPROTO));
        }
        for wire in [
            ProviderWireFormat::Abi9Copy4,
            ProviderWireFormat::Abi9Copy5,
            ProviderWireFormat::Abi10Copy4,
            ProviderWireFormat::Abi10Copy5,
        ] {
            let out = read(wire, |p| {
                unsafe { p.cast::<FdEvent>().write(current) };
                0
            });
            assert!(out.status.succeeded());
            assert_eq!(out.raw, current);
            assert!(
                ack(wire, &out.raw, |p| {
                    assert_eq!(unsafe { p.cast::<FdEvent>().read() }, current);
                    0
                })
                .succeeded()
            );
        }
    }
}
