//! Private op26 transport values. Only actual successful packaged C collection
//! authenticates the image anchor; these serializable values are not capabilities.
use std::io;

use serde::Deserialize;
use serde::Serialize;

use super::CommandResult;
use super::Observation;
use super::ffi::executable_source as raw;
wire!(pub(in crate::network_runtime) Intent, raw::Intent, {command:u64,registration:u64,owner_mm:u64,call:u64,address:u64,length:u64,iovec:u64,registers:u64});
impl From<Intent> for raw::Intent {
    fn from(v: Intent) -> Self {
        Self {
            command: v.command,
            registration: v.registration,
            owner_mm: v.owner_mm,
            call: v.call,
            address: v.address,
            length: v.length,
            iovec: v.iovec,
            registers: v.registers,
        }
    }
}
wire!(pub(in crate::network_runtime) Mapping, raw::Mapping, {task:u64,start:u64,tracer:u64,tracer_start:u64,mm:u64,file:u64,exe_file:u64,inode:u64,mapping:u64,fops:u64,vm_ops:u64,filesystem:u64,device:u64,inode_number:u64,file_size:u64,vm_start:u64,vm_end:u64,vm_pgoff:u64,vm_flags:u64,file_mode:u32,inode_mode:u32,writecount:i64,iovec_base:u64,iovec_length:u64});
impl From<Mapping> for raw::Mapping {
    fn from(v: Mapping) -> Self {
        Self {
            task: v.task,
            start: v.start,
            tracer: v.tracer,
            tracer_start: v.tracer_start,
            mm: v.mm,
            file: v.file,
            exe_file: v.exe_file,
            inode: v.inode,
            mapping: v.mapping,
            fops: v.fops,
            vm_ops: v.vm_ops,
            filesystem: v.filesystem,
            device: v.device,
            inode_number: v.inode_number,
            file_size: v.file_size,
            vm_start: v.vm_start,
            vm_end: v.vm_end,
            vm_pgoff: v.vm_pgoff,
            vm_flags: v.vm_flags,
            file_mode: v.file_mode,
            inode_mode: v.inode_mode,
            writecount: v.writecount,
            iovec_base: v.iovec_base,
            iovec_length: v.iovec_length,
        }
    }
}
wire!(pub(in crate::network_runtime) Receipt, raw::Receipt, {intent:Intent,entered:Mapping,returned:Mapping,phases:u64,problem:u64,ptrace_return:i64,find_enter_return:i64,find_exit_return:i64});
impl From<Receipt> for raw::Receipt {
    fn from(v: Receipt) -> Self {
        Self {
            intent: v.intent.into(),
            entered: v.entered.into(),
            returned: v.returned.into(),
            phases: v.phases,
            problem: v.problem,
            ptrace_return: v.ptrace_return,
            find_enter_return: v.find_enter_return,
            find_exit_return: v.find_exit_return,
        }
    }
}
wire!(pub(in crate::network_runtime) Effect, raw::Effect, {command:CommandResult,receipt:Receipt});
impl From<Effect> for raw::Effect {
    fn from(v: Effect) -> Self {
        Self {
            command: v.command.into(),
            receipt: v.receipt.into(),
        }
    }
}

impl Intent {
    pub(in crate::network_runtime) fn valid_unarmed(&self) -> bool {
        self.command == 0
            && self.registration != 0
            && self.call != 0
            && self.address != 0
            && (1..=512).contains(&self.length)
            && self.address < 0x800000000000
            && self.length <= 0x800000000000 - self.address
            && self.address >> 12 == (self.address + self.length - 1) >> 12
            && self.iovec != 0
            && self.registers != 0
            && self.iovec != self.registers
    }
}

/// Structural/correlation validation is independent of the C image check. This
/// function deliberately does not reconstruct an anchor from observed pointers.
/// A caller issuing R's unsafe backing proof must additionally hold the actual
/// successful service response from the packaged C collect and its exact ACK.
pub(in crate::network_runtime) fn validate_collection(
    observed: &Observation<Effect>,
    expected: &Intent,
    provider: u64,
    task: u64,
    start: u64,
) -> io::Result<()> {
    let e = &observed.raw;
    let c: super::ffi::CommandResult = e.command.clone().into();
    let r: raw::Receipt = e.receipt.clone().into();
    let mut unarmed = expected.clone();
    unarmed.command = 0;
    let mut returned = r.returned;
    returned.writecount = r.entered.writecount;
    if observed.status.operation != "ap_collect_executable_source"
        || observed.status.returned != 0
        || observed.status.errno.is_some()
        || !unarmed.valid_unarmed()
        || expected.command == 0
        || r.intent != expected.clone().into()
        || c.command != expected.command
        || c.operation != 26
        || c.phase != 1
        || c.returned != 0
        || c.reserved != 0
        || c.original_count != expected.length
        || c.identity
            != (super::ffi::Identity {
                provider,
                object: 0,
                namespace: 0,
            })
        || provider == 0
        || task == 0
        || start == 0
        || c.task != task
        || c.start_boottime != start
        || c.creation != 0
        || c.cookie != 0
        || c.state != super::ffi::RawState::default()
        || r.phases != 7
        || r.problem != 0
        || r.ptrace_return != 0
        || r.find_enter_return != 0
        || r.find_exit_return != 0
        || r.entered.task != task
        || r.entered.start != start
        || r.entered.tracer == 0
        || r.entered.tracer_start == 0
        || r.entered != returned
        || r.entered.writecount >= 0
        || r.returned.writecount >= 0
        || r.entered.file == 0
        || r.entered.file != r.entered.exe_file
        || r.entered.mm == 0
        || r.entered.inode == 0
        || r.entered.mapping == 0
        || r.entered.iovec_base != expected.registers
        || r.entered.iovec_length != 216
    {
        return Err(io::Error::other(
            "executable source changed actual collection/command/identity",
        ));
    }
    Ok(())
}

/// Both ends of the existing transport validate this same retained two-frame
/// group; no reconstructed request or copied status can stand in for its rights.
pub(in crate::network_runtime) fn validate_group(
    owner: super::NetworkStreamOwner,
    call: u64,
    prepared: u64,
    completed: u64,
    views: &[(&super::Envelope, &[u8], usize)],
) -> io::Result<()> {
    use super::Operation;
    use super::Reply;
    use super::Request;
    if call == 0 || prepared == 0 || completed <= prepared || views.len() != 2 {
        return Err(io::Error::other("executable group sequence/count mismatch"));
    }
    let [(p, pb, pr), (c, cb, cr)] = views else {
        unreachable!()
    };
    if p.owner != Some(owner)
        || c.owner != Some(owner)
        || p.accept.is_some()
        || c.accept.is_some()
        || p.sequence != prepared
        || c.sequence != completed
        || *pr != 1
        || *cr != 0
        || p.operation != Operation::PrepareExecutableSource
        || c.operation != Operation::CollectExecutableSource
    {
        return Err(io::Error::other(
            "executable group changed owned target/frames",
        ));
    }
    let Request::PrepareExecutableSource { mut intent } = serde_json::from_slice(&p.body)? else {
        return Err(io::Error::other("executable group preparation kind"));
    };
    let Reply::Prepared(armed) = serde_json::from_slice(pb)? else {
        return Err(io::Error::other("executable group installation missing"));
    };
    if !intent.valid_unarmed()
        || intent.owner_mm != owner.mm.generation()
        || intent.call != call
        || armed.status.operation != "ap_prepare_executable_source"
        || armed.status.returned != 0
        || armed.status.errno.is_some()
        || armed.raw == 0
    {
        return Err(io::Error::other("executable group installation changed"));
    }
    let Request::CollectExecutableSource {
        call: cc,
        command,
        prepared_request,
    } = serde_json::from_slice(&c.body)?
    else {
        return Err(io::Error::other("executable group collection kind"));
    };
    if (cc, command, prepared_request) != (call, armed.raw, prepared) {
        return Err(io::Error::other(
            "executable group collection changed command",
        ));
    }
    let Reply::ExecutableSource(observed) = serde_json::from_slice(cb)? else {
        return Err(io::Error::other("executable group collection missing"));
    };
    intent.command = command;
    validate_collection(
        &observed,
        &intent,
        observed.raw.command.identity.provider,
        observed.raw.command.task,
        observed.raw.command.start_boottime,
    )
}

/// Controlled packaged-C premise used only by component tests. No loaded
/// provider, original ptracer observation or executable backing is certified.
#[cfg(test)]
pub(in crate::network_runtime) fn controlled_collection(intent: Intent) -> Observation<Effect> {
    let mapping = raw::Mapping {
        task: 19,
        start: 23,
        tracer: 29,
        tracer_start: 31,
        mm: 37,
        file: 41,
        exe_file: 41,
        inode: 43,
        mapping: 47,
        fops: 0xffffffff82a8bba8,
        vm_ops: 0xffffffff82a8bb20,
        filesystem: 0x9123683e,
        device: (8 << 20) | 17,
        inode_number: 53,
        file_size: 8192,
        vm_start: 0x401000,
        vm_end: 0x402000,
        vm_pgoff: 1,
        vm_flags: 5,
        file_mode: 1 | 32 | 0x02000000,
        inode_mode: 0o100755,
        writecount: -1,
        iovec_base: intent.registers,
        iovec_length: 216,
    };
    let raw = raw::Effect {
        command: super::ffi::CommandResult {
            command: intent.command,
            operation: 26,
            task: 19,
            start_boottime: 23,
            identity: super::ffi::Identity {
                provider: 17,
                object: 0,
                namespace: 0,
            },
            phase: 1,
            original_count: intent.length,
            ..Default::default()
        },
        receipt: raw::Receipt {
            intent: intent.into(),
            entered: mapping,
            returned: mapping,
            phases: 7,
            ..Default::default()
        },
    };
    Observation {
        status: super::CallStatus {
            operation: "ap_collect_executable_source".into(),
            returned: 0,
            errno: None,
        },
        raw: raw.into(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn intent() -> Intent {
        Intent {
            command: 7,
            registration: 11,
            owner_mm: 0,
            call: 13,
            address: 0x401020,
            length: 5,
            iovec: 0x700000,
            registers: 0x700100,
        }
    }
    #[test]
    fn executable_collection_requires_exact_original_and_positive_c_result() {
        let expected = intent();
        let good = controlled_collection(expected.clone());
        validate_collection(&good, &expected, 17, 19, 23).unwrap();
        for field in 0..25 {
            let mut observed = good.clone();
            match field {
                0 => observed.status.returned = -1,
                1 => observed.status.errno = Some(libc::EIO),
                2 => observed.status.operation = "different".into(),
                3 => observed.raw.command.command += 1,
                4 => observed.raw.command.operation = 24,
                5 => observed.raw.command.task += 1,
                6 => observed.raw.command.start_boottime += 1,
                7 => observed.raw.command.phase = 0,
                8 => observed.raw.command.identity.provider += 1,
                9 => observed.raw.command.identity.object = 1,
                10 => observed.raw.receipt.intent.registration += 1,
                11 => observed.raw.receipt.intent.owner_mm += 1,
                12 => observed.raw.receipt.intent.call += 1,
                13 => observed.raw.receipt.intent.address += 1,
                14 => observed.raw.receipt.intent.length += 1,
                15 => observed.raw.receipt.intent.iovec += 1,
                16 => observed.raw.receipt.intent.registers += 1,
                17 => observed.raw.receipt.returned.tracer += 1,
                18 => observed.raw.receipt.returned.tracer_start += 1,
                19 => observed.raw.receipt.phases = 3,
                20 => observed.raw.receipt.problem = 1,
                21 => observed.raw.receipt.ptrace_return = -1,
                22 => observed.raw.receipt.find_enter_return = -1,
                23 => observed.raw.receipt.find_exit_return = -1,
                24 => observed.raw.receipt.returned.iovec_length = 208,
                _ => unreachable!(),
            }
            let retained = observed.clone();
            assert!(
                validate_collection(&observed, &expected, 17, 19, 23).is_err(),
                "field {field}"
            );
            assert_eq!(
                observed, retained,
                "raw failure evidence must remain unchanged"
            );
        }
    }
}
