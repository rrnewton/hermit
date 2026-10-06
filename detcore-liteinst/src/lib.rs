//! In-guest Detcore runtime for the LiteInst backend.
//!
//! Hermit preloads this library into the guest for `--backend=liteinst`. When
//! the dynamic loader loads it, its constructor installs Detcore's `Tool`
//! inside the guest process before the program's `main`. The loader, the C library's initialization, `IFUNC`
//! resolvers and the constructors of the program's own shared libraries run
//! earlier, unmonitored. A loader that fails to load the library runs the
//! guest without it; Hermit reports that the guest never connected only after
//! the guest exits. Once Detcore is installed, patched system calls and the
//! SIGSYS fallback dispatch to Detcore in the guest, and Detcore's global
//! state stays in Hermit, reached over the coordinator RPC socket that
//! `reverie_liteinst::LiteinstBackend::run_with_preload` creates. No ptrace
//! tracer sits on the system-call path.
#![deny(missing_docs)]

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-3635): Review the
// in-guest Detcore constructor boundary.

/// Private opt-in, set by Hermit, for forwarding the in-guest Tool's
/// deterministic INFO records: the number of an inherited socket (the sending
/// end of a Unix `SOCK_SEQPACKET` pair) to send them on. The constructor
/// removes it from the guest's environment, moves the socket to a number
/// Reverie reserves and protects from the guest
/// (`reverie_liteinst::reserve_tool_output_fd`), and sends each record there
/// as one message, where Hermit's `--verify` reads them into the run's log. The
/// guest cannot write to, close or shut down that socket, so its own output
/// never mixes with the records.
pub const DETLOG_FORWARD_ENV: &str = "HERMIT_LITEINST_FORWARD_DETLOG";

/// Set by Hermit next to [`DETLOG_FORWARD_ENV`]: the encoded
/// `detcore::detlog::ForwardPolicy`, the CLI filter's INFO answer for each
/// target its `RUST_LOG` names and for any other. The forwarder sends a record
/// only when that policy logs its module, which is what each in-process
/// `detlog!` callsite asks tracing under ptrace. The constructor removes it
/// from the guest's environment.
pub const DETLOG_FORWARD_POLICY_ENV: &str = "HERMIT_LITEINST_FORWARD_DETLOG_POLICY";

/// Hermit validates that this constructor is registered in `.init_array`
/// before it preloads the library, so a DSO that would load without
/// installing Detcore is refused instead of running the guest unmonitored.
#[used]
#[unsafe(link_section = ".init_array")]
static DETCORE_LITEINST_INIT: unsafe extern "C" fn() = detcore_liteinst_initialize;

/// Installs Detcore in the current guest process.
///
/// The `.init_array` entry above runs this once per process image, before
/// `main`. It exits the process with status 127 when the coordinator socket is
/// missing or Detcore cannot be installed, so a guest never runs past a failed
/// installation.
///
/// # Safety
///
/// Only the dynamic loader may call this, from `.init_array`, while the process
/// is still single-threaded and before any seccomp filter is active.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn detcore_liteinst_initialize() {
    let Some(socket) = std::env::var_os(reverie_liteinst::COORDINATOR_ENV) else {
        fail("the coordinator socket environment variable is missing");
    };
    let forward_request = std::env::var_os(DETLOG_FORWARD_ENV);
    let forward_policy = std::env::var_os(DETLOG_FORWARD_POLICY_ENV);
    // SAFETY: the loader runs constructors while the process is still
    // single-threaded, so nothing reads the environment concurrently.
    unsafe {
        std::env::remove_var(DETLOG_FORWARD_ENV);
        std::env::remove_var(DETLOG_FORWARD_POLICY_ENV);
    }
    if let Some(value) = forward_request {
        let Some(fd) = value
            .to_str()
            .and_then(|value| value.parse::<libc::c_int>().ok())
        else {
            fail("the DETLOG forwarding descriptor is not a descriptor number");
        };
        // Hermit sends the policy with the descriptor; without a readable one
        // this process cannot forward the records ptrace would log.
        let policy = match forward_policy.as_deref().map(|value| {
            value
                .to_str()
                .ok_or_else(|| "it is not UTF-8".to_owned())
                .and_then(detcore::detlog::ForwardPolicy::decode)
        }) {
            Some(Ok(policy)) => policy,
            Some(Err(error)) => fail(&format!(
                "the DETLOG forwarding policy is unreadable: {error}"
            )),
            None => fail("the DETLOG forwarding policy is missing"),
        };
        // SAFETY: the process is still single-threaded and Detcore is not yet
        // installed, which is when the reservation must be made.
        match unsafe {
            reverie_liteinst::reserve_tool_output_fd(fd, detcore::detlog::FORWARDING_RETIRED_NOTICE)
        } {
            Ok(_) => {
                let _ = detcore::detlog::set_forwarder(forward_detlog, policy);
            }
            Err(error) => fail(&format!(
                "cannot reserve the DETLOG forwarding descriptor {fd}: {error}"
            )),
        }
    }
    // SAFETY: the loader runs constructors before any application thread
    // exists and before the application can install a seccomp filter, which is
    // the window `install_tool` requires.
    if let Err(error) = unsafe { reverie_liteinst::install_tool::<detcore::Detcore>(socket) } {
        fail(&error.to_string());
    }
}

/// Sends one Detcore record on the reserved socket. Its number can move when the
/// guest `dup2`s onto it, so it is read for each record.
fn forward_detlog(target: &str, record_suffix: &str, message: std::fmt::Arguments<'_>) {
    if let Some(socket) = reverie_liteinst::tool_output_fd() {
        detcore::detlog::send_forwarded_record(socket, target, record_suffix, message);
    }
}

fn fail(message: &str) -> ! {
    eprintln!("detcore-liteinst: initialization failed: {message}");
    // SAFETY: `_exit` takes no pointers and does not return.
    unsafe { libc::_exit(127) }
}
