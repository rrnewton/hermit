//! Characterization of the retention frame and its retirement helpers, as
//! they behave before they move into Reverie unchanged. The bodies below
//! use only what is the same on both sides of that move: the frame's
//! methods, never its fields. After the move the same bodies must pass
//! there, and only the `use` lines may differ.

use std::fs::File;
use std::os::fd::AsFd;

use reverie_inguest::launcher::exec_stop::CleanupReceipt;
use reverie_inguest::launcher::exec_stop::ExecStop;
use reverie_inguest::launcher::exec_stop::PreparedExec;
use reverie_inguest::launcher::exec_stop::physical::Preparation;
use reverie_loader::handoff::Extent;

use super::RetainedPhysical;
use super::RetirementStep;
use super::guarded_retirement;
use super::retain_unwind;

fn true_request<'fd>() -> PreparedExec<'fd> {
    PreparedExec::new(
        std::path::Path::new("/bin/true"),
        &["retention".into()],
        &[],
        None,
        Vec::new(),
    )
    .unwrap()
}

#[test]
fn an_empty_frame_holds_no_capability_and_names_each_absence() {
    let mut frame = RetainedPhysical::empty(CleanupReceipt::default());
    assert!(!frame.has_cleanup_capability());
    assert_eq!(
        frame.initial().err().map(|error| error.to_string()),
        Some("actual initial stop absent".to_owned())
    );
    assert_eq!(
        frame.mapped().err().map(|error| error.to_string()),
        Some("actual prepared mapping absent".to_owned())
    );
    frame.retire_once();
    assert!(!frame.has_cleanup_capability());
}

#[test]
#[allow(unsafe_code)]
fn preparing_without_an_initial_stop_refuses_by_name() {
    let null = File::open("/dev/null").unwrap();
    let mut frame = RetainedPhysical::empty(CleanupReceipt::default());
    // SAFETY: the frame holds no initial stop, so prepare refuses before it
    // reads any input; no child exists.
    let refused = unsafe {
        frame.prepare(Preparation {
            runtime_fd: null.as_fd(),
            runtime_bytes: b"",
            layout_address: 0,
            handoff_fd: null.as_fd(),
            handoff_bytes: b"",
            handoff_address: 0,
            input_fd: null.as_fd(),
            free_band: Extent { start: 0, end: 0 },
            filter_scratch: 0,
        })
    };
    assert_eq!(
        refused.err().map(|error| error.to_string()),
        Some("actual initial stop absent".to_owned())
    );
    assert!(!frame.has_cleanup_capability());
}

#[test]
fn a_real_initial_stop_is_retained_then_retired_once_with_its_reap_confirmed() {
    let receipt = CleanupReceipt::default();
    let mut frame = RetainedPhysical::empty(receipt.clone());
    frame.retain_spawn(ExecStop::spawn(true_request(), receipt.clone()));
    assert!(frame.initial().is_ok());
    assert!(frame.has_cleanup_capability());
    assert_eq!(
        frame.mapped().err().map(|error| error.to_string()),
        Some("actual prepared mapping absent".to_owned())
    );
    let state = receipt.snapshot();
    assert!(state.child_created);
    assert!(!state.confirmed);

    frame.retire_once();
    let state = receipt.snapshot();
    assert!(state.confirmed);
    assert!(!frame.has_cleanup_capability());
    assert_eq!(
        frame.initial().err().map(|error| error.to_string()),
        Some("actual initial stop absent".to_owned())
    );
    // A second retirement finds nothing left to retire.
    frame.retire_once();
    assert!(!frame.has_cleanup_capability());
    assert!(receipt.snapshot().confirmed);
}

#[test]
fn a_launch_failure_is_retained_named_and_never_released_by_retirement() {
    let receipt = CleanupReceipt::default();
    let mut first = RetainedPhysical::empty(receipt.clone());
    first.retain_spawn(ExecStop::spawn(true_request(), receipt.clone()));
    assert!(first.initial().is_ok());

    // The same receipt cannot launch twice: a real LaunchFailure.
    let failure = match ExecStop::spawn(true_request(), receipt.clone()) {
        Ok(_) => panic!("a used receipt must refuse the launch"),
        Err(failure) => failure,
    };
    assert_eq!(
        failure.error.to_string(),
        "exec-stop cleanup receipt already used"
    );
    let expected = format!("actual launch refused: {failure:?}");
    let mut refused = RetainedPhysical::empty(CleanupReceipt::default());
    refused.retain_spawn(Err(failure));
    assert!(refused.has_cleanup_capability());
    assert_eq!(
        refused.initial().err().map(|error| error.to_string()),
        Some(expected)
    );
    assert_eq!(
        refused.mapped().err().map(|error| error.to_string()),
        Some("actual prepared mapping absent".to_owned())
    );
    refused.retire_once();
    assert!(refused.has_cleanup_capability());

    first.retire_once();
    assert!(receipt.snapshot().confirmed);
    assert!(!first.has_cleanup_capability());
}

#[test]
fn a_real_frames_retirement_step_completes_once_its_reap_is_confirmed() {
    let receipt = CleanupReceipt::default();
    let mut frame = RetainedPhysical::empty(receipt.clone());
    frame.retain_spawn(ExecStop::spawn(true_request(), receipt.clone()));
    let mut first = None;
    let step = guarded_retirement(
        &mut frame,
        &mut first,
        |root| root.retire_once(),
        |root| {
            let state = receipt.snapshot();
            (
                state.child_created,
                state.confirmed,
                root.has_cleanup_capability(),
            )
        },
    );
    assert_eq!(step, RetirementStep::Complete);
    assert!(first.is_none());
    assert!(receipt.snapshot().confirmed);
    assert!(!frame.has_cleanup_capability());
}

#[test]
fn retain_unwind_returns_the_value_and_keeps_only_the_first_payload() {
    let mut first = None;
    assert_eq!(retain_unwind(&mut first, || 7), Some(7));
    assert!(first.is_none());
    assert_eq!(
        retain_unwind(&mut first, || -> u32 { std::panic::panic_any("first") }),
        None
    );
    assert_eq!(
        retain_unwind(&mut first, || -> u32 { std::panic::panic_any("second") }),
        None
    );
    assert_eq!(retain_unwind(&mut first, || 8), Some(8));
    let payload = first.expect("the first payload is retained");
    assert_eq!(payload.downcast_ref::<&str>(), Some(&"first"));
}
