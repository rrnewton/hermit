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

/// Selects the delegate that will produce an original file operation's result.
/// This declaration is not physical authority: the native branch still requires
/// the actual backend Prepared/Returned and provider selection observations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OriginalFileExecution {
    /// Use the original native invocation; actual backend/provider observations remain required.
    Native,
    /// Consume the existing recorded event without claiming a native invocation or result.
    Recorded,
}

/// The original syscall is delegated at its existing record/replay boundary.
/// Recorded values retain their event provenance and never become native facts.
pub trait RecordOrReplay: Tool<GlobalState = GlobalState> {
    /// Declares which existing delegate path supplies this original file operation.
    fn original_file_execution(&self, _call: Syscall) -> OriginalFileExecution {
        OriginalFileExecution::Native
    }
    /// Delegates one admitted Read at the shared native or recorded boundary.
    /// Native implementations preserve typed interruption before serialization;
    /// Recorded implementations consume the matching result or control and
    /// obtain actual stopped-task custody before handing back an interruption.
    /// Legacy `Guest::inject` cannot supply that control transition. Only actual
    /// backend/provider observations authenticate a native completion.
    fn invoke_original_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: reverie::syscalls::Read,
    ) -> impl std::future::Future<Output = Result<reverie::InjectedReadResult, Error>> + Send;
    /// Consumes the matching semantic result from the delegate's existing recorded event stream.
    fn consume_recorded_original_file<G: Guest<Self>>(
        &self,
        _guest: &mut G,
        _call: Syscall,
    ) -> impl std::future::Future<Output = Result<i64, Error>> + Send {
        async {
            Err(Error::Tool(anyhow::anyhow!(
                "delegate has no recorded original-file result"
            )))
        }
    }
}

impl RecordOrReplay for NoopTool {
    async fn invoke_original_read<G: Guest<Self>>(
        &self,
        guest: &mut G,
        call: reverie::syscalls::Read,
    ) -> Result<reverie::InjectedReadResult, Error> {
        Ok(guest.inject_original_read(call).await)
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
