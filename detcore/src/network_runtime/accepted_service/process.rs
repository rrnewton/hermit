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

fn monotonic_ns() -> Option<u64> {
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

fn run_service(service: &mut AcceptedProviderService) -> ! {
    let mut failure_reported = false;
    let mut live_before = None;
    loop {
        let sampled = monotonic_ns();
        match service.controller_has_exited() {
            Ok(true) => exit_after_controller(service, ControllerTerminal, live_before),
            Ok(false) => live_before = sampled.or(live_before),
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
) -> ! {
    // Retain the exact sealed artifact before even endpoint validation. An
    // early refusal releases it only through this dedicated process exit.
    let mut library_file = ManuallyDrop::new(Some(library_file));
    let service =
        unsafe { AcceptedProviderService::from_private_stdin(stdin, incarnation, library, object) };
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
                let _ = report(&serde_json::json!({
                    "schema": "hermit-accepted-provider-terminal-v1",
                    "phase": "process_protection_failure",
                    "run": incarnation,
                    "failure": service.failure,
                    "controller_terminal": false,
                    "service_status": 125,
                }));
                // The retained service has received no message and created no
                // BPF owner. Release all original custody only at process exit.
                unsafe { libc::_exit(125) }
            }
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
