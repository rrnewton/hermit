//! Explicit ABI12 op27 values. Raw observations do not construct authority.
use std::ffi::c_int;
use std::io;

use super::CommandResult;
use super::SessionPtr;
pub(super) type Prepare = unsafe extern "C" fn(SessionPtr, c_int, *const Intent, *mut u64) -> c_int;
pub(super) type Collect =
    unsafe extern "C" fn(SessionPtr, c_int, u64, *mut CommandResult, *mut Receipt) -> c_int;
pub(super) type Validate =
    unsafe extern "C" fn(SessionPtr, *const CommandResult, *const Receipt) -> c_int;
pub(super) struct Api {
    pub prepare: Prepare,
    pub collect: Collect,
    pub validate: Validate,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Intent {
    pub command: u64,
    pub registration: u64,
    pub owner_mm: u64,
    pub normal_epoch: u64,
    pub expected_table: u64,
    pub expected_file: u64,
    pub fd: i32,
    pub reserved: u32,
    pub syscall_nr: u64,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Profile {
    pub task: u64,
    pub start: u64,
    pub tracer: u64,
    pub tracer_start: u64,
    pub mm: u64,
    pub table: u64,
    pub file: u64,
    pub raw_table: u64,
    pub raw_file: u64,
    pub socket: u64,
    pub sk: u64,
    pub inode: u64,
    pub file_ops: u64,
    pub file_flush: u64,
    pub file_release: u64,
    pub socket_ops: u64,
    pub socket_release: u64,
    pub protocol_ops: u64,
    pub protocol_close: u64,
    pub ulp_ops: u64,
    pub ulp_data: u64,
    pub file_ref_raw: u64,
    pub file_refs: u64,
    pub linger_ticks: u64,
    pub files_refs: u32,
    pub max_fds: u32,
    pub aliases: u32,
    pub family: u32,
    pub type_: u32,
    pub protocol: u32,
    pub repair: u32,
    pub linger: u32,
}
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Receipt {
    pub intent: Intent,
    pub entered: Profile,
    pub returned: Profile,
    pub phases: u64,
    pub problem: u64,
    pub ptrace_return: i64,
    pub iovec: u64,
    pub registers: u64,
    pub register_bytes: u64,
    pub original_nr: u64,
    pub original_fd: u64,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Effect {
    pub support: Option<super::CallStatus>,
    pub command: CommandResult,
    pub receipt: Receipt,
}

pub(super) fn read(
    wire: crate::network_runtime::ProviderWireFormat,
    call: impl FnOnce(*mut CommandResult, *mut Receipt) -> c_int,
    validate: impl FnOnce(&CommandResult, &Receipt) -> c_int,
) -> io::Result<super::Observation<Effect>> {
    if !wire.has_current_close_profile() {
        return Err(io::Error::other(
            "current Close profile requires authenticated ABI12-copy5",
        ));
    }
    let mut raw = Effect::default();
    let rc = call(&mut raw.command, &mut raw.receipt);
    let status = super::CallStatus::capture("ap_collect_current_close_profile", rc);
    if status.returned == 0 && status.errno.is_none() {
        let result = validate(&raw.command, &raw.receipt);
        raw.support = Some(super::CallStatus::capture(
            "ap_validate_current_close_profile",
            result,
        ));
    }
    Ok(super::Observation { status, raw })
}
const _: () = assert!(
    size_of::<Intent>() == 64 && size_of::<Profile>() == 224 && size_of::<Receipt>() == 576
);
