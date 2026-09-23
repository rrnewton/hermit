//! The service is a dedicated process owner. Rust stack unwinding must never
//! become the final ordinary close of a retained guest TCP socket: Linux gives
//! process-exit release different linger and signal semantics.

use std::ffi::CString;
use std::io;
use std::io::Write;
use std::mem::ManuallyDrop;
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

fn exit_after_controller(service: &mut AcceptedProviderService, _: ControllerTerminal) -> ! {
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
        && service.run_replies.is_empty()
        && provider_state["active_setters"] == 0
        && provider_state["unresolved_matches"] == 0
        && inventories.iter().all(ffi::Inventory::complete);
    let before = serde_json::json!({
        "schema": "hermit-accepted-provider-terminal-v1",
        "phase": "before_close",
        "run": service.incarnation,
        "controller_terminal": true,
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
    let closed = service.provider.close_for_process_exit();
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
        "run": service.incarnation,
        "controller_terminal": true,
        "close_receipts": closed.as_ref().ok().map(|receipts| receipts.iter().map(close_receipt).collect::<Vec<_>>()),
        "close_error": closed.as_ref().err().map(ToString::to_string),
        "service_status": if successful { 0 } else { 125 },
        "requires_external_absence": true,
        "socket_release": "pending_process_exit",
    });
    successful &= report(&final_report).is_ok();
    // The process owns all inbox/outbox/quarantine capabilities until here.
    // Do not drop the service, its library, or any socket-right container.
    unsafe { libc::_exit(if successful { 0 } else { 125 }) }
}

fn run_service(service: &mut AcceptedProviderService) -> ! {
    let mut failure_reported = false;
    loop {
        match service.controller_has_exited() {
            Ok(true) => exit_after_controller(service, ControllerTerminal),
            Ok(false) => {}
            Err(error) => {
                service.failure.get_or_insert_with(|| error.to_string());
            }
        }
        if service.failure.is_none() {
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
        if service.failure.is_some() {
            if !failure_reported {
                let _ = report(&serde_json::json!({
                    "schema": "hermit-accepted-provider-terminal-v1",
                    "phase": "retained_failure",
                    "run": service.incarnation,
                    "failure": service.failure,
                    "controller_terminal": false,
                    "requires_external_recovery": true,
                }));
                failure_reported = true;
            }
            // A missing/unreadable controller capability is not a terminal
            // proof. The bounded launch owner still owns unit recovery. Avoid
            // repeatedly polling a dead endpoint or rerunning an unknown call.
            std::thread::sleep(Duration::from_millis(10));
        } else if let Err(error) =
            service.wait_transport(Instant::now() + super::OBSERVATION_MAINTENANCE)
        {
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
) -> ! {
    let service =
        unsafe { AcceptedProviderService::from_private_stdin(stdin, incarnation, library, object) };
    match service {
        Ok(service) => {
            let mut service = ManuallyDrop::new(service);
            run_service(&mut service)
        }
        Err((error, endpoint)) => {
            // No message or BPF operation occurred. Retain even this original
            // endpoint through process exit; do not claim a controller drain.
            let _endpoint = ManuallyDrop::new(endpoint);
            let _ = report(&serde_json::json!({
                "schema": "hermit-accepted-provider-terminal-v1",
                "phase": "invalid_private_endpoint",
                "run": incarnation,
                "failure": error.to_string(),
                "controller_terminal": false,
                "service_status": 125,
            }));
            unsafe { libc::_exit(125) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
