//! The service is a dedicated process owner. Rust stack unwinding must never
//! become the final ordinary close of a retained guest TCP socket: Linux gives
//! process-exit release different linger and signal semantics.

use std::ffi::CString;
use std::io;
use std::io::Write;
use std::mem::ManuallyDrop;
use std::num::NonZeroU64;
use std::os::fd::OwnedFd;
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use std::time::Duration;
use std::time::Instant;

use serde_json::Value;

use super::super::accepted_provider::CallStatus;
use super::super::accepted_provider_ffi as ffi;
use super::super::accepted_transport::SessionCustody;
use super::AcceptedProviderService;

fn inventory(receipt: &ffi::Inventory) -> Value {
    serde_json::json!({
        "status": CallStatus::from(receipt.status),
        "ids": receipt.ids.iter().map(|id| serde_json::json!({"kind": id.kind, "id": id.id})).collect::<Vec<_>>(),
        "count_invalid": receipt.count_invalid,
        "complete": receipt.complete(),
    })
}

fn close_receipt(receipt: &ffi::CloseReceipt) -> Value {
    serde_json::json!({
        "incarnation": receipt.incarnation,
        "inventory": inventory(&receipt.inventory),
        "close": CallStatus::from(receipt.close),
        "unexpected_drop": receipt.unexpected_drop,
        "requires_external_absence": receipt.requires_external_absence,
    })
}

fn unresolved(custody: &SessionCustody) -> bool {
    custody.incoming_unfinished != 0
        || custody.outgoing_unacknowledged != 0
        || custody.command_ack_unknown != 0
        || custody.quarantined_messages != 0
}

/// stdout belongs to the owning launcher. A regular-file sink receives fsync
/// before the process releases anything; pipe sinks require the launcher's
/// independent durable readback. Neither successful write nor ap_close proves
/// that the exact kernel resource IDs are absent.
fn report(value: &Value) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    let mut stdout = io::stdout().lock();
    stdout.write_all(&bytes)?;
    stdout.flush()?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { libc::fstat(libc::STDOUT_FILENO, stat.as_mut_ptr()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if unsafe { stat.assume_init() }.st_mode & libc::S_IFMT == libc::S_IFREG
        && unsafe { libc::fsync(libc::STDOUT_FILENO) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

struct ControllerTerminal;

pub(super) fn monotonic_ns() -> Option<u64> {
    let mut clock: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut clock) } != 0 {
        return None;
    }
    (clock.tv_sec as u64)
        .checked_mul(1_000_000_000)
        .and_then(|value| value.checked_add(clock.tv_nsec as u64))
}

/// Grouped peers bound release by their own first controller-exit observation,
/// which is never earlier than the exit. `live_before` was sampled before a poll
/// that still saw the controller live, so it precedes every such observation and
/// its one second never extends theirs. The original start's second still caps it.
fn grouped_cutoff(start: u64, live_before: Option<u64>) -> Option<u64> {
    let full = start.checked_add(1_000_000_000)?;
    Some(
        live_before
            .and_then(|live| live.checked_add(1_000_000_000))
            .map_or(full, |bound| bound.min(full)),
    )
}

fn exit_after_controller(
    service: &mut AcceptedProviderService,
    _: ControllerTerminal,
    live_before: Option<u64>,
) -> ! {
    // The only constructor is the actual retained-pidfd observation below.
    // Exit is monotonic for that pinned task; EOF and transport/wrapper errors
    // never construct this proof or authorize the irreversible close.
    let bootstrap = service.bootstrap.terminal_custody();
    let run = service
        .run
        .as_ref()
        .map(|session| session.terminal_custody());
    let provider_state = service.provider.terminal_state();
    let inventories = service.provider.terminal_inventory();
    let mut successful = service.failure.is_none()
        && service.bootstrap_sent
        && !unresolved(&bootstrap)
        && run.as_ref().is_some_and(|custody| !unresolved(custody))
        && service.observation.is_none()
        && service.last_observation.is_none()
        && service.fd_observation.is_none()
        && service.last_fd_observation.is_none()
        && service.run_replies.is_empty()
        && provider_state["active_setters"] == 0
        && provider_state["unresolved_matches"] == 0
        && inventories.iter().all(ffi::Inventory::complete);
    let before = serde_json::json!({
        "schema": "hermit-accepted-provider-terminal-v1",
        "phase": "before_close",
        "fd_journal_pending":service.fd_observation.as_ref().map(|(request,event,_)|serde_json::json!({"request":request,"event":event})),
        "fd_journal_unretired":service.last_fd_observation,
        "fd_journal_last_raw_probe":service.last_fd_probe.as_ref().map(|body|serde_json::from_slice::<Value>(body).unwrap_or_else(|_|serde_json::json!({"invalid_raw_bytes":body}))),
        "run": service.incarnation,
        "controller_terminal": true,
        "controller_live_before_ns": live_before,
        "run_peer_ended": service.run_peer_ended(),
        "failure": service.failure,
        "bootstrap": bootstrap,
        "run_custody": run,
        "pending_observation": service.observation.as_ref().map(|pending| pending.request),
        "last_observation": service.last_observation,
        "unsent_replies": service.run_replies,
        "provider": provider_state,
        "inventories": inventories.iter().map(inventory).collect::<Vec<_>>(),
        "requires_external_absence": true,
    });
    successful &= report(&before).is_ok();
    // The independent parent query uses this original monotonic close boundary,
    // never a fresh deadline measured after it receives the report.
    let closed_ns = monotonic_ns();
    successful &= closed_ns.is_some();
    let mut grouped_close = None;
    let closed = if service.grouped_installed {
        let result = closed_ns
            .ok_or_else(|| io::Error::other("original grouped close clock unavailable"))
            .and_then(|start| {
                grouped_cutoff(start, live_before)
                    .ok_or_else(|| io::Error::other("original grouped close cutoff overflow"))
                    .and_then(|cutoff| {
                        service
                            .provider
                            .close_grouped_for_process_exit(start, cutoff)
                    })
            });
        // The native result remains visible even if subsequent runtime custody
        // or absence verification fails. It never replaces the primary error.
        grouped_close = service.provider.grouped_close_receipt().cloned();
        result.and_then(|receipt| {
            if receipt.native_pointer_retained || !receipt.runtime_completed {
                return Err(io::Error::other(
                    "grouped close did not complete actual runtime retirement",
                ));
            }
            let inventory = receipt.inventory.ok_or_else(|| {
                io::Error::other("grouped unopened retirement has no provider inventory")
            })?;
            Ok(vec![ffi::CloseReceipt {
                incarnation: receipt.incarnation,
                inventory,
                close: receipt.close,
                unexpected_drop: false,
                requires_external_absence: receipt.requires_external_absence,
            }])
        })
    } else {
        service.provider.close_for_process_exit()
    };
    successful &= closed.as_ref().is_ok_and(|receipts| {
        !receipts.is_empty()
            && receipts.iter().all(|receipt| {
                receipt.inventory.complete()
                    && receipt.close.succeeded()
                    && !receipt.unexpected_drop
            })
    });
    let final_report = serde_json::json!({
        "schema": "hermit-accepted-provider-terminal-v1",
        "phase": "after_close",
        "closed_ns": closed_ns,
        "run": service.incarnation,
        "controller_terminal": true,
        "close_receipts": closed.as_ref().ok().map(|receipts| receipts.iter().map(close_receipt).collect::<Vec<_>>()),
        "close_error": closed.as_ref().err().map(ToString::to_string),
        "grouped_close": grouped_close.as_ref().map(|receipt| serde_json::json!({
            "incarnation": receipt.incarnation,
            "inventory": receipt.inventory.as_ref().map(inventory),
            "close": CallStatus::from(receipt.close),
            "original_release_start": receipt.original_release_start,
            "cutoff": receipt.cutoff,
            "native_pointer_retained": receipt.native_pointer_retained,
            "runtime_completed": receipt.runtime_completed,
            "requires_external_absence": receipt.requires_external_absence,
        })),
        "service_status": if successful { 0 } else { 125 },
        "requires_external_absence": true,
        "socket_release": "pending_process_exit",
    });
    successful &= report(&final_report).is_ok();
    // The process owns all inbox/outbox/quarantine capabilities until here.
    // Do not drop the service, its library, or any socket-right container.
    unsafe { libc::_exit(if successful { 0 } else { 125 }) }
}

enum ServiceExit {
    Controller(ControllerTerminal),
    NeverAuthorized(super::NeverAuthorized),
}

// One iteration of the actual process owner, also exercised without calling
// _exit by the socketpair/state controls below. It never releases custody.
fn advance_service(
    service: &mut AcceptedProviderService,
    live_before: &mut Option<u64>,
) -> Option<ServiceExit> {
    let sampled = monotonic_ns();
    match service.controller_has_exited() {
        Ok(true) => return Some(ServiceExit::Controller(ControllerTerminal)),
        Ok(false) => *live_before = sampled.or(*live_before),
        Err(error) => {
            service.failure.get_or_insert_with(|| error.to_string());
        }
    }
    if service.failure.is_none() && !service.run_peer_ended() {
        match catch_unwind(AssertUnwindSafe(|| service.step())) {
            Ok(Ok(())) => {}
            Ok(Err(error)) => {
                service.failure.get_or_insert_with(|| error.to_string());
            }
            Err(_) => {
                service.failure.get_or_insert_with(|| {
                    "accepted service callback panicked; effect remains unknown".into()
                });
            }
        }
    }
    // A received malformed packet or failed alias duplication is a failed
    // startup, not an empty/terminal/successful session. All retained SCM
    // remains owned by the dedicated process until PF_EXITING.
    service
        .take_pre_authorization_refusal()
        .map(ServiceExit::NeverAuthorized)
}

// State snapshot used by assertions, not an emitted terminal/durability receipt.
#[cfg(test)]
fn pre_authorization_report(
    service: &AcceptedProviderService,
    never: &super::NeverAuthorized,
) -> Value {
    serde_json::json!({
        "schema": "hermit-accepted-provider-startup-failure-v1",
        "phase": "never_authorized",
        "run": service.incarnation,
        "startup_cutoff_ns": never.startup_cutoff_ns.get(),
        "failure": service.failure,
        "bootstrap": service.bootstrap.terminal_custody(),
        "controller_terminal": false,
        "provider_authorized": false,
        "service_status": 125,
        "socket_release": "pending_process_exit",
    })
}

fn exit_before_authorization(
    service: &mut AcceptedProviderService,
    _never: super::NeverAuthorized,
) -> ! {
    // The existing private endpoint attempts at most one MSG_DONTWAIT send.
    // Missing/malformed request, EOF, full queue or send error may lose this
    // diagnostic. Never retry, write/flush stdout or fsync before this exit.
    // Status125, not delivery/durability, is the failure outcome. The first
    // error and all candidate aliases remain owned until PF_EXITING.
    service.notify_bootstrap_failure();
    // No close_for_process_exit, ControllerTerminal, successful inventory or
    // grant is constructed. The service is already ManuallyDrop; even queued
    // and quarantined rights are released only by this dedicated process exit.
    unsafe { libc::_exit(125) }
}

fn run_service(service: &mut AcceptedProviderService) -> ! {
    let mut failure_reported = false;
    let mut live_before = None;
    loop {
        match advance_service(service, &mut live_before) {
            Some(ServiceExit::Controller(proof)) => {
                exit_after_controller(service, proof, live_before)
            }
            Some(ServiceExit::NeverAuthorized(proof)) => exit_before_authorization(service, proof),
            None => {}
        }
        let maintenance = service.maintenance_interval();
        if service.failure.is_some() {
            // The parent is waiting for this exact bootstrap response before
            // it can take its existing owned Container cancellation path.
            // Keep all provider/SCM custody until the original controller exits.
            service.notify_bootstrap_failure();
            if !failure_reported {
                let run_send_ended = service.end_run_send_direction();
                let _ = report(&serde_json::json!({
                    "schema": "hermit-accepted-provider-terminal-v1",
                    "phase": "retained_failure",
                    "run": service.incarnation,
                    "failure": service.failure,
                    "bootstrap_failure_sent": service.bootstrap_failure_sent,
                    "bootstrap_failure_send_error": service.bootstrap_failure_send_error,
                    "run_send_direction_ended": run_send_ended.as_ref().map(|r| r.is_ok()),
                    "run_send_direction_error": run_send_ended.and_then(Result::err),
                    "controller_terminal": false,
                    "requires_external_recovery": true,
                }));
                failure_reported = true;
            }
            // A missing/unreadable controller capability is not a terminal
            // proof. The bounded launch owner still owns unit recovery. Avoid
            // repeatedly polling a dead endpoint or rerunning an unknown call.
            std::thread::sleep(Duration::from_millis(10));
        } else if service.run_peer_ended() {
            // Nothing remains to receive; wait only for the pidfd proof.
            std::thread::sleep(maintenance);
        } else if let Err(error) = service.wait_transport(Instant::now() + maintenance) {
            service.failure.get_or_insert_with(|| error.to_string());
        }
    }
}

/// Run the accepted provider in its dedicated outside-container process.
///
/// The function never returns and uses `_exit` after terminal evidence, so every
/// retained socket right receives Linux process-exit release rather than Rust
/// stack-drop close. The launcher must retain and reap the exact unit/process,
/// read the terminal report, and independently prove every reported BPF ID absent.
/// A failed or incomplete service never emits a successful terminal status.
///
/// # Safety
/// This must be an early entry of a dedicated, single-threaded helper process,
/// before ordinary CLI/runtime initialization. `stdin` is the authenticated
/// private seqpacket endpoint. `library` and `object` name sealed owned memfds
/// whose owners remain alive on the caller's stack; bytes, BTF and trusted loader
/// environment/dependencies satisfy `Provider::open`'s contract. No guest has
/// started before the private bootstrap succeeds. An outside owner supplies
/// finite process/unit bounds and retains recovery resources on every failure.
pub unsafe fn run_accepted_provider_process(
    stdin: OwnedFd,
    incarnation: [u8; 16],
    library: CString,
    object: CString,
    library_file: OwnedFd,
    startup_cutoff_ns: NonZeroU64,
) -> ! {
    // Retain the exact sealed artifact before even endpoint validation. An
    // early refusal releases it only through this dedicated process exit.
    let mut library_file = ManuallyDrop::new(Some(library_file));
    let service = unsafe {
        AcceptedProviderService::from_private_stdin(
            stdin,
            incarnation,
            library,
            object,
            startup_cutoff_ns,
        )
    };
    match service {
        Ok(mut service) => {
            service.grouped_library = library_file.take();
            let mut service = ManuallyDrop::new(service);
            // This dedicated early entry owns the endpoint and sealed library
            // before process policy setup. No bootstrap/control receipt or
            // provider work may precede the strict grouped-holder protection.
            let protected = (|| -> io::Result<()> {
                if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                if unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) } != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            })();
            if let Err(error) = protected {
                service.failure.get_or_insert_with(|| error.to_string());
                // The retained service has received no message and created no
                // BPF owner. There is no request to notify. Do not let stdout
                // backpressure delay process-exit release of original custody.
                unsafe { libc::_exit(125) }
            }
            run_service(&mut service)
        }
        Err((_error, endpoint)) => {
            // No message or BPF operation occurred. Retain even this original
            // endpoint through process exit; do not claim a controller drain.
            let _endpoint = ManuallyDrop::new(endpoint);
            // An invalid endpoint cannot carry a trusted diagnostic. Status125
            // survives; no blocking stdout fallback is permitted before exit.
            unsafe { libc::_exit(125) }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsRawFd;
    use std::os::fd::FromRawFd;

    use super::*;

    fn unopened(cutoff: NonZeroU64) -> (OwnedFd, AcceptedProviderService) {
        let (peer, service, _reply_alias) = unopened_with_reply_alias(cutoff);
        (peer, service)
    }

    fn unopened_with_reply_alias(
        cutoff: NonZeroU64,
    ) -> (OwnedFd, AcceptedProviderService, OwnedFd) {
        let mut pair = [-1; 2];
        assert_eq!(
            unsafe {
                libc::socketpair(
                    libc::AF_UNIX,
                    libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC,
                    0,
                    pair.as_mut_ptr(),
                )
            },
            0
        );
        let peer = unsafe { OwnedFd::from_raw_fd(pair[0]) };
        let input = unsafe { OwnedFd::from_raw_fd(pair[1]) };
        let reply_alias = super::super::duplicate(&input).unwrap();
        // No provider operation is reachable in these pre-authorization cases.
        let service = unsafe {
            AcceptedProviderService::from_private_stdin(
                input,
                [7; 16],
                CString::new("/never-opened/library").unwrap(),
                CString::new("/never-opened/object").unwrap(),
                cutoff,
            )
        }
        .map_err(|(error, _)| error)
        .unwrap();
        (peer, service, reply_alias)
    }

    fn future_cutoff() -> NonZeroU64 {
        NonZeroU64::new(monotonic_ns().unwrap().checked_add(1_000_000_000).unwrap()).unwrap()
    }

    fn regular_right() -> OwnedFd {
        let raw = unsafe { libc::memfd_create(c"prebootstrap-state".as_ptr(), libc::MFD_CLOEXEC) };
        assert!(raw >= 0);
        unsafe { OwnedFd::from_raw_fd(raw) }
    }

    fn original_self_pidfd() -> OwnedFd {
        let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) };
        assert!(raw >= 0);
        let pin = unsafe { OwnedFd::from_raw_fd(raw as i32) };
        super::super::super::accepted_provider::PidfdIdentity::read(&pin).unwrap();
        pin
    }

    fn send_bootstrap(
        peer: OwnedFd,
        controller: OwnedFd,
        run: OwnedFd,
    ) -> super::super::AcceptedSession {
        use super::super::super::accepted_parent::ProviderArtifact;
        use super::super::super::accepted_transport::AcceptedSession;
        use super::super::super::accepted_transport::Envelope;
        use super::super::super::accepted_transport::Operation;
        let expected = ProviderArtifact {
            topology: super::super::super::ProviderTopology::ClassicV40,
            wire_format: super::super::super::ProviderWireFormat::Abi7Copy4,
            object_sha256: [1; 32],
            library_sha256: [2; 32],
            btf_sha256: [4; 32],
            maps: 17,
            programs: 25,
            links: 25,
        };
        let mut parent = AcceptedSession::new(peer, [7; 16]).unwrap();
        let sequence = parent
            .prepare(
                Envelope {
                    run: [7; 16],
                    sequence: 0,
                    owner: None,
                    accept: None,
                    operation: Operation::Bootstrap,
                    body: serde_json::to_vec(&expected).unwrap(),
                },
                vec![controller, run],
            )
            .unwrap();
        assert_eq!(sequence, 1);
        assert!(parent.try_send(sequence).unwrap());
        parent
    }

    fn require_wrong_rights_refusal(controller: OwnedFd) {
        let (peer, mut service) = unopened(future_cutoff());
        let _parent = send_bootstrap(peer, controller, regular_right());
        let decision = advance_service(&mut service, &mut None);
        assert!(
            matches!(decision, Some(ServiceExit::NeverAuthorized(_))),
            "valid framed wrong-type SCM must terminate before provider permission"
        );
        let first = service.failure.clone().unwrap();
        assert!(
            service.controller.is_some(),
            "candidate controller must remain owned"
        );
        assert_eq!(service.bootstrap.terminal_custody().retained_rights, 2);
        assert_eq!(service.bootstrap.terminal_custody().incoming_unfinished, 1);
        assert!(!service.bootstrap_sent);
        assert_eq!(service.provider.terminal_state()["close_started"], false);
        assert!(
            !service.controller_has_exited().unwrap(),
            "candidate must not dispatch terminal"
        );
        assert!(advance_service(&mut service, &mut None).is_none());
        assert_eq!(service.failure.as_ref(), Some(&first));
    }

    #[test]
    fn prebootstrap_valid_scm_empty_pipe_and_regular_file_refused() {
        let mut pair = [-1; 2];
        assert_eq!(
            unsafe { libc::pipe2(pair.as_mut_ptr(), libc::O_CLOEXEC) },
            0
        );
        let read = unsafe { OwnedFd::from_raw_fd(pair[0]) };
        let _writer = unsafe { OwnedFd::from_raw_fd(pair[1]) };
        require_wrong_rights_refusal(read);
    }

    #[test]
    fn prebootstrap_valid_scm_readable_file_is_not_terminal() {
        require_wrong_rights_refusal(regular_right());
    }

    #[test]
    fn prebootstrap_authenticated_pidfd_invalid_run_retains_refusal() {
        require_wrong_rights_refusal(original_self_pidfd());
    }

    #[test]
    fn prebootstrap_rightless_eof_terminates_original_service_iteration() {
        let (peer, mut service) = unopened(future_cutoff());
        assert_eq!(
            unsafe { libc::shutdown(peer.as_raw_fd(), libc::SHUT_WR) },
            0
        );
        let decision = advance_service(&mut service, &mut None);
        assert!(
            matches!(decision, Some(ServiceExit::NeverAuthorized(_))),
            "original service iteration must terminate unopened EOF, not retain forever"
        );
        let Some(ServiceExit::NeverAuthorized(proof)) = decision else {
            unreachable!()
        };
        let failure = pre_authorization_report(&service, &proof);
        assert_eq!(failure["service_status"], 125);
        assert_eq!(failure["controller_terminal"], false);
        assert_eq!(failure["provider_authorized"], false);
        assert_eq!(
            failure["schema"],
            "hermit-accepted-provider-startup-failure-v1"
        );
        assert!(failure.get("close_receipts").is_none());
        assert!(failure.get("grant").is_none());
        assert!(service.failure.as_ref().unwrap().contains("end-of-stream"));
        assert!(service.controller.is_none());
        assert!(!service.bootstrap_sent);
        assert_eq!(service.bootstrap.terminal_custody().retained_rights, 0);
        assert_eq!(service.provider.terminal_state()["close_started"], false);
    }

    #[test]
    fn prebootstrap_silent_peer_uses_original_cutoff() {
        let cutoff = NonZeroU64::new(monotonic_ns().unwrap() + 20_000_000).unwrap();
        let (_peer, mut service) = unopened(cutoff);
        assert!(advance_service(&mut service, &mut None).is_none());
        std::thread::sleep(Duration::from_millis(25));
        let decision = advance_service(&mut service, &mut None);
        let Some(ServiceExit::NeverAuthorized(proof)) = decision else {
            panic!("original service iteration must terminate at the original silent-peer cutoff");
        };
        assert_eq!(proof.startup_cutoff_ns, cutoff);
        assert_eq!(
            service.failure.as_deref(),
            Some("original accepted startup cutoff elapsed before authorization")
        );
        assert!(service.controller.is_none());
        assert_eq!(service.provider.terminal_state()["close_started"], false);
    }

    #[test]
    fn prebootstrap_malformed_scm_is_retained_failure_not_empty_success() {
        let (peer, mut service) = unopened(future_cutoff());
        let (right, _right_peer) = unopened(future_cutoff());
        let mut byte = b'x';
        let mut iov = libc::iovec {
            iov_base: (&mut byte as *mut u8).cast(),
            iov_len: 1,
        };
        let mut control = [0usize; 8];
        let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
        msg.msg_iov = &mut iov;
        msg.msg_iovlen = 1;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen =
            unsafe { libc::CMSG_SPACE(std::mem::size_of::<i32>() as u32) } as usize;
        unsafe {
            let header = libc::CMSG_FIRSTHDR(&msg);
            (*header).cmsg_level = libc::SOL_SOCKET;
            (*header).cmsg_type = libc::SCM_RIGHTS;
            (*header).cmsg_len = libc::CMSG_LEN(std::mem::size_of::<i32>() as u32) as usize;
            libc::CMSG_DATA(header)
                .cast::<i32>()
                .write(right.as_raw_fd());
            assert_eq!(libc::sendmsg(peer.as_raw_fd(), &msg, libc::MSG_NOSIGNAL), 1);
        }
        assert!(matches!(
            advance_service(&mut service, &mut None),
            Some(ServiceExit::NeverAuthorized(_))
        ));
        let custody = service.bootstrap.terminal_custody();
        assert_eq!(custody.quarantined_messages, 1);
        assert_eq!(custody.quarantined_rights, 1);
        assert!(
            service
                .failure
                .as_ref()
                .unwrap()
                .contains("retained with its received rights")
        );
        assert!(!service.bootstrap_sent);
        assert!(service.controller.is_none());
        assert_eq!(service.provider.terminal_state()["close_started"], false);
    }

    #[test]
    fn prebootstrap_failed_controller_duplicate_preserves_inbox_and_first_error() {
        use super::super::super::accepted_transport::AcceptedSession;
        use super::super::super::accepted_transport::Envelope;
        use super::super::super::accepted_transport::Operation;
        use super::super::super::accepted_transport::Received;
        let (peer, mut service) = unopened(future_cutoff());
        let mut parent = AcceptedSession::new(peer, [7; 16]).unwrap();
        let (a, _a_peer) = unopened(future_cutoff());
        let (b, _b_peer) = unopened(future_cutoff());
        let request = parent
            .prepare(
                Envelope {
                    run: [7; 16],
                    sequence: 0,
                    owner: None,
                    accept: None,
                    operation: Operation::Bootstrap,
                    body: vec![],
                },
                vec![a, b],
            )
            .unwrap();
        assert!(parent.try_send(request).unwrap());
        assert_eq!(
            service.bootstrap.try_receive().unwrap(),
            Some(Received::Request(request))
        );
        let (_, rights, _) = service.bootstrap.retained_request(request).unwrap();
        // The real binding transition receives an explicit syscall failure;
        // no process-wide FD limit or fabricated successful controller is used.
        let error = super::super::bind_controller(
            &mut service.controller,
            &mut service.never_authorized,
            &rights[0],
            |_| Err(io::Error::from_raw_os_error(libc::EMFILE)),
        )
        .unwrap_err();
        let first = error.to_string();
        service.failure = Some(first.clone());
        assert!(matches!(
            advance_service(&mut service, &mut None),
            Some(ServiceExit::NeverAuthorized(_))
        ));
        let custody = service.bootstrap.terminal_custody();
        assert_eq!(custody.incoming, 1);
        assert_eq!(custody.retained_rights, 2);
        assert_eq!(custody.incoming_unfinished, 1);
        assert_eq!(service.failure.as_ref(), Some(&first));
        assert!(service.controller.is_none());
        assert!(!service.bootstrap_sent);
    }

    #[test]
    fn prebootstrap_binding_permanently_spends_startup_exit_authority() {
        let cutoff = future_cutoff();
        let (_peer, mut service) = unopened(cutoff);
        let original = original_self_pidfd();
        super::super::bind_controller(
            &mut service.controller,
            &mut service.never_authorized,
            &original,
            super::super::duplicate,
        )
        .unwrap();
        assert!(
            service.never_authorized.is_some(),
            "authenticated alias alone is not permission"
        );
        let (run, _run_peer) = unopened(future_cutoff());
        super::super::import_run(
            &mut service.run_candidate,
            &mut service.run,
            &run,
            service.incarnation,
            super::super::super::ProviderWireFormat::Abi7Copy4,
        )
        .unwrap();
        super::super::authorize_provider(
            &mut service.never_authorized,
            &mut service.authorized,
            &service.controller,
            &service.run,
        )
        .unwrap();
        assert!(service.authorized.is_some());
        assert!(!service.controller_has_exited().unwrap());
        let remaining = cutoff.get().saturating_sub(monotonic_ns().unwrap());
        std::thread::sleep(Duration::from_nanos(remaining + 1));
        service.check_startup_cutoff();
        assert!(
            service.failure.is_none(),
            "startup cutoff must not become a guest-lifetime limit"
        );
        // No native attempt is executed by this control. Model its failure at
        // the actual already-spent production boundary; it cannot restore exit.
        service.failure = Some("failed native attempt after permission".into());
        assert!(service.take_pre_authorization_refusal().is_none());
        assert!(advance_service(&mut service, &mut None).is_none());
        assert!(service.controller.is_some());
        // Even loss of a field cannot recreate spent authority from emptiness.
        let retained = service.controller.take();
        assert!(service.take_pre_authorization_refusal().is_none());
        assert!(retained.is_some());
    }

    #[test]
    fn prebootstrap_unknown_partial_binding_never_uses_new_exit() {
        let (peer, mut service) = unopened(NonZeroU64::new(1).unwrap());
        service.controller = Some(super::super::duplicate(&peer).unwrap());
        // Candidate aliases alone are safe preauthorization custody. An actual
        // contradictory native-install marker must still refuse early exit.
        service.grouped_installed = true;
        service.failure = Some("original unknown binding".into());
        service.check_startup_cutoff();
        assert!(service.take_pre_authorization_refusal().is_none());
        assert_eq!(service.failure.as_deref(), Some("original unknown binding"));
        assert!(service.never_authorized.is_some());
        assert!(service.controller.is_some());
        assert!(!service.controller_has_exited().unwrap());
    }

    #[test]
    fn prebootstrap_original_cutoff_after_partial_acquisition_is_final() {
        let cutoff = NonZeroU64::new(monotonic_ns().unwrap() + 20_000_000).unwrap();
        let (_peer, mut service) = unopened(cutoff);
        let pin = original_self_pidfd();
        super::super::bind_controller(
            &mut service.controller,
            &mut service.never_authorized,
            &pin,
            super::super::duplicate,
        )
        .unwrap();
        let (run, _run_peer) = unopened(future_cutoff());
        super::super::import_run(
            &mut service.run_candidate,
            &mut service.run,
            &run,
            service.incarnation,
            super::super::super::ProviderWireFormat::Abi7Copy4,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(25));
        let error = super::super::authorize_provider(
            &mut service.never_authorized,
            &mut service.authorized,
            &service.controller,
            &service.run,
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        let first = error.to_string();
        service.failure = Some(first.clone());
        let Some(ServiceExit::NeverAuthorized(proof)) = advance_service(&mut service, &mut None)
        else {
            panic!("partial aliases must not spend original-cutoff refusal");
        };
        assert_eq!(proof.startup_cutoff_ns, cutoff);
        assert!(service.controller.is_some() && service.run.is_some());
        assert!(service.authorized.is_none());
        assert_eq!(service.failure.as_ref(), Some(&first));
    }

    #[test]
    fn prebootstrap_invalid_run_alias_and_failure_reply_remain_owned() {
        let (peer, mut service) = unopened(future_cutoff());
        let mut parent = send_bootstrap(peer, original_self_pidfd(), regular_right());
        assert!(matches!(
            advance_service(&mut service, &mut None),
            Some(ServiceExit::NeverAuthorized(_))
        ));
        assert!(
            service.run_candidate.is_some(),
            "failed run import must retain its alias"
        );
        let first = service.failure.clone();
        service.notify_bootstrap_failure();
        assert!(service.bootstrap_failure_sent);
        assert_eq!(service.failure, first);
        assert_eq!(service.bootstrap.terminal_custody().retained_rights, 2);
        assert_eq!(service.bootstrap.terminal_custody().incoming_unfinished, 1);
        parent.try_receive().unwrap().unwrap();
        let response: super::super::super::accepted_parent::BootstrapReply =
            serde_json::from_slice(parent.response(1).unwrap().unwrap()).unwrap();
        let super::super::super::accepted_parent::BootstrapReply::Failed(failure) = response else {
            panic!("preauthorization failure must never be READY");
        };
        assert_eq!(Some(failure.error), first);
        assert!(!service.bootstrap_sent);
        assert!(service.authorized.is_none());
    }

    #[test]
    fn prebootstrap_full_diagnostic_queue_does_not_delay_refusal() {
        let (peer, mut service, reply) = unopened_with_reply_alias(future_cutoff());
        let _parent = send_bootstrap(peer, original_self_pidfd(), regular_right());
        assert!(matches!(
            advance_service(&mut service, &mut None),
            Some(ServiceExit::NeverAuthorized(_))
        ));
        let first = service.failure.clone();
        let bytes = [0u8; 4096];
        let mut full = false;
        for _ in 0..4096 {
            let n = unsafe {
                libc::send(
                    reply.as_raw_fd(),
                    bytes.as_ptr().cast(),
                    bytes.len(),
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            };
            if n < 0 {
                assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
                full = true;
                break;
            }
            assert_eq!(n as usize, bytes.len());
        }
        assert!(full, "fixture must actually saturate the owned transport");
        let begin = Instant::now();
        service.notify_bootstrap_failure();
        assert!(begin.elapsed() < Duration::from_secs(1));
        assert!(
            !service.bootstrap_failure_sent,
            "a full queue is not delivered"
        );
        assert!(service.bootstrap_failure_send_error.is_none());
        assert_eq!(service.failure, first);
        assert_eq!(
            Some(&service.bootstrap_failure.as_ref().unwrap().error),
            first.as_ref()
        );
        assert!(service.controller.is_some() && service.run_candidate.is_some());
        assert_eq!(service.bootstrap.terminal_custody().retained_rights, 2);
        assert!(!service.bootstrap_sent);
    }

    #[test]
    fn grouped_cutoff_never_extends_any_later_controller_exit_observation() {
        let second = 1_000_000_000;
        let (live, start) = (50 * second, 50 * second + 7_000_000);
        let cutoff = grouped_cutoff(start, Some(live)).unwrap();
        assert_eq!(cutoff, live + second);
        // Every peer's first observation follows the live poll; the Keeper
        // requires cutoff <= first + 1s. A start-based cutoff fails that for
        // any observation before the provider's own later start.
        for first in [live + 1, live + 1_000_000, start - 1, start, start + 1] {
            assert!(cutoff <= first + second);
        }
        // Never later than the original start's second; no live proof keeps it.
        assert_eq!(grouped_cutoff(start, Some(start + 5)), Some(start + second));
        assert_eq!(grouped_cutoff(start, None), Some(start + second));
        assert_eq!(grouped_cutoff(start, Some(u64::MAX)), Some(start + second));
        assert_eq!(grouped_cutoff(u64::MAX, Some(live)), None);
    }

    #[test]
    fn accepted_provider_terminal_close_is_never_resubmitted() {
        let mut provider = super::super::super::accepted_provider::Provider::empty();
        assert!(provider.close_for_process_exit().unwrap().is_empty());
        assert!(provider.close_for_process_exit().is_err());
        assert_eq!(provider.terminal_state()["close_started"], true);
    }

    #[test]
    fn accepted_terminal_custody_never_equates_retained_rights_with_unresolved_effects() {
        let mut custody = SessionCustody {
            incoming: 2,
            outgoing: 1,
            incoming_unfinished: 0,
            outgoing_unacknowledged: 0,
            command_ack_unknown: 0,
            retained_rights: 5,
            quarantined_messages: 0,
            quarantined_rights: 0,
        };
        assert!(!unresolved(&custody));
        custody.incoming_unfinished = 1;
        assert!(unresolved(&custody));
        custody.incoming_unfinished = 0;
        custody.outgoing_unacknowledged = 1;
        assert!(unresolved(&custody));
        custody.outgoing_unacknowledged = 0;
        custody.command_ack_unknown = 1;
        assert!(unresolved(&custody));
        custody.command_ack_unknown = 0;
        custody.quarantined_messages = 1;
        assert!(unresolved(&custody));
    }

    #[test]
    fn accepted_terminal_close_report_preserves_partial_inventory_and_raw_error() {
        let receipt = ffi::CloseReceipt {
            incarnation: 7,
            inventory: ffi::Inventory {
                status: ffi::CallStatus {
                    operation: "ap_identifiers",
                    returned: -1,
                    errno: Some(libc::EIO),
                },
                ids: vec![ffi::ResourceId { kind: 2, id: 17 }],
                count_invalid: true,
            },
            close: ffi::CallStatus {
                operation: "ap_close",
                returned: -1,
                errno: Some(libc::EBUSY),
            },
            unexpected_drop: false,
            requires_external_absence: true,
        };
        let report = close_receipt(&receipt);
        assert_eq!(report["inventory"]["ids"][0]["id"], 17);
        assert_eq!(report["inventory"]["status"]["errno"], libc::EIO);
        assert_eq!(report["close"]["errno"], libc::EBUSY);
        assert_eq!(report["inventory"]["complete"], false);
        assert_eq!(report["requires_external_absence"], true);
    }
}
