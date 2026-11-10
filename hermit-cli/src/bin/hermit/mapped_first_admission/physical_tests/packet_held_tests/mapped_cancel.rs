//! Genuine map/cancel controls plus an explicit bounded initializer experiment.
// H-private root component wired below; receipts record compilation and tests.
// Uses existing retained-owner APIs. It grants no production source authority.
use std::io;
// H-private map/cancel data only. No execution/source/admission capability.
// It authorizes mapping only; execution qualification is separate.
// Control body, placed as a child of packet_held_tests so it reuses
// that module's genuine admit/Admission/exact_storage/Config/watch helpers.
// Requires the reviewed custody bridge and paired reverie-loader dev dependency.
// Ordinary controls stop after mapping; Initialize separately observes real return.
use std::panic::AssertUnwindSafe;
use std::panic::catch_unwind;
use std::panic::resume_unwind;
use std::sync::atomic::Ordering;

use reverie_inguest::launcher::exec_stop::CleanupReceipt;
use reverie_inguest::launcher::exec_stop::ExecStop;
use reverie_inguest::launcher::exec_stop::PreparedExec;
use reverie_inguest::launcher::exec_stop::physical::BootstrapEntryCoordinates;
use reverie_inguest::launcher::exec_stop::producer::Armed;
use reverie_inguest::launcher::exec_stop::producer::ArmingFacts;
use reverie_inguest::launcher::exec_stop::producer::AuditedRuntimeImage;
use reverie_inguest::launcher::exec_stop::producer::BirthKind;
use sha2::Digest;
use sha2::Sha256;

use super::*;
use crate::app::mapped_first_admission::runtime_support::WAIT;
use crate::app::mapped_first_admission::runtime_support::need;
use crate::app::mapped_first_admission::runtime_support::rpc;
use crate::app::mapped_first_admission::runtime_support::*;

#[derive(Clone, Copy, PartialEq, Eq)]
enum MappedCase {
    Positive,
    OtherBirth,
    Poison,
    Initialize,
}

fn same_error(error: io::Error, expected: &str) -> io::Result<()> {
    need(
        error.kind() == io::ErrorKind::InvalidData && error.to_string() == expected,
        "negative reached a different exact lease predicate",
    )
}
fn launch_into<'fd>(
    root: &mut RetainedPhysical<'fd>,
    a: &'fd MappedBirth<'_>,
    b: &'fd MappedBirth<'_>,
    detlog: &'fd UnixStream,
    own: &'fd MappedBirth<'_>,
) -> io::Result<()> {
    let [_, runtime, handoff] = own.inherited();
    let request = PreparedExec::new(
        std::path::Path::new("/bin/true"),
        &["mapped-genuine-packet-cancel-only".into()],
        &[("ORIGINAL".into(), "mapped-packet".into())],
        None,
        // BOTH packet OFDs really cross BOTH births. No membership failure can
        // stand in for the intended negative watched-birth comparison.
        vec![
            a.leases().packet(),
            b.leases().packet(),
            detlog.as_fd(),
            runtime,
            handoff,
        ],
    )
    .map_err(rpc)?;
    root.retain_spawn(ExecStop::spawn(request, root.receipt.clone()));
    root.initial().map(|_| ())
}
/// Safety: called only by an exclusively bounded physical-control runner on the
/// original OS thread, with compatible stable CPUID/XCR0 domain for XState::pkru,
/// genuine sole-leader/unshared-FD initial stop and known seccomp/signal lineage.
/// The existing prepare_mapping contract remains unchanged. Ordinary cases only
/// map. Initialize also requires the reviewed exact-image valid-path, signal and
/// concurrent-RPC contract at its unsafe observer call; never Guest release.
unsafe fn mapped_packet_control(
    case: MappedCase,
    audited: &AuditedRuntimeImage,
    initializer: BootstrapEntryCoordinates,
) -> io::Result<()> {
    // All fallible pre-birth setup may use '?'. After birth every outcome reaches
    // same-owner cancellation before watch teardown or host assertion unwinds.
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let directory = tempfile::tempdir()?;
    let directory_fd = File::open(directory.path())?;
    let socket = |name: &str| {
        // Real initialization uses the existing rpc.sock/stats.sock companion
        // contract. Map/cancel-only controls keep their distinct a/b endpoints.
        let name = if case == MappedCase::Initialize && name == "a" {
            "rpc"
        } else {
            name
        };
        std::path::PathBuf::from(format!(
            "/proc/{}/fd/{}/{name}.sock",
            std::process::id(),
            directory_fd.as_raw_fd()
        ))
    };
    let config = detcore::Config {
        no_rcb_time: true,
        max_timeslice: None,
        target_timeslice: None,
        sequentialize_threads: false,
        in_guest_detlog_forward_policy: Some(detcore::detlog::ForwardPolicy::all().encode()),
        ..Default::default()
    };
    need(
        packet::config_violation(&config).is_none(),
        "Config outside ordinary domain",
    )?;
    let global = Arc::new(runtime.block_on(
        <detcore::GlobalState as GlobalTool>::init_global_state(&config),
    ));
    let reserved_signals = reserved_signals(&config);
    // Each birth's packet, runtime and handoff descriptors.
    let birth_a = MappedBirth::new(audited, MEMFD_NAMES).map_err(rpc)?;
    let birth_b = MappedBirth::new(audited, MEMFD_NAMES).map_err(rpc)?;
    let (sender_a, receiver_a) = if case == MappedCase::Initialize {
        let (sender, receiver) = crate::app::run::RunOpts::open_forwarded_detlog_channel()
            .map_err(rpc)?
            .ok_or_else(|| rpc("actual DETLOG channel absent"))?;
        (UnixStream::from(sender), UnixStream::from(receiver))
    } else {
        UnixStream::pair()?
    };
    let mut trial = if case == MappedCase::Initialize {
        Some(init_trial::Trial::register(
            receiver_a.as_fd().try_clone_to_owned()?,
        ))
    } else {
        None
    };
    let (sender_b, receiver_b) = UnixStream::pair()?;
    receiver_a.set_nonblocking(true)?;
    receiver_b.set_nonblocking(true)?;
    // Tokio's sockets are registered under the same current-thread runtime.
    let (mut receiver_a, mut receiver_b) = {
        let _entered = runtime.enter();
        (
            tokio::net::UnixStream::from_std(receiver_a)?,
            tokio::net::UnixStream::from_std(receiver_b)?,
        )
    };
    let mut admissions = Vec::new();
    // Stable optional wire storage is declared before the borrowing physical
    // holders; explicit cleanup drops physical holders first, then this storage.
    let mut bundle = None;
    let mut a = RetainedPhysical::empty(CleanupReceipt::default());
    let mut b = RetainedPhysical::empty(CleanupReceipt::default());
    let mut panic_payload = None;
    let setup = catch_unwind(AssertUnwindSafe(|| -> io::Result<_> {
        launch_into(&mut a, &birth_a, &birth_b, &sender_a, &birth_a)?;
        if matches!(case, MappedCase::OtherBirth | MappedCase::Poison) {
            launch_into(&mut b, &birth_a, &birth_b, &sender_b, &birth_b)?;
        }
        let stop = a.initial()?;
        let captured = stop.capture_initial_inputs().map_err(rpc)?;
        let admission_a = runtime.block_on(admit(
            &mut admissions,
            &global,
            &config,
            &socket("a"),
            stop,
            &sender_a,
        ))?;
        let admission_b = if matches!(case, MappedCase::OtherBirth | MappedCase::Poison) {
            let actual = runtime.block_on(admit(
                &mut admissions,
                &global,
                &config,
                &socket("b"),
                b.initial()?,
                &sender_b,
            ))?;
            need(
                admission_a.watched_process().birth_identity()
                    != actual.watched_process().birth_identity(),
                "controls did not create distinct real births",
            )?;
            Some(actual)
        } else {
            None
        };
        let sealed_a = birth_a
            .fill_packet(stop, &*admission_a)
            .map_err(producer_error)?;
        let sealed_b = match admission_b.as_ref() {
            Some(admission) if case == MappedCase::OtherBirth => Some(
                birth_b
                    .fill_packet(b.initial()?, &**admission)
                    .map_err(producer_error)?,
            ),
            _ => None,
        };
        exact_storage(
            birth_a.leases().packet(),
            &sealed_a,
            &admission_a.initial_packet()?,
        )?;
        birth_a
            .leases()
            .verify_at(stop, &*admission_a)
            .map_err(producer_error)?;
        let copied = stop
            .prepare_copied_inputs(
                &captured,
                &birth_a.validated_image().map_err(rpc)?,
                &birth_a.copied_input_request().map_err(rpc)?,
            )
            .map_err(rpc)?;
        // SAFETY: actual unchanged stopped XState; runner's compatible CPU/XCR0
        // contract is explicit above. No host RDPKRU, guessed offset or zero.
        let pkru = unsafe { stop.entry().xstate().pkru() }.map_err(rpc)?;
        let Armed {
            preparation,
            entry: _,
            claim: _,
        } = birth_a
            .arm(&ArmingFacts {
                birth: BirthKind::Initial,
                captured: &captured,
                copied: &copied,
                mask: stop.entry().signal_mask(),
                pkru,
                reserved_signals,
            })
            .map_err(rpc)?;
        Ok((
            Some(preparation),
            sealed_a,
            sealed_b,
            admission_a,
            admission_b,
        ))
    }));
    let mut primary = match setup {
        Ok(Ok(value)) => {
            bundle = Some(value);
            Ok(())
        }
        Ok(Err(error)) => Err(error),
        Err(payload) => {
            panic_payload = Some(payload);
            Err(rpc("setup panicked; retained root cleanup follows"))
        }
    };
    if let Some((preparation, sealed_a, sealed_b, admission_a, admission_b)) = bundle.as_mut() {
        let observed = catch_unwind(AssertUnwindSafe(|| -> io::Result<()> {
            let inputs = preparation
                .take()
                .ok_or_else(|| rpc("armed preparation already consumed"))?;
            // SAFETY: root retains exactly the outer leases named above; only
            // existing synchronous mapping syscall/INT3 at this step.
            unsafe { a.prepare(inputs) }?;
            birth_a
                .leases()
                .verify_at(a.mapped()?, &**admission_a)
                .map_err(producer_error)?;
            exact_storage(
                birth_a.leases().packet(),
                sealed_a,
                &admission_a.initial_packet()?,
            )?;
            need(
                !admission_a.failed.load(Ordering::Acquire)
                    && !admission_a.initial_used.load(Ordering::Acquire),
                "positive poisoned admission or consumed initial nonce",
            )?;
            match case {
                MappedCase::Positive => {}
                MappedCase::Initialize => {
                    let trial = trial
                        .as_mut()
                        .ok_or_else(|| rpc("initializer trial absent"))?;
                    runtime.block_on(async {
                        trial.start(
                            admissions
                                .first_mut()
                                .ok_or_else(|| rpc("actual admission absent"))?,
                            &directory_fd,
                        )?;
                        // SAFETY: the same retained image, exact entry/terminal
                        // coordinates and decoded PC list; independent valid-path
                        // pre-TLS source audit, original
                        // ptracer/current-thread runtime, retained real leases,
                        // concurrent genuine RPC and outer bounded cancellation.
                        // This experiment stops at the real initializer INT3 and
                        // never claims full qualification or releases a Guest.
                        unsafe {
                            trial
                                .observe(
                                    a.mapped
                                        .as_mut()
                                        .ok_or_else(|| rpc("actual mapping absent"))?,
                                    &config,
                                    initializer,
                                )
                                .await
                        }
                    })?;
                }
                MappedCase::OtherBirth => {
                    sealed_b
                        .as_ref()
                        .ok_or_else(|| rpc("actual sealed B absent"))?;
                    let admission_b = admission_b
                        .as_ref()
                        .ok_or_else(|| rpc("actual admission B absent"))?;
                    let error = birth_b
                        .leases()
                        .verify_at(a.mapped()?, &**admission_b)
                        .map_err(producer_error)
                        .err()
                        .ok_or_else(|| rpc("other actual birth accepted after mapping"))?;
                    same_error(
                        error,
                        "native peer/watch/source belongs to a different or failed birth",
                    )?;
                    need(
                        admission_b.failed.load(Ordering::Acquire)
                            && !admission_b.initial_used.load(Ordering::Acquire),
                        "wrong-birth admission not poisoned or nonce used",
                    )?;
                    birth_a
                        .leases()
                        .verify_at(a.mapped()?, &**admission_a)
                        .map_err(producer_error)?;
                    need(
                        !admission_a.failed.load(Ordering::Acquire),
                        "wrong-birth B poisoned A",
                    )?;
                }
                MappedCase::Poison => {
                    let admission_b = admission_b
                        .as_ref()
                        .ok_or_else(|| rpc("actual admission B absent"))?;
                    let error = match birth_a.fill_packet(b.initial()?, &**admission_b) {
                        Err(error) => producer_error(error),
                        Ok(_) => return Err(rpc("second fill accepted")),
                    };
                    same_error(error, "mapped packet lease fill was already attempted")?;
                    need(
                        admission_b.failed.load(Ordering::Acquire)
                            && !admission_a.failed.load(Ordering::Acquire),
                        "second-fill B did not poison exactly its actual admission",
                    )?;
                    let error = birth_a
                        .leases()
                        .verify_at(a.mapped()?, &**admission_a)
                        .map_err(producer_error)
                        .err()
                        .ok_or_else(|| rpc("old sealed A survived permanent poison"))?;
                    same_error(error, "mapped packet lease is permanently poisoned")?;
                    need(
                        admission_a.failed.load(Ordering::Acquire),
                        "old A verification failed without poisoning A",
                    )?;
                    // Raw immutable bytes remain equal; storage equality cannot
                    // substitute for the permanently failed lease predicate.
                    exact_storage(birth_a.leases().packet(), sealed_a, sealed_a)?;
                }
            }
            Ok(())
        }));
        primary = match observed {
            Ok(result) => result,
            Err(payload) => {
                panic_payload = Some(payload);
                Err(rpc("mapping/observation panicked; cleanup follows"))
            }
        };
    }
    // Neither assertions nor a Result error may unwind the owners before both
    // actual children have confirmed same-generation original-owner retirement.
    cleanup_all(&mut a, &mut b, &mut panic_payload);
    let transcript = a.cancelled.as_ref().map(|cancelled| {
        need(
            cancelled.cleanup.confirmed
                && cancelled.filter_bytes.len() == 23 * 8
                && cancelled
                    .injected_calls
                    .last()
                    .is_some_and(|(nr, args, result)| {
                        *nr == libc::SYS_seccomp as u64
                            && args[0] == libc::SECCOMP_SET_MODE_FILTER as u64
                            && args[1] == libc::SECCOMP_FILTER_FLAG_TSYNC
                            && *result == 0
                    }),
            "actual map/cancel transcript lost successful final filter",
        )
    });
    let mut cleanup_errors = Vec::new();
    match transcript {
        Some(Err(error)) => cleanup_errors.push(format!("mapping transcript: {error}")),
        None if primary.is_ok() => {
            cleanup_errors.push("no actual mapping cancellation transcript".to_owned())
        }
        _ => {}
    }
    // End every descriptor/backing borrow before dropping parent DETLOG senders.
    drop(a);
    drop(b);
    drop(bundle);
    drop(sender_a);
    drop(sender_b);
    runtime.block_on(async {
        if let Some(trial) = trial.as_mut() {
            match trial.close().await {
                Ok(Some(owner)) => match admissions
                    .iter_mut()
                    .find(|slot| Arc::ptr_eq(&slot.owner, &owner))
                {
                    Some(slot) => slot.drained = true,
                    None => cleanup_errors
                        .push("drained RPC has no matching retained admission".to_owned()),
                },
                Ok(None) => {}
                Err(error) => cleanup_errors.push(format!("actual initializer services: {error}")),
            }
        }
        for slot in &mut admissions {
            if primary.is_err() {
                slot.owner.fail(&"mapped packet observation failed");
            }
            let drain = if slot.drained {
                Ok(())
            } else {
                slot.owner.exit_admission().drain(WAIT).await
            };
            match drain {
                Ok(()) => {
                    slot.drained = true;
                    if slot.owner.exit_admission().reported_exits()
                        != [slot.owner.watched_process().process_id()]
                    {
                        cleanup_errors.push(
                            "actual watch did not report its own child exactly once".to_owned(),
                        );
                    }
                }
                Err(error) => cleanup_errors.push(format!("actual watch drain: {error:?}")),
            }
            if slot
                .issues
                .as_ref()
                .is_some_and(|issues| !issues.snapshot().is_empty())
            {
                cleanup_errors.push("actual RPC listener acquired an issue".to_owned());
            }
        }
        for receiver in [&mut receiver_a, &mut receiver_b] {
            let outcome = super::bounded(async {
                let mut byte = [0];
                need(
                    receiver.read(&mut byte).await? == 0,
                    "actual inherited DETLOG not EOF",
                )
            })
            .await;
            if let Err(error) = outcome {
                cleanup_errors.push(format!("DETLOG EOF: {error}"));
            }
        }
    });
    drop(admissions); // Directory and GlobalState still live here.
    // Input digest closure is separate from packet/birth/cleanup observations.
    if <[u8; 32]>::from(Sha256::digest(audited.bytes())) != audited.facts().image_sha256 {
        cleanup_errors.push("retained input image changed across control".to_owned());
    }
    if let Some(payload) = panic_payload {
        if !cleanup_errors.is_empty() {
            // Cleanup is confirmed now; diagnostic failure still must not replace
            // the original setup/observation/retirement panic being rethrown.
            if let Err(diagnostic) = catch_unwind(AssertUnwindSafe(|| {
                eprintln!("independent cleanup failures: {cleanup_errors:?}")
            })) {
                std::mem::forget(diagnostic);
            }
        }
        resume_unwind(payload);
    }
    if !cleanup_errors.is_empty() {
        return Err(rpc(format!(
            "cleanup={cleanup_errors:?}; primary={primary:?}"
        )));
    }
    // Map controls executed no image entry. Initialize may have observed the
    // genuine initializer return, but no case releases Guest or claims Ready.
    primary
}

// The map-only controls run on the same package fixture as Initialize: the
// producer's audit accepts only an image whose every syscall site is declared,
// which the package's facts are and the historical V11 coordinates are not.
#[test]
fn mapped_packet_prepared_same_birth() -> io::Result<()> {
    with_package_fixture(MappedCase::Positive)
}

#[test]
fn mapped_packet_prepared_other_birth_refuses() -> io::Result<()> {
    with_package_fixture(MappedCase::OtherBirth)
}

#[test]
fn mapped_packet_prepared_poison_refuses_old_lease() -> io::Result<()> {
    with_package_fixture(MappedCase::Poison)
}

#[cfg(test)]
mod retirement_unwind_tests {
    use std::cell::Cell;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;
    use std::sync::atomic::Ordering;

    use super::PanicPayload;
    use super::RetirementStep;
    use super::guarded_retirement;

    // Pure host-control model: these booleans are NOT a CleanupReceipt, child,
    // wait result or kernel confirmation. No test can enter the retention loop.
    struct Model {
        created: bool,
        confirmed: bool,
        capability: Option<()>,
    }
    impl Model {
        fn snapshot(&self) -> (bool, bool, bool) {
            (self.created, self.confirmed, self.capability.is_some())
        }
    }
    struct OuterLease(Rc<Cell<usize>>);
    impl Drop for OuterLease {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }

    #[test]
    fn consumed_capability_unwind_retains_outer_frame_without_confirmation() {
        let drops = Rc::new(Cell::new(0));
        let outer = OuterLease(drops.clone());
        let mut owner = Model {
            created: true,
            confirmed: false,
            capability: Some(()),
        };
        let mut panic = None;
        let step = guarded_retirement(
            &mut owner,
            &mut panic,
            |owner| {
                let _consumed = owner.capability.take();
                std::panic::panic_any("consumed before cancel returned");
            },
            Model::snapshot,
        );
        assert_eq!(step, RetirementStep::RetainFrame);
        assert!(
            panic
                .as_ref()
                .is_some_and(|payload| payload.downcast_ref::<&str>()
                    == Some(&"consumed before cancel returned"))
        );
        assert!(owner.capability.is_none());
        assert_eq!(drops.get(), 0);
        // The real root spins here. A data-only test drops its simulated lease
        // explicitly after checking the production decision; no reap is claimed.
        drop(outer);
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn borrowed_retry_unwind_preserves_only_retained_capability() {
        let mut owner = Model {
            created: true,
            confirmed: false,
            capability: Some(()),
        };
        let mut panic = None;
        assert_eq!(
            guarded_retirement(
                &mut owner,
                &mut panic,
                |_| {
                    std::panic::panic_any("borrowed retry panicked");
                },
                Model::snapshot
            ),
            RetirementStep::RetryRetained
        );
        assert!(owner.capability.is_some());
        assert!(panic.is_some());
    }

    #[test]
    fn confirmation_observed_after_unwind_is_not_lost() {
        let mut owner = Model {
            created: true,
            confirmed: false,
            capability: Some(()),
        };
        let mut panic = None;
        assert_eq!(
            guarded_retirement(
                &mut owner,
                &mut panic,
                |owner| {
                    let _ = owner.capability.take();
                    owner.confirmed = true;
                    std::panic::panic_any("panic after modeled confirmation");
                },
                Model::snapshot
            ),
            RetirementStep::Complete
        );
        assert!(panic.is_some());
    }

    #[test]
    fn snapshot_unwind_cannot_authorize_frame_release() {
        let mut owner = Model {
            created: true,
            confirmed: true,
            capability: Some(()),
        };
        let mut panic = None;
        assert_eq!(
            guarded_retirement(
                &mut owner,
                &mut panic,
                |_| {},
                |_| {
                    std::panic::panic_any("receipt snapshot unavailable");
                }
            ),
            RetirementStep::RetainFrame
        );
        assert!(panic.is_some());
    }

    #[test]
    fn original_panic_survives_secondary_payload_with_panicking_drop() {
        struct Secondary(Arc<AtomicUsize>);
        impl Drop for Secondary {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
                panic!("secondary payload Drop must not run during cleanup");
            }
        }
        let dropped = Arc::new(AtomicUsize::new(0));
        let mut owner = Model {
            created: true,
            confirmed: false,
            capability: Some(()),
        };
        let mut first: Option<PanicPayload> = Some(Box::new("original setup panic"));
        assert_eq!(
            guarded_retirement(
                &mut owner,
                &mut first,
                |_| {
                    std::panic::panic_any(Secondary(dropped.clone()));
                },
                Model::snapshot
            ),
            RetirementStep::RetryRetained
        );
        assert_eq!(
            first.as_ref().unwrap().downcast_ref::<&str>(),
            Some(&"original setup panic")
        );
        assert_eq!(dropped.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn no_created_child_completes_without_fabricating_confirmation() {
        let mut owner = Model {
            created: false,
            confirmed: false,
            capability: None,
        };
        let mut panic = None;
        assert_eq!(
            guarded_retirement(&mut owner, &mut panic, |_| {}, Model::snapshot),
            RetirementStep::Complete
        );
        assert!(!owner.confirmed);
        assert!(panic.is_none());
    }
}

#[test]
fn mapped_genuine_initializer_returns_before_guest() -> io::Result<()> {
    with_package_fixture(MappedCase::Initialize)
}

/// Run `case` on the package fixture's audited image and coordinates.
fn with_package_fixture(case: MappedCase) -> io::Result<()> {
    use crate::app::mapped_first_admission::runtime_binding::RetainedImage;

    // The maintained package gate supplies the actual paired resources for the
    // current host/runtime sources. Missing input is a failure, never a skip or
    // a fallback to the historical V11 initializer. Do not change global
    // HERMIT_INSTALL_DIR: the other controls keep their own inputs.
    let install = std::env::var_os("HERMIT_RETAINED_PACKAGE_TEST_INSTALL")
        .filter(|path| !path.is_empty())
        .ok_or_else(|| rpc("HERMIT_RETAINED_PACKAGE_TEST_INSTALL is required"))?;
    let retained = RetainedImage::open_test_install(std::path::Path::new(&install))?;
    let outcome = catch_unwind(AssertUnwindSafe(|| -> io::Result<()> {
        let initializer = BootstrapEntryCoordinates {
            entry: retained.entry(),
            terminal_int3: retained.terminal_int3(),
        };
        // SAFETY: the package gate authenticates the same current source/image
        // pair and the reviewed bounded valid-path entry/ABI contract, as for
        // mapped_run. Stable CPUID/XState, signal/filter lineage, original
        // ptracer thread, real RPC and whole-subtree containment remain required.
        // The retained pair and coordinates are DATA, not execution authority
        // or Ready. Initialize stops at initializer return and the map-only
        // cases after mapping; each cancels before Guest, and
        // mapped_packet_control retains every lease until real cleanup.
        unsafe { mapped_packet_control(case, retained.audited(), initializer) }
    }));
    // Close BOTH original OFDs after every Result and after confirmed-cleanup
    // panic. Neither a successful close nor a later error hides the real control.
    let close = retained.verify();
    match outcome {
        Err(payload) => {
            if let Err(error) = close
                && let Err(diagnostic) = catch_unwind(AssertUnwindSafe(|| {
                    eprintln!("independent retained-package close failure: {error}")
                }))
            {
                std::mem::forget(diagnostic);
            }
            resume_unwind(payload)
        }
        Ok(result) => match (result, close) {
            (result, Ok(())) => result,
            (result, Err(error)) => Err(rpc(format!(
                "retained-package close: {error}; control: {result:?}"
            ))),
        },
    }
}
