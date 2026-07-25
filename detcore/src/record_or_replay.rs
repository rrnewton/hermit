/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use async_trait::async_trait;
use reverie::Error;
use reverie::Guest;
use reverie::Subscription;
use reverie::Tool;
use reverie::syscalls::Syscall;
use serde::Deserialize;
use serde::Serialize;

use crate::config::Config;
use crate::tool_global::GlobalState;

/// The record/replay "subtool" plugged into `Detcore<T>`.
///
/// This is any Reverie [`Tool`] sharing Detcore's [`GlobalState`]: the recorder,
/// the replayer, or the [`NoopTool`] used by plain `hermit run`.
#[async_trait]
pub trait RecordOrReplay: Tool<GlobalState = GlobalState> {
    /// Record or replay a syscall on a container-INTERNAL file descriptor
    /// (currently pipes, whose two endpoints are both owned by guest processes).
    ///
    /// The default behaves exactly like [`Tool::handle_syscall_event`], which is
    /// correct for recording (inject the live syscall and log its result) and for
    /// internal READS on replay (the recorded bytes are written straight back into
    /// the guest buffer without touching any descriptor).
    ///
    /// The replayer overrides this for WRITES: an internal-pipe write must return
    /// its recorded byte count WITHOUT physically injecting anything. On replay the
    /// paired reader reproduces the transferred bytes from the log, so the internal
    /// pipe carries no live data. Re-injecting the write would at best be a wasted
    /// write into an undrained pipe and -- when the writer's fd number happens to be
    /// 1/2 while the guest's `dup2` redirections are not physically replayed (they
    /// are recorded as return-value-only `handle_simple` syscalls) -- would leak the
    /// bytes onto the real console and duplicate the program's output.
    async fn handle_internal_fd_syscall<G: Guest<Self>>(
        &self,
        guest: &mut G,
        syscall: Syscall,
    ) -> Result<i64, Error> {
        self.handle_syscall_event(guest, syscall).await
    }
}

impl RecordOrReplay for NoopTool {}

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
