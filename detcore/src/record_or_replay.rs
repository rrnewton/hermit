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
use serde::Deserialize;
use serde::Serialize;

use crate::config::Config;
use crate::tool_global::GlobalState;

/// Record/replay subtools have static identities so admission can distinguish
/// the exact no-op injector from wrappers that inspect or materialize exec.
pub trait RecordOrReplay: Tool<GlobalState = GlobalState> + 'static {
    /// Whether this is exactly the no-op subtool for retained-image exec.
    /// This does not grant backend executable authority or admit other calls.
    fn supports_parent_death_retained_exec() -> bool;
}

impl<T> RecordOrReplay for T
where
    T: Tool<GlobalState = GlobalState> + 'static,
{
    fn supports_parent_death_retained_exec() -> bool {
        std::any::TypeId::of::<T>() == std::any::TypeId::of::<NoopTool>()
    }
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
