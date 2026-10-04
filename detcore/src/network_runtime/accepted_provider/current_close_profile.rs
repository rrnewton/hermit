//! Private current-Close transaction. These serializable values are evidence,
//! not a current scheduler grant or a finite-Close capability.
use std::io;

use serde::Deserialize;
use serde::Serialize;

use super::CommandResult;
use super::Observation;
use super::ffi::current_close_profile as raw;
wire!(pub(in crate::network_runtime) Intent,raw::Intent,{command:u64,registration:u64,owner_mm:u64,normal_epoch:u64,expected_table:u64,expected_file:u64,fd:i32,reserved:u32,syscall_nr:u64});
impl From<Intent> for raw::Intent {
    fn from(v: Intent) -> Self {
        Self {
            command: v.command,
            registration: v.registration,
            owner_mm: v.owner_mm,
            normal_epoch: v.normal_epoch,
            expected_table: v.expected_table,
            expected_file: v.expected_file,
            fd: v.fd,
            reserved: v.reserved,
            syscall_nr: v.syscall_nr,
        }
    }
}
wire!(pub(in crate::network_runtime) Profile,raw::Profile,{task:u64,start:u64,tracer:u64,tracer_start:u64,mm:u64,table:u64,file:u64,raw_table:u64,raw_file:u64,socket:u64,sk:u64,inode:u64,file_ops:u64,file_flush:u64,file_release:u64,socket_ops:u64,socket_release:u64,protocol_ops:u64,protocol_close:u64,ulp_ops:u64,ulp_data:u64,file_ref_raw:u64,file_refs:u64,linger_ticks:u64,files_refs:u32,max_fds:u32,aliases:u32,family:u32,type_:u32,protocol:u32,repair:u32,linger:u32});
impl From<Profile> for raw::Profile {
    fn from(v: Profile) -> Self {
        Self {
            task: v.task,
            start: v.start,
            tracer: v.tracer,
            tracer_start: v.tracer_start,
            mm: v.mm,
            table: v.table,
            file: v.file,
            raw_table: v.raw_table,
            raw_file: v.raw_file,
            socket: v.socket,
            sk: v.sk,
            inode: v.inode,
            file_ops: v.file_ops,
            file_flush: v.file_flush,
            file_release: v.file_release,
            socket_ops: v.socket_ops,
            socket_release: v.socket_release,
            protocol_ops: v.protocol_ops,
            protocol_close: v.protocol_close,
            ulp_ops: v.ulp_ops,
            ulp_data: v.ulp_data,
            file_ref_raw: v.file_ref_raw,
            file_refs: v.file_refs,
            linger_ticks: v.linger_ticks,
            files_refs: v.files_refs,
            max_fds: v.max_fds,
            aliases: v.aliases,
            family: v.family,
            type_: v.type_,
            protocol: v.protocol,
            repair: v.repair,
            linger: v.linger,
        }
    }
}
wire!(pub(in crate::network_runtime) Receipt,raw::Receipt,{intent:Intent,entered:Profile,returned:Profile,phases:u64,problem:u64,ptrace_return:i64,iovec:u64,registers:u64,register_bytes:u64,original_nr:u64,original_fd:u64});
impl From<Receipt> for raw::Receipt {
    fn from(v: Receipt) -> Self {
        Self {
            intent: v.intent.into(),
            entered: v.entered.into(),
            returned: v.returned.into(),
            phases: v.phases,
            problem: v.problem,
            ptrace_return: v.ptrace_return,
            iovec: v.iovec,
            registers: v.registers,
            register_bytes: v.register_bytes,
            original_nr: v.original_nr,
            original_fd: v.original_fd,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(in crate::network_runtime) struct Effect {
    pub command: CommandResult,
    pub receipt: Receipt,
    pub support: Option<super::CallStatus>,
}
impl From<raw::Effect> for Effect {
    fn from(v: raw::Effect) -> Self {
        Self {
            command: v.command.into(),
            receipt: v.receipt.into(),
            support: v.support.map(Into::into),
        }
    }
}

impl Intent {
    pub(in crate::network_runtime) fn valid_unarmed(&self) -> bool {
        self.command == 0
            && self.registration != 0
            && self.expected_table != 0
            && self.expected_file != 0
            && self.fd >= 0
            && self.fd < 256
            && self.reserved == 0
            && self.syscall_nr == 3
    }
}
/// Custody predicate, independent of supported physical policy. A complete
/// unsafe observation can be ACKed, but cannot issue a Completed capability.
pub(in crate::network_runtime) fn validate_collection(
    observed: &Observation<Effect>,
    expected: &Intent,
    provider: u64,
    task: u64,
    start: u64,
) -> io::Result<()> {
    let e = &observed.raw;
    let c = &e.command;
    let r = &e.receipt;
    let mut unarmed = expected.clone();
    unarmed.command = 0;
    if observed.status.operation != "ap_collect_current_close_profile"
        || observed.status.returned != 0
        || observed.status.errno.is_some()
        || !unarmed.valid_unarmed()
        || expected.command == 0
        || r.intent != *expected
        || c.command != expected.command
        || c.operation != 27
        || c.phase != 1
        || c.returned != 0
        || c.reserved != 0
        || c.original_count != 0
        || provider == 0
        || task == 0
        || start == 0
        || c.identity.provider != provider
        || c.identity.object != 0
        || c.identity.namespace != 0
        || c.task != task
        || c.start_boottime != start
        || c.creation != 0
        || c.cookie != 0
        || c.state != super::ffi::RawState::default().into()
        || r.phases != 7
        || r.ptrace_return != 0
        || r.entered.task != task
        || r.entered.start != start
        || r.entered.tracer == 0
        || r.entered.tracer_start == 0
        || r.entered.mm == 0
        || !owned_same(&r.entered, &r.returned)
        || r.problem & !4 != 0
        || !owned(&r.entered, expected)
        || !owned(&r.returned, expected)
        || r.entered.table != expected.expected_table
        || r.entered.file != expected.expected_file
        || r.entered.raw_table == 0
        || r.entered.raw_file == 0
        || r.iovec == 0
        || r.registers == 0
        || r.register_bytes != 216
        || r.original_nr != 3
        || r.original_fd != expected.fd as u64
    {
        return Err(io::Error::other(
            "current Close collection changed actual command/owner/entry/return",
        ));
    }
    Ok(())
}
fn owned(p: &Profile, i: &Intent) -> bool {
    p.task != 0
        && p.start != 0
        && p.tracer != 0
        && p.tracer_start != 0
        && p.mm != 0
        && p.table == i.expected_table
        && p.file == i.expected_file
        && p.raw_table != 0
        && p.raw_file != 0
        && p.inode != 0
        && p.file_ops != 0
        && p.files_refs == 1
        && p.max_fds != 0
        && p.max_fds <= 256
        && i.fd >= 0
        && (i.fd as u32) < p.max_fds
        && p.aliases != 0
        && p.aliases <= p.max_fds
        && p.file_ref_raw <= 0x7fff_ffff_ffff_ffff
        && p.file_ref_raw.checked_add(1) == Some(p.file_refs)
        && p.file_refs == u64::from(p.aliases)
}
fn owned_same(a: &Profile, b: &Profile) -> bool {
    a.task == b.task
        && a.start == b.start
        && a.tracer == b.tracer
        && a.tracer_start == b.tracer_start
        && a.mm == b.mm
        && a.table == b.table
        && a.file == b.file
        && a.raw_table == b.raw_table
        && a.raw_file == b.raw_file
        && a.inode == b.inode
        && a.files_refs == b.files_refs
        && a.max_fds == b.max_fds
        && a.aliases == b.aliases
        && a.file_ref_raw == b.file_ref_raw
        && a.file_refs == b.file_refs
}
pub(in crate::network_runtime) fn validate_supported(
    observed: &Observation<Effect>,
) -> io::Result<()> {
    let r = &observed.raw.receipt;
    let support = observed.raw.support.as_ref().ok_or_else(|| {
        io::Error::other("current Close lacks actual anchored C support validation")
    })?;
    if support.operation != "ap_validate_current_close_profile"
        || support.returned != 0
        || support.errno.is_some()
        || r.problem != 0
        || r.entered.socket != r.returned.socket
        || r.entered.sk != r.returned.sk
    {
        return Err(io::Error::other(
            "current Close physical profile is unsupported; logical foreground selection is retained",
        ));
    }
    for p in [&r.entered, &r.returned] {
        if p.family != libc::AF_INET as u32
            || p.type_ != libc::SOCK_STREAM as u32
            || p.protocol != libc::IPPROTO_TCP as u32
            || p.linger != 0
            || p.repair != 0
            || p.ulp_ops != 0
            || p.ulp_data != 0
            || p.file_flush != 0
            || [
                p.socket,
                p.sk,
                p.inode,
                p.file_ops,
                p.file_release,
                p.socket_ops,
                p.socket_release,
                p.protocol_ops,
                p.protocol_close,
            ]
            .contains(&0)
        {
            return Err(io::Error::other(
                "current Close changed supported stable configuration",
            ));
        }
    }
    // The actual C validation above independently authenticates function
    // pointers against the session anchor; nonzero pointers alone are not proof.
    Ok(())
}

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
        return Err(io::Error::other(
            "current_close group sequence/count mismatch",
        ));
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
        || p.operation != Operation::PrepareCurrentCloseProfile
        || c.operation != Operation::CollectCurrentCloseProfile
    {
        return Err(io::Error::other(
            "current_close group changed owned target/frames",
        ));
    }
    let Request::PrepareCurrentCloseProfile {
        call: retained_call,
        mut intent,
    } = serde_json::from_slice(&p.body)?
    else {
        return Err(io::Error::other("current_close group preparation kind"));
    };
    let Reply::Prepared(armed) = serde_json::from_slice(pb)? else {
        return Err(io::Error::other("current_close group installation missing"));
    };
    if !intent.valid_unarmed()
        || intent.owner_mm != owner.mm.generation()
        || retained_call != call
        || armed.status.operation != "ap_prepare_current_close_profile"
        || armed.status.returned != 0
        || armed.status.errno.is_some()
        || armed.raw == 0
    {
        return Err(io::Error::other("current_close group installation changed"));
    }
    let Request::CollectCurrentCloseProfile {
        call: cc,
        command,
        prepared_request,
    } = serde_json::from_slice(&c.body)?
    else {
        return Err(io::Error::other("current_close group collection kind"));
    };
    if (cc, command, prepared_request) != (call, armed.raw, prepared) {
        return Err(io::Error::other(
            "current_close group collection changed command",
        ));
    }
    let Reply::CurrentCloseProfile(observed) = serde_json::from_slice(cb)? else {
        return Err(io::Error::other("current_close group collection missing"));
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

/// Controlled packaged-C premise, never a kernel observation/capability.
#[cfg(test)]
pub(in crate::network_runtime) fn controlled_collection(
    intent: Intent,
    provider: u64,
    task: u64,
    start: u64,
) -> Observation<Effect> {
    let p = Profile {
        task,
        start,
        tracer: 29,
        tracer_start: 31,
        mm: 37,
        table: intent.expected_table,
        file: intent.expected_file,
        raw_table: 41,
        raw_file: 43,
        socket: 47,
        sk: 53,
        inode: 59,
        file_ops: 61,
        file_flush: 0,
        file_release: 67,
        socket_ops: 71,
        socket_release: 73,
        protocol_ops: 79,
        protocol_close: 83,
        ulp_ops: 0,
        ulp_data: 0,
        file_ref_raw: 1,
        file_refs: 2,
        linger_ticks: 0,
        files_refs: 1,
        max_fds: 64,
        aliases: 2,
        family: 2,
        type_: 1,
        protocol: 6,
        repair: 0,
        linger: 0,
    };
    Observation {
        status: super::CallStatus {
            operation: "ap_collect_current_close_profile".into(),
            returned: 0,
            errno: None,
        },
        raw: Effect {
            command: super::ffi::CommandResult {
                command: intent.command,
                operation: 27,
                task,
                start_boottime: start,
                identity: super::ffi::Identity {
                    provider,
                    object: 0,
                    namespace: 0,
                },
                phase: 1,
                ..Default::default()
            }
            .into(),
            receipt: Receipt {
                original_fd: intent.fd as u64,
                intent,
                entered: p.clone(),
                returned: p,
                phases: 7,
                problem: 0,
                ptrace_return: 0,
                iovec: 0x700000,
                registers: 0x700100,
                register_bytes: 216,
                original_nr: 3,
            },
            support: Some(super::CallStatus {
                operation: "ap_validate_current_close_profile".into(),
                returned: 0,
                errno: None,
            }),
        },
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
            normal_epoch: 0,
            expected_table: 5,
            expected_file: 7,
            fd: 4,
            reserved: 0,
            syscall_nr: 3,
        }
    }
    #[test]
    fn current_close_settled_collection_requires_exact_owner_tuple_and_alias_census() {
        let i = intent();
        let good = controlled_collection(i.clone(), 17, 19, 23);
        validate_collection(&good, &i, 17, 19, 23).unwrap();
        validate_supported(&good).unwrap();
        for field in 0..28 {
            let mut v = good.clone();
            match field {
                0 => v.status.returned = -1,
                1 => v.status.errno = Some(libc::EIO),
                2 => v.status.operation = "wrong".into(),
                3 => v.raw.command.command += 1,
                4 => v.raw.command.operation = 6,
                5 => v.raw.command.phase = 0,
                6 => v.raw.command.task += 1,
                7 => v.raw.command.start_boottime += 1,
                8 => v.raw.command.identity.provider += 1,
                9 => v.raw.receipt.intent.normal_epoch += 1,
                10 => v.raw.receipt.intent.registration += 1,
                11 => v.raw.receipt.intent.expected_file += 1,
                12 => v.raw.receipt.intent.fd += 1,
                13 => v.raw.receipt.phases = 3,
                14 => v.raw.receipt.problem = 1,
                15 => v.raw.receipt.ptrace_return = -1,
                16 => v.raw.receipt.returned.raw_file += 1,
                17 => v.raw.receipt.returned.raw_table += 1,
                18 => v.raw.receipt.entered.files_refs = 2,
                19 => v.raw.receipt.entered.file_refs = 3,
                20 => v.raw.receipt.entered.file_ref_raw = u64::MAX,
                21 => v.raw.receipt.entered.aliases = 0,
                22 => v.raw.receipt.entered.max_fds = 257,
                23 => v.raw.receipt.original_fd += 1,
                24 => v.raw.receipt.original_nr = 1,
                25 => v.raw.receipt.register_bytes = 208,
                26 => v.raw.receipt.registers = 0,
                27 => v.raw.receipt.returned.tracer += 1,
                _ => unreachable!(),
            }
            let before = v.clone();
            assert!(
                validate_collection(&v, &i, 17, 19, 23).is_err(),
                "field {field}"
            );
            assert_eq!(v, before);
        }
    }
    #[test]
    fn current_close_unsafe_profile_is_settled_but_never_supported_or_a_fallback() {
        let i = intent();
        let mut v = controlled_collection(i.clone(), 17, 19, 23);
        v.raw.receipt.problem = 4;
        v.raw.receipt.entered.linger = 1;
        v.raw.receipt.returned.linger = 1;
        v.raw.support.as_mut().unwrap().returned = -1;
        v.raw.support.as_mut().unwrap().errno = Some(libc::EOPNOTSUPP);
        validate_collection(&v, &i, 17, 19, 23).unwrap();
        assert!(validate_supported(&v).is_err());
        for mutation in 0..5 {
            let mut v = controlled_collection(i.clone(), 17, 19, 23);
            match mutation {
                0 => v.raw.support = None,
                1 => v.raw.support.as_mut().unwrap().errno = Some(libc::ESTALE),
                2 => v.raw.receipt.returned.ulp_ops = 99,
                3 => v.raw.receipt.returned.repair = 1,
                4 => v.raw.receipt.returned.protocol_close = 0,
                _ => unreachable!(),
            }
            assert!(validate_supported(&v).is_err(), "mutation {mutation}");
        }
    }
}
