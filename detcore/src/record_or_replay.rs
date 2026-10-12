/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::future::Future;

use reverie::Error;
use reverie::Guest;
use reverie::Subscription;
use reverie::Tool;
use reverie::syscalls::Syscall;
use serde::Deserialize;
use serde::Serialize;

use crate::config::Config;
use crate::tool_global::GlobalState;

/// The inner tool that owns recorded syscall and fdinfo acquisition facts.
pub trait RecordOrReplay: Tool<GlobalState = GlobalState> {
    /// Preserve one serialized fdinfo fact in the current thread's existing
    /// event stream. With `None`, recording/noop tools return `None` without
    /// acquiring anything; replay consumes the next fact before any physical
    /// helper operation. With `Some`, preserve the already captured result.
    /// Replay returns recorded bytes; it does not infer a backend marker
    /// from a bare errno or replace a historical subject with a current path.
    /// This hook performs no guest injection, RPC, clock or scheduler turn.
    fn fdinfo_capture_fact<G: Guest<Self>>(
        &self,
        guest: &mut G,
        kind: u8,
        observed: Option<&[u8]>,
    ) -> impl Future<Output = Result<Option<Vec<u8>>, Error>> + Send;
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

impl RecordOrReplay for NoopTool {
    async fn fdinfo_capture_fact<G: Guest<Self>>(
        &self,
        _guest: &mut G,
        _kind: u8,
        observed: Option<&[u8]>,
    ) -> Result<Option<Vec<u8>>, Error> {
        Ok(observed.map(<[u8]>::to_vec))
    }
}
