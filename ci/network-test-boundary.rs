// Copyright (c) Meta Platforms, Inc. and affiliates.
// All rights reserved.
// Licensed under the BSD-style license in the repository LICENSE file.

//! Exact identities eligible for the official network-test cgroup capability.

pub const REQUEST_ENV: &str = "HERMIT_NEXTEST_NETWORK_BOUNDARY";
pub const FD_ENV: &str = "HERMIT_NEXTEST_NETWORK_CGROUP_FD";
pub const CAUSE_FD_ENV: &str = "HERMIT_NEXTEST_NETWORK_CAUSE_FD";
pub const CASE_ENV: &str = "HERMIT_NEXTEST_NETWORK_CASE";
pub const CLI_ENV: &str = "HERMIT_PREPARED_NETWORK_CLI";
pub const MEMORY_BYTES: u64 = 16 * 1024 * 1024 * 1024;

pub const CASES: &[(&str, &str)] = &[
    (
        "tcp",
        "network_only::external_tcp_recording_replays_offline_across_schedules_and_refuses_mismatch",
    ),
    (
        "poll",
        "network_only::blocked_poll_replays_current_lowat_across_schedules",
    ),
    (
        "recv",
        "network_only::blocked_receive_replays_saved_target_across_schedules",
    ),
    (
        "identity",
        "network_channel_identity::endpoint_identity_replays_creation_orders_and_refuses_wrong_peers",
    ),
    (
        "unix",
        "network_unix::default_unix_denies_external_contact_and_preserves_guest_ipc",
    ),
];

pub fn authorize(request: &str, package: &str, binary: &str, test: &str) -> bool {
    package == "hermit"
        && binary == "hermit::record_replay"
        && CASES
            .iter()
            .any(|(case, name)| request == *case && test == *name)
}

/// Fixed nonblocking pipe messages reach the outside owner before cgroup.kill.
/// No disk or blocking stderr write is on the lethal path.
pub fn cause_frame(reason: u8) -> [u8; 2] {
    assert!((1..=4).contains(&reason));
    [1, reason]
}

pub fn decode_causes(bytes: &[u8]) -> Option<Vec<&'static str>> {
    if bytes.len() > 8 || bytes.len() % 2 != 0 {
        return None;
    }
    bytes
        .chunks_exact(2)
        .map(|frame| {
            if !(1..=4).contains(&frame[1]) || frame != cause_frame(frame[1]) {
                return None;
            }
            Some(match frame[1] {
                1 => "wall deadline",
                2 => "stream byte bound",
                3 => "stream capture error",
                4 => "unfinished cell",
                _ => return None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_declared_case_requires_its_exact_test_identity() {
        for (case, name) in CASES {
            assert!(authorize(case, "hermit", "hermit::record_replay", name));
            assert!(!authorize(case, "foreign", "hermit::record_replay", name));
            assert!(!authorize(case, "hermit", "record_replay", name));
            assert!(!authorize(
                case,
                "hermit",
                "hermit::record_replay",
                "record_replay_matrix"
            ));
            for (other, _) in CASES {
                assert_eq!(
                    authorize(other, "hermit", "hermit::record_replay", name),
                    other == case
                );
            }
        }
        assert_eq!(decode_causes(&[]), Some(vec![]));
        for reason in 1..=4 {
            assert_eq!(decode_causes(&cause_frame(reason)).unwrap().len(), 1);
        }
        for invalid in [
            vec![1],
            vec![0, 1],
            vec![1, 0],
            vec![1, 5],
            vec![1, 1, 1, 2, 1, 3, 1, 4, 1, 1],
        ] {
            assert_eq!(decode_causes(&invalid), None);
        }
        assert!(!authorize(
            "",
            "hermit",
            "hermit::record_replay",
            CASES[0].1
        ));
        assert!(!authorize(
            "unknown",
            "hermit",
            "hermit::record_replay",
            CASES[0].1
        ));
    }
}
