//! LiteInst's use of the in-guest fallback continuation, which lives in
//! [`reverie_inguest::guest::continuation`]: it registers the runtime's
//! dispatch functions as the continuation's handlers and keeps the two
//! observation symbols the fixtures look up by name.

use std::io;
use std::sync::OnceLock;

use liteinst2::trampoline::HookContext;
use reverie_inguest::guest::context::RegisterContext;
use reverie_inguest::guest::continuation;
pub(crate) use reverie_inguest::guest::continuation::rebind_fork_child;
use reverie_inguest::guest::event::InstructionEventKind;

/// LiteInst's one registration of the continuation handlers, kept so that a
/// retried installation reuses it while a registration by another backend is
/// still refused.
static REGISTRATION: OnceLock<Result<(), io::ErrorKind>> = OnceLock::new();

/// Registers LiteInst's dispatch functions as the continuation's handlers
/// (once per process) and prepares this thread's continuation.
pub(crate) fn initialize() -> io::Result<()> {
    let region = reverie_inguest::guest::tool_region::ToolRegion::reserve()?;
    let registration = REGISTRATION.get_or_init(|| {
        // SAFETY: both handlers only pass the continuation's register block
        // on to the runtime's dispatch, which uses it for the call and keeps
        // nothing.
        unsafe {
            continuation::register_handlers(continuation::ContinuationHandlers {
                syscall: dispatch_syscall,
                instruction: dispatch_instruction,
            })
        }
        .map_err(|error| error.kind())
    });
    if let Err(kind) = registration {
        return Err(io::Error::new(
            *kind,
            "the in-guest continuation's handlers are registered by another backend",
        ));
    }
    continuation::initialize_with_stack_backing(
        reverie_inguest::guest::tool_region::StackBacking::ToolRegion(region),
    )
}

// HookContext and RegisterContext have the same layout (checked in
// tool_host.rs), so the continuation's context is passed on by a cast.
unsafe fn dispatch_syscall(context: *mut RegisterContext, pkru: &mut Option<u32>) {
    unsafe { crate::runtime::dispatch_fallback_context(context.cast::<HookContext>(), pkru) };
}

unsafe fn dispatch_instruction(context: *mut RegisterContext, kind: InstructionEventKind) {
    unsafe { crate::runtime::dispatch_fallback_instruction(context.cast::<HookContext>(), kind) };
}

/// Diagnostic observations for the current thread's owned continuation.
/// Selectors 0/1/2 are actual fallback entry, reached ordinary callback, and
/// prepared genuine completion frame. They are not a kernel-stop census.
#[unsafe(no_mangle)]
pub extern "C" fn reverie_liteinst_owned_fallback_observation(selector: u32) -> u64 {
    continuation::owned_observation(selector)
}

/// Check a fixture's actual callback local address against owned stack bounds.
#[unsafe(no_mangle)]
pub extern "C" fn reverie_liteinst_on_owned_fallback_stack(address: usize) -> bool {
    continuation::on_owned_stack(address)
}
