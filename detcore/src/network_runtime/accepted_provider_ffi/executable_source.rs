//! ABI11 raw executable observations. These values are not a source capability.
use std::ffi::c_int;
use std::io;

use super::CommandResult;
use super::RawState;
use super::SessionPtr;

pub(super) type Prepare =
    unsafe extern "C" fn(SessionPtr, c_int, u64, u64, u64, u64, u64, u64, u64, *mut u64) -> c_int;
pub(super) type Collect =
    unsafe extern "C" fn(SessionPtr, c_int, u64, *mut CommandResult, *mut Receipt) -> c_int;
pub(super) struct Api {
    pub prepare: Prepare,
    pub collect: Collect,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Intent {
    pub command: u64,
    pub registration: u64,
    pub owner_mm: u64,
    pub call: u64,
    pub address: u64,
    pub length: u64,
    pub iovec: u64,
    pub registers: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Mapping {
    pub task: u64,
    pub start: u64,
    pub tracer: u64,
    pub tracer_start: u64,
    pub mm: u64,
    pub file: u64,
    pub exe_file: u64,
    pub inode: u64,
    pub mapping: u64,
    pub fops: u64,
    pub vm_ops: u64,
    pub filesystem: u64,
    pub device: u64,
    pub inode_number: u64,
    pub file_size: u64,
    pub vm_start: u64,
    pub vm_end: u64,
    pub vm_pgoff: u64,
    pub vm_flags: u64,
    pub file_mode: u32,
    pub inode_mode: u32,
    pub writecount: i64,
    pub iovec_base: u64,
    pub iovec_length: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Receipt {
    pub intent: Intent,
    pub entered: Mapping,
    pub returned: Mapping,
    pub phases: u64,
    pub problem: u64,
    pub ptrace_return: i64,
    pub find_enter_return: i64,
    pub find_exit_return: i64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Effect {
    pub command: CommandResult,
    pub receipt: Receipt,
}
fn read_into(
    wire: crate::network_runtime::ProviderWireFormat,
    raw: &mut Effect,
    call: impl FnOnce(*mut CommandResult, *mut Receipt) -> c_int,
) -> io::Result<super::CallStatus> {
    if !wire.has_executable_source() {
        return Err(io::Error::other(
            "executable source requires authenticated ABI11",
        ));
    }
    *raw = Effect::default();
    let rc = call(&mut raw.command, &mut raw.receipt);
    Ok(super::CallStatus::capture(
        "ap_collect_executable_source",
        rc,
    ))
}
pub(super) fn read(
    wire: crate::network_runtime::ProviderWireFormat,
    call: impl FnOnce(*mut CommandResult, *mut Receipt) -> c_int,
) -> io::Result<super::Observation<Effect>> {
    let mut raw = Effect::default();
    let status = read_into(wire, &mut raw, call)?;
    Ok(super::Observation { status, raw })
}
const _: () = assert!(
    size_of::<Intent>() == 64 && size_of::<Mapping>() == 184 && size_of::<Receipt>() == 472
);
const IMAGE_ANCHOR: u64 = 0xffffffff8206bf70;
const FOPS: u64 = 0xffffffff82a8bba8;
const VMOPS: u64 = 0xffffffff82a8bb20;
fn range(i: &Intent) -> bool {
    i.address != 0
        && (1..=512).contains(&i.length)
        && i.address < 0x800000000000
        && i.length <= 0x800000000000 - i.address
        && i.address >> 12 == (i.address + i.length - 1) >> 12
}
impl Mapping {
    fn valid(&self, i: &Intent, anchor: u64) -> bool {
        let o = self;
        let Some(fops) = FOPS.checked_add_signed(anchor.wrapping_sub(IMAGE_ANCHOR) as i64) else {
            return false;
        };
        let Some(vmops) = VMOPS.checked_add_signed(anchor.wrapping_sub(IMAGE_ANCHOR) as i64) else {
            return false;
        };
        if !range(i)
            || anchor < 0xffff800000000000
            || anchor & 4095 != IMAGE_ANCHOR & 4095
            || [
                o.task,
                o.start,
                o.tracer,
                o.tracer_start,
                o.mm,
                o.file,
                o.inode,
                o.mapping,
                o.inode_number,
                o.file_size,
            ]
            .contains(&0)
            || o.file != o.exe_file
            || o.file_size > i64::MAX as u64
            || o.inode_mode & 0o170000 != 0o100000
            || o.inode_mode > 0o177777
            || o.filesystem != 0x9123683e
            || !(i32::MIN as i64..0).contains(&o.writecount)
            || o.file_mode & 1 == 0
            || o.file_mode & 2 != 0
            || o.file_mode & 32 == 0
            || !matches!(o.file_mode & 0x06000000, 0x02000000 | 0x04000000)
            || o.fops != fops
            || o.vm_ops != vmops
            || o.vm_start == 0
            || o.vm_start >= o.vm_end
            || o.vm_start & 4095 != 0
            || o.vm_end & 4095 != 0
            || o.vm_flags & 1 == 0
            || o.vm_flags & (2 | 8 | 0x400 | 0x4000 | 0x40000 | 0x400000 | 0x10000000) != 0
            || i.address < o.vm_start
            || i.address > o.vm_end
            || i.length > o.vm_end - i.address
            || o.vm_pgoff > u64::MAX >> 12
            || o.iovec_base != i.registers
            || o.iovec_length != 216
        {
            return false;
        }
        let Some(offset) = (o.vm_pgoff << 12).checked_add(i.address - o.vm_start) else {
            return false;
        };
        offset <= o.file_size && i.length <= o.file_size - offset
    }
}
impl Effect {
    /// Authenticate the closed observation against the originally held command,
    /// image anchor and original task identity. No anonymous-backing authority.
    pub fn validate(
        &self,
        expected: Intent,
        provider: u64,
        task: u64,
        start: u64,
        anchor: u64,
    ) -> io::Result<()> {
        let r = &self.command;
        let e = &self.receipt;
        let i = &e.intent;
        let mut second = e.returned;
        second.writecount = e.entered.writecount;
        let valid = provider != 0
            && expected.command != 0
            && expected.registration != 0
            && expected.call != 0
            && expected.iovec != 0
            && expected.registers != 0
            && expected.iovec != expected.registers
            && *i == expected
            && range(i)
            && r.command == i.command
            && r.operation == 26
            && r.phase == 1
            && r.original_count == i.length
            && r.identity.provider == provider
            && r.identity.object == 0
            && r.identity.namespace == 0
            && r.creation == 0
            && r.cookie == 0
            && r.returned == 0
            && r.reserved == 0
            && r.state == RawState::default()
            && r.task == task
            && r.start_boottime == start
            && r.task == e.entered.task
            && r.start_boottime == e.entered.start
            && e.phases == 7
            && e.problem == 0
            && e.ptrace_return == 0
            && e.find_enter_return == 0
            && e.find_exit_return == 0
            && e.entered.valid(i, anchor)
            && e.returned.valid(i, anchor)
            && second == e.entered;
        if valid {
            Ok(())
        } else {
            Err(io::Error::other(
                "executable source command/observation mismatch",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::Identity;
    use super::super::LoadError;
    use super::super::authenticate_provider_declarations;
    use super::*;
    use crate::network_runtime::ProviderTopology;
    use crate::network_runtime::ProviderWireFormat;
    fn good() -> Effect {
        let intent = Intent {
            command: 7,
            registration: 11,
            owner_mm: 0,
            call: 13,
            address: 0x401020,
            length: 5,
            iovec: 0x700000,
            registers: 0x700100,
        };
        let m = Mapping {
            task: 17,
            start: 19,
            tracer: 23,
            tracer_start: 29,
            mm: 31,
            file: 37,
            exe_file: 37,
            inode: 41,
            mapping: 43,
            fops: FOPS,
            vm_ops: VMOPS,
            filesystem: 0x9123683e,
            device: 47,
            inode_number: 53,
            file_size: 8192,
            vm_start: 0x401000,
            vm_end: 0x402000,
            vm_pgoff: 1,
            vm_flags: 5,
            file_mode: 0x02000021,
            inode_mode: 0o100755,
            writecount: -1,
            iovec_base: intent.registers,
            iovec_length: 216,
        };
        Effect {
            command: CommandResult {
                command: 7,
                operation: 26,
                task: 17,
                start_boottime: 19,
                identity: Identity {
                    provider: 3,
                    ..Default::default()
                },
                phase: 1,
                original_count: 5,
                ..Default::default()
            },
            receipt: Receipt {
                intent,
                entered: m,
                returned: m,
                phases: 7,
                ..Default::default()
            },
        }
    }
    fn accepted(e: &Effect) -> bool {
        e.validate(good().receipt.intent, 3, 17, 19, IMAGE_ANCHOR)
            .is_ok()
    }
    #[test]
    fn executable_receipt_closed_fields_and_geometry_are_checked() {
        let original = good();
        assert!(accepted(&original));
        let mut changed = original;
        changed.receipt.returned.writecount = -2;
        assert!(accepted(&changed));
        for anchor in [0, 1, IMAGE_ANCHOR + 1, 0xffff7fffffffffff] {
            let mut e = original;
            e.receipt.entered.fops = 0;
            e.receipt.entered.vm_ops = 0;
            e.receipt.returned = e.receipt.entered;
            assert!(e.validate(e.receipt.intent, 3, 17, 19, anchor).is_err());
        }
        for value in [0, 1, i64::MIN] {
            let mut e = original;
            e.receipt.returned.writecount = value;
            assert!(!accepted(&e));
        }
        // Every raw receipt word is independently changed. Negative writecounts
        // are deliberately not equality-bound; mutate that word to nonnegative.
        let raw = unsafe {
            std::slice::from_raw_parts((&original.receipt as *const Receipt).cast::<u64>(), 59)
        };
        for at in 0..59 {
            let mut e = original;
            let words = unsafe {
                std::slice::from_raw_parts_mut((&mut e.receipt as *mut Receipt).cast::<u64>(), 59)
            };
            words[at] = if at == 28 || at == 51 { 0 } else { raw[at] ^ 1 };
            assert!(!accepted(&e), "receipt word {at}");
        }
        for mode in [0, 0x21, 0x06000021, 0x02000023] {
            let mut e = original;
            e.receipt.entered.file_mode = mode;
            e.receipt.returned.file_mode = mode;
            assert!(!accepted(&e));
        }
        for flags in [0, 3, 9, 0x401, 0x4001, 0x40001, 0x400001, 0x10000001] {
            let mut e = original;
            e.receipt.entered.vm_flags = flags;
            e.receipt.returned.vm_flags = flags;
            assert!(!accepted(&e));
        }
        for (address, length) in [
            (0, 1),
            (0x401000, 0),
            (0x401000, 513),
            (0x401fff, 2),
            (u64::MAX, 1),
            (0x800000000000, 1),
        ] {
            let mut e = original;
            e.receipt.intent.address = address;
            e.receipt.intent.length = length;
            assert!(
                e.validate(e.receipt.intent, 3, 17, 19, IMAGE_ANCHOR)
                    .is_err()
            );
        }
        let mut e = original;
        e.receipt.entered.vm_pgoff = u64::MAX;
        e.receipt.returned = e.receipt.entered;
        assert!(!accepted(&e));
        let mut e = original;
        e.receipt.entered.file_size = 4096;
        e.receipt.returned = e.receipt.entered;
        assert!(!accepted(&e));
        for field in 0..6 {
            let mut e = original;
            match field {
                0 => e.command.phase = 2,
                1 => e.command.returned = -14,
                2 => e.command.identity.object = 1,
                3 => e.command.state.lowat = 1,
                4 => e.command.original_count = 6,
                _ => e.command.reserved = 1,
            };
            assert!(!accepted(&e));
        }
    }
    #[test]
    fn executable_typed_writer_preserves_failure_and_never_calls_legacy() {
        #[repr(C)]
        struct Guarded {
            before: u64,
            raw: Effect,
            after: u64,
        }
        for wire in [
            ProviderWireFormat::Abi11Copy4,
            ProviderWireFormat::Abi11Copy5,
        ] {
            let mut guarded = Guarded {
                before: 0xfeed,
                raw: Effect::default(),
                after: 0xbeef,
            };
            let expected = good();
            let status = read_into(wire, &mut guarded.raw, |command, receipt| {
                unsafe {
                    command.write(expected.command);
                    receipt.write(expected.receipt);
                }
                0
            })
            .unwrap();
            assert!(status.succeeded());
            assert_eq!((guarded.before, guarded.after), (0xfeed, 0xbeef));
            assert_eq!(guarded.raw, expected);
            let observation = read(wire, |command, out| {
                unsafe {
                    assert_eq!(*out, Receipt::default());
                    assert_eq!(*command, CommandResult::default());
                    (*out).entered.file_mode = 0x04000021;
                    (*out).ptrace_return = -14;
                    (*command).returned = -14;
                    *libc::__errno_location() = libc::EIO;
                }
                -1
            })
            .unwrap();
            assert_eq!(observation.raw.receipt.ptrace_return, -14);
            assert_eq!(observation.raw.command.returned, -14);
            assert_eq!(observation.raw.receipt.entered.file_mode, 0x04000021);
            assert!(!observation.status.succeeded());
            assert_eq!(observation.status.errno, Some(libc::EIO));
        }
        for old in [
            ProviderWireFormat::Abi7Copy4,
            ProviderWireFormat::Abi8Copy5,
            ProviderWireFormat::Abi9Copy4,
            ProviderWireFormat::Abi9Copy5,
            ProviderWireFormat::Abi10Copy4,
            ProviderWireFormat::Abi10Copy5,
        ] {
            assert!(
                read(old, |_, _| panic!(
                    "legacy format reached executable writer"
                ))
                .is_err()
            );
        }
    }
    #[test]
    fn executable_abi_declares_exact_layout_before_operational_resolution() {
        for wire in [
            ProviderWireFormat::Abi11Copy4,
            ProviderWireFormat::Abi11Copy5,
        ] {
            assert_eq!(
                ProviderWireFormat::from_versions(wire.abi_version(), wire.copy_version()).unwrap(),
                wire
            );
            assert_eq!(
                ProviderWireFormat::from_package("415052555354000b", Some(wire.copy_version()))
                    .unwrap(),
                wire
            );
            for size in [
                None,
                Some(0),
                Some(464),
                Some(471),
                Some(472),
                Some(473),
                Some(480),
                Some(u64::MAX),
            ] {
                let mut names = Vec::new();
                let result = authenticate_provider_declarations(
                    wire,
                    &ProviderTopology::ClassicV40,
                    |name| {
                        names.push(name.to_owned());
                        match name.to_bytes() {
                            b"ap_adapter_abi_version" => Ok(wire.abi_version()),
                            b"ap_adapter_copy_version" => Ok(wire.copy_version()),
                            b"ap_adapter_task_command_size" => Ok(72),
                            b"ap_adapter_executable_source_size" => {
                                size.ok_or_else(|| LoadError("missing size".into()))
                            }
                            b"ap_provider_topology_version" => Ok(0),
                            _ => panic!("operational symbol reached before declarations"),
                        }
                    },
                );
                assert_eq!(result.is_ok(), size == Some(472));
                assert_eq!(
                    names.last().unwrap().to_bytes(),
                    if size == Some(472) {
                        b"ap_provider_topology_version".as_slice()
                    } else {
                        b"ap_adapter_executable_source_size".as_slice()
                    }
                );
            }
        }
        for old in [
            ProviderWireFormat::Abi7Copy4,
            ProviderWireFormat::Abi8Copy5,
            ProviderWireFormat::Abi9Copy4,
            ProviderWireFormat::Abi9Copy5,
            ProviderWireFormat::Abi10Copy4,
            ProviderWireFormat::Abi10Copy5,
        ] {
            assert!(!old.has_executable_source());
        }
        for copy in [None, Some(0), Some(6), Some(u64::MAX)] {
            assert!(ProviderWireFormat::from_package("415052555354000b", copy).is_err());
        }
    }
}
