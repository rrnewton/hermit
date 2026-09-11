use std::ffi::c_void;
use std::io;
use std::os::fd::BorrowedFd;
use std::sync::OnceLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use reverie_liteinst::startup::AuxvSnapshot;
use reverie_liteinst::startup::InterpreterImage;
use reverie_liteinst::startup::original_interpreter::OriginalInterpreter;

use crate::startup::Failure;

#[repr(C)]
struct InitialInputs {
    auxv: *const u8,
    auxv_bytes: usize,
    stack_begin: usize,
    stack_end: usize,
    original_fd: i32,
    original_brk: usize,
}

const _: () = assert!(std::mem::size_of::<InitialInputs>() == 48);
const _: () = assert!(std::mem::offset_of!(InitialInputs, original_brk) == 40);

unsafe extern "C" {
    fn pl_prepare_private_tls(context: *const c_void) -> i32;
    fn pl_initial_inputs(context: *const c_void, output: *mut InitialInputs) -> i32;
    fn pl_validate_original_destination(context: *const c_void, base: usize, span: usize) -> i32;
    fn pl_arm_initial(context: *const c_void, loader_entry: u64) -> i32;
    fn pl_restore_and_fault() -> !;
    static pl_context_hooks: reverie_preload::clock_boundary::ExecutionContext;
}

pub(super) struct Prepared {
    pub(super) context: usize,
    pub(super) original: OriginalInterpreter,
    pub(super) initial: AuxvSnapshot,
    pub(super) interpreter_entry: u64,
    pub(super) constructor_completed: AtomicBool,
}

pub(super) static PREPARED: OnceLock<Prepared> = OnceLock::new();

fn checked(stage: &'static str, result: i32) -> Result<(), Failure> {
    if result == 0 {
        Ok(())
    } else {
        let error = if (-4095..0).contains(&result) {
            io::Error::from_raw_os_error(-result)
        } else {
            io::Error::other("invalid private startup operation result")
        };
        Err(Failure::PrivateStartup(stage, error))
    }
}

unsafe fn map_pages(
    range: &std::ops::Range<u64>,
    protection: i32,
    fd: i32,
    offset: u64,
) -> Result<(), Failure> {
    if range.is_empty() {
        return Ok(());
    }
    let flags = libc::MAP_PRIVATE | libc::MAP_FIXED | if fd < 0 { libc::MAP_ANONYMOUS } else { 0 };
    let result = unsafe {
        reverie_preload::trap::raw_syscall6(
            libc::SYS_mmap,
            [
                range.start,
                range.end - range.start,
                protection as u64,
                flags as u64,
                fd as u64,
                offset,
            ],
        )
    };
    if result as u64 == range.start {
        Ok(())
    } else {
        let error = if (-4095..0).contains(&result) {
            io::Error::from_raw_os_error(-result as i32)
        } else {
            io::Error::other("original loader mapping returned a different address")
        };
        Err(Failure::PrivateStartup("original-mapping", error))
    }
}

unsafe fn map_original(
    context: *const c_void,
    original: &OriginalInterpreter,
    initial: &AuxvSnapshot,
    fd: i32,
) -> Result<u64, Failure> {
    let image = InterpreterImage::parse(original.bytes())
        .map_err(|error| Failure::PrivateStartup("original-layout", io::Error::other(error)))?;
    let span = image.required_span();
    let end = initial.base().checked_add(span).ok_or_else(|| {
        Failure::PrivateStartup(
            "original-layout",
            io::Error::other("original loader span overflow"),
        )
    })?;
    let reservation = initial.base()..end;
    let plan = original
        .plan(initial, reservation.clone(), &[])
        .map_err(|error| Failure::PrivateStartup("original-layout", io::Error::other(error)))?;
    checked("original-destination", unsafe {
        pl_validate_original_destination(context, reservation.start as usize, span as usize)
    })?;
    unsafe { map_pages(&reservation, libc::PROT_NONE, -1, 0) }?;
    for segment in plan.segments() {
        let flags = segment.flags();
        let protection = if flags & 4 != 0 { libc::PROT_READ } else { 0 }
            | if flags & 2 != 0 { libc::PROT_WRITE } else { 0 }
            | if flags & 1 != 0 { libc::PROT_EXEC } else { 0 };
        if let Some((offset, pages)) = segment.file_pages() {
            unsafe { map_pages(pages, protection, fd, *offset) }?;
        }
        unsafe { map_pages(segment.anonymous_pages(), protection, -1, 0) }?;
        let zero = segment.zero_fill();
        if !zero.is_empty() {
            unsafe {
                std::ptr::write_bytes(zero.start as *mut u8, 0, (zero.end - zero.start) as usize)
            };
        }
    }
    Ok(plan.interpreter_entry())
}

fn validate_original_brk(original: usize, current: i64) -> Result<(), Failure> {
    if original == 0 || original >= 1 << 47 || current <= 0 {
        return Err(Failure::PrivateStartup(
            "original-brk",
            io::Error::other("invalid retained or observed program break"),
        ));
    }
    if current as u64 != original as u64 {
        return Err(Failure::PrivateStartup(
            "original-brk",
            io::Error::other("program break changed since pre-CRT acquisition"),
        ));
    }
    Ok(())
}

unsafe fn prepare(context: *const c_void) -> Result<(), Failure> {
    checked("private-tls", unsafe { pl_prepare_private_tls(context) })?;
    let provider = unsafe { reverie_liteinst::__PrivateGnuStartup::prepare(context) }
        .map_err(|error| Failure::PrivateStartup("gnu-root", io::Error::other(error)))?;
    let mut output = std::mem::MaybeUninit::<InitialInputs>::uninit();
    checked("original-input", unsafe {
        pl_initial_inputs(context, output.as_mut_ptr())
    })?;
    let inputs = unsafe { output.assume_init() };
    let original =
        OriginalInterpreter::acquire(unsafe { BorrowedFd::borrow_raw(inputs.original_fd) })
            .map_err(|error| Failure::PrivateStartup("original-interpreter", error))?;
    let brk = unsafe { reverie_preload::trap::raw_syscall6(libc::SYS_brk, [0; 6]) };
    validate_original_brk(inputs.original_brk, brk)?;
    let initial = AuxvSnapshot::parse(
        unsafe { std::slice::from_raw_parts(inputs.auxv, inputs.auxv_bytes) },
        inputs.stack_begin as u64..inputs.stack_end as u64,
        inputs.original_brk as u64,
    )
    .map_err(|error| Failure::PrivateStartup("original-auxv", io::Error::other(error)))?;
    let interpreter_entry =
        unsafe { map_original(context, &original, &initial, inputs.original_fd) }?;
    unsafe { reverie_liteinst::vdso::prepare_private() }
        .map_err(|error| Failure::PrivateStartup("retained-vdso", error))?;
    unsafe {
        reverie_liteinst::mapping::prepare_private(
            &initial,
            &original,
            BorrowedFd::borrow_raw(inputs.original_fd),
        )
    }
    .map_err(|error| Failure::PrivateStartup("guest-mapping-owner", error))?;
    provider.close().map_err(|error| {
        Failure::PrivateStartup("gnu-publication-close", io::Error::other(error))
    })?;
    unsafe { reverie_preload::clock_boundary::register_execution_context(&pl_context_hooks) }
        .map_err(|error| Failure::PrivateStartup("private-context-registration", error))?;
    PREPARED
        .set(Prepared {
            context: context as usize,
            original,
            initial,
            interpreter_entry,
            constructor_completed: AtomicBool::new(false),
        })
        .map_err(|_| {
            Failure::PrivateStartup(
                "private-owner",
                io::Error::other("private owner already prepared"),
            )
        })?;
    Ok(())
}

unsafe fn terminal(error: Failure) -> ! {
    let _ = unsafe { crate::startup::finish_initializer(Err(error)) };
    let _ =
        unsafe { reverie_preload::trap::raw_syscall6(libc::SYS_exit_group, [127, 0, 0, 0, 0, 0]) };
    loop {
        core::hint::spin_loop();
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn pe_private_prepare_runtime(context: *const c_void) -> i32 {
    match unsafe { prepare(context) } {
        Ok(()) => 0,
        Err(error) => unsafe { terminal(error) },
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn pe_private_handoff_to_interpreter(context: *const c_void) -> ! {
    let result = (|| -> Result<(), Failure> {
        let owner = PREPARED.get().ok_or_else(|| {
            Failure::PrivateStartup(
                "private-handoff",
                io::Error::other("private owner is not prepared"),
            )
        })?;
        if owner.context != context as usize
            || !owner.constructor_completed.swap(false, Ordering::AcqRel)
        {
            return Err(Failure::PrivateStartup(
                "private-handoff",
                io::Error::other("actual private Tool constructor did not complete exactly once"),
            ));
        }
        let image = InterpreterImage::parse(owner.original.bytes())
            .map_err(|error| Failure::PrivateStartup("original-layout", io::Error::other(error)))?;
        let end = owner
            .initial
            .base()
            .checked_add(image.required_span())
            .ok_or_else(|| {
                Failure::PrivateStartup(
                    "original-layout",
                    io::Error::other("original span overflow"),
                )
            })?;
        let plan = owner
            .original
            .plan(&owner.initial, owner.initial.base()..end, &[])
            .map_err(|error| Failure::PrivateStartup("original-layout", io::Error::other(error)))?;
        if plan.interpreter_entry() != owner.interpreter_entry {
            return Err(Failure::PrivateStartup(
                "original-layout",
                io::Error::other("retained loader entry changed"),
            ));
        }
        unsafe { reverie_liteinst::__prepare_private_startup() }
            .map_err(|error| Failure::PrivateStartup("initial-runtime", error))?;
        checked("initial-transfer", unsafe {
            pl_arm_initial(context, owner.interpreter_entry)
        })
    })();
    if let Err(error) = result {
        unsafe { terminal(error) };
    }
    unsafe { pl_restore_and_fault() }
}

unsafe extern "C" fn initialize_private() -> i32 {
    if PREPARED.get().is_none() {
        unsafe {
            terminal(Failure::PrivateStartup(
                "private-owner",
                io::Error::other("private constructor has no prepared owner"),
            ))
        };
    }
    match unsafe { crate::initialize_selected() } {
        Ok(1) => 1,
        Ok(_) => unsafe {
            terminal(Failure::PrivateStartup(
                "private-selection",
                io::Error::other("private CRT requires the actual Detcore constructor"),
            ))
        },
        Err(error) => unsafe { terminal(error) },
    }
}

unsafe extern "C" fn finish_private() {
    if let Err(error) = unsafe { reverie_liteinst::__defer_private_constructor() } {
        unsafe { terminal(Failure::PrivateStartup("private-clock-domain", error)) };
    }
    let owner = PREPARED.get().expect("checked private owner");
    if owner.constructor_completed.swap(true, Ordering::AcqRel) {
        unsafe {
            terminal(Failure::PrivateStartup(
                "private-constructor",
                io::Error::other("private constructor completed twice"),
            ))
        };
    }
}

#[cfg(not(test))]
#[used]
#[unsafe(link_section = ".init_array")]
pub(super) static HERMIT_LITEINST_INIT: unsafe extern "C" fn() = {
    #[unsafe(naked)]
    unsafe extern "C" fn entry() {
        core::arch::naked_asm!(
            "sub rsp, 8", "call {begin}", "call {initialize}",
            "call {finish}", "add rsp, 8", "ret",
            begin = sym reverie_liteinst::__clock_constructor_begin,
            initialize = sym initialize_private,
            finish = sym finish_private,
        );
    }
    entry
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_original_brk_accepts_only_unchanged_valid_state() {
        for original in [1, 0x12345000, 0x12345001, (1usize << 47) - 1] {
            assert!(validate_original_brk(original, original as i64).is_ok());
        }
    }

    #[test]
    fn retained_original_brk_refuses_growth_shrinkage_and_invalid_state() {
        for (original, current, message) in [
            (
                0x12345000,
                0x12346000,
                "program break changed since pre-CRT acquisition",
            ),
            (
                0x12345000,
                0x12344000,
                "program break changed since pre-CRT acquisition",
            ),
            (0, 0, "invalid retained or observed program break"),
            (0, 0x12345000, "invalid retained or observed program break"),
            (
                1usize << 47,
                1i64 << 47,
                "invalid retained or observed program break",
            ),
            (0x12345000, 0, "invalid retained or observed program break"),
            (
                0x12345000,
                -libc::ENOMEM as i64,
                "invalid retained or observed program break",
            ),
        ] {
            let Err(Failure::PrivateStartup(stage, error)) =
                validate_original_brk(original, current)
            else {
                panic!("invalid or changed brk must refuse");
            };
            assert_eq!(stage, "original-brk");
            assert_eq!(error.to_string(), message);
        }
    }
}
