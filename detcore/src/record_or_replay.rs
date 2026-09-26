/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use reverie::Error;
use reverie::Guest;
use reverie::Subscription;
use reverie::Tool;
use reverie::syscalls::Syscall;
use reverie::syscalls::Sysno;
use serde::Deserialize;
use serde::Serialize;

use crate::config::Config;
use crate::tool_global::GlobalState;

/// Helper trait.
pub trait RecordOrReplay: Tool<GlobalState = GlobalState> {}

impl<T> RecordOrReplay for T where T: Tool<GlobalState = GlobalState> {}

/// Only the existing captured-clock handlers may replace virtual time in
/// record/replay mode. A data directory alone is not a capture policy, and the
/// outer Detcore subscription does not prove that the nested tool handles it.
pub(crate) fn has_captured_clock_handler<T: RecordOrReplay>(
    config: &Config,
    syscall: Sysno,
) -> bool {
    config.recordreplay_modes
        && config.replay_data.is_some()
        && matches!(
            syscall,
            Sysno::clock_gettime | Sysno::gettimeofday | Sysno::time
        )
        && T::subscriptions(config)
            .iter_syscalls()
            .any(|subscribed| subscribed == syscall)
}

/// A tool that only injects the syscall it receives.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct NoopTool;

#[reverie::tool]
impl Tool for NoopTool {
    type GlobalState = GlobalState;
    type ThreadState = ();

    fn subscriptions(_cfg: &Config) -> Subscription {
        // Don't subscribe to anything by default. This noop-tool doesn't care
        // about any syscalls and will be ORed with the detcore subscriptions.
        Subscription::none()
    }

    async fn handle_syscall_event<T: Guest<Self>>(
        &self,
        guest: &mut T,
        call: Syscall,
    ) -> Result<i64, Error> {
        // NOTE: Cannot use tail_inject here as that would prevent any detcore
        // post-hook code from running.
        Ok(guest.inject(call).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default, Serialize, Deserialize)]
    struct ClockSubscriber;

    #[reverie::tool]
    impl Tool for ClockSubscriber {
        type GlobalState = GlobalState;
        type ThreadState = ();

        fn subscriptions(_config: &Config) -> Subscription {
            // Deliberately overbroad: subscription is necessary, not sufficient.
            Subscription::all()
        }
    }

    fn recording_config() -> Config {
        Config {
            virtualize_time: false,
            recordreplay_modes: true,
            replay_data: Some("recording".into()),
            ..Config::default()
        }
    }

    #[test]
    fn captured_clock_routing_never_exempts_other_subscribed_syscalls() {
        let config = recording_config();
        let routed: Vec<_> = crate::all_pinned_syscalls()
            .filter(|syscall| has_captured_clock_handler::<ClockSubscriber>(&config, *syscall))
            .collect();
        assert_eq!(
            routed,
            [Sysno::gettimeofday, Sysno::time, Sysno::clock_gettime]
        );
    }

    #[test]
    fn captured_clock_routing_requires_both_recording_policy_and_data() {
        for syscall in [Sysno::clock_gettime, Sysno::gettimeofday, Sysno::time] {
            let mut config = recording_config();
            config.recordreplay_modes = false;
            assert!(!has_captured_clock_handler::<ClockSubscriber>(
                &config, syscall
            ));
            config.recordreplay_modes = true;
            config.replay_data = None;
            assert!(!has_captured_clock_handler::<ClockSubscriber>(
                &config, syscall
            ));
        }
    }

    #[test]
    fn captured_clock_routing_refuses_noop_even_with_recording_policy() {
        let config = recording_config();
        for syscall in [Sysno::clock_gettime, Sysno::gettimeofday, Sysno::time] {
            assert!(!has_captured_clock_handler::<NoopTool>(&config, syscall));
        }
    }
}
