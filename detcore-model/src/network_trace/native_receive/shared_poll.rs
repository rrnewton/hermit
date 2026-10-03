//! Shared Poll provenance is recorded before the physical observation. Legacy
//! traces cannot acquire that authority from controls at later release time.
use super::*;

pub(super) fn validate_policy(trace: &NetworkTraceV4) -> Validation {
    let shared = matches!(
        trace.release_model,
        NetworkReleaseModelV4::SerializedSharedMmAttemptsV1 { .. }
    );
    for input in &trace.inputs {
        match input.event {
            NetworkInputKindV2::SharedRawTcpPollState {
                receive_low_water, ..
            } if !shared || receive_low_water == 0 || receive_low_water > i32::MAX as u32 => {
                return Err(Invalid::InvalidNativeObservation);
            }
            NetworkInputKindV2::RawTcpPollState { .. } if shared => {
                return Err(Invalid::InvalidNativeObservation);
            }
            _ => {}
        }
    }
    Ok(())
}

/// Only the private payload validator consumes this projection. The original
/// V4 event and release metadata remain authoritative and unchanged.
pub(super) fn payload(input: &NetworkInputKindV2) -> NetworkInputKindV2 {
    match *input {
        NetworkInputKindV2::SharedRawTcpPollState {
            consumed_prefix,
            revents,
            ..
        } => NetworkInputKindV2::RawTcpPollState {
            consumed_prefix,
            revents,
        },
        _ => input.clone(),
    }
}
