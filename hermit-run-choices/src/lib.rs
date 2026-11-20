/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

//! Hermit's ptrace run with a choice source (`--choice-tape`,
//! `--choice-controller`), and the one place Detcore's controlled run mode,
//! `detcore::ControlledMode`, is instantiated.
//!
//! An ordinary run is `hermit-run-ptrace`'s own, whose Detcore is statically
//! the seeded run mode and carries no choice machinery. The controlled
//! instantiation is rooted here, in its own crate, so it builds in a separate
//! rustc process and adds nothing to the ordinary runner's instantiation.
//! `hermit_run_ptrace::spawn_tracer` hands a run here once, at launch, when
//! `Config::has_choice_source` says it has a choice source. The entry point
//! mirrors that one: it is not generic and returns a boxed future, so its
//! callers instantiate nothing of it.

use std::future::Future;
use std::pin::Pin;

use reverie::BackendStatsRequest;
use reverie::process::Command;
use reverie_ptrace::GdbConnection;

/// The controlled ptrace tracer: `detcore::Detcore<detcore::ControlledMode>`
/// under `reverie_ptrace`. Its global state is the ordinary runner's.
pub type Tracer = reverie_ptrace::Tracer<detcore::GlobalState>;

/// The future [`spawn_tracer`] returns.
pub type SpawnTracer = Pin<Box<dyn Future<Output = Result<Tracer, reverie::Error>>>>;

/// Spawns COMMAND under Detcore in the controlled run mode with CONFIG on the
/// ptrace backend, as `hermit_run_ptrace::spawn_tracer` does for an ordinary
/// run (see there for STATS, GDBSERVER and SEQUENTIALIZED_GUEST).
///
/// CONFIG must name a choice source. Without one the run would build no
/// session, and every thread would be refused for lacking one: this runner
/// refuses the config by name instead, before spawning anything.
pub fn spawn_tracer(
    command: Command,
    config: detcore::Config,
    gdbserver: Option<GdbConnection>,
    sequentialized_guest: bool,
    stats: BackendStatsRequest,
) -> SpawnTracer {
    Box::pin(async move {
        if !config.has_choice_source() {
            return Err(reverie::Error::Io(std::io::Error::other(
                "hermit-run-choices runs only a configuration with a choice source \
                 (--choice-tape or --choice-controller); a run without one is \
                 hermit-run-ptrace's seeded run",
            )));
        }
        let mut builder =
            reverie_ptrace::TracerBuilder::<detcore::Detcore<detcore::ControlledMode>>::new(
                command,
            )
            .config(config);
        if let Some(connection) = gdbserver {
            builder = builder.gdbserver(connection);
            if sequentialized_guest {
                builder = builder.sequentialized_guest();
            }
        }
        builder.backend_stats(stats).spawn().await
    })
}

#[cfg(test)]
mod tests {
    use std::task::Context;
    use std::task::Poll;
    use std::task::Waker;

    use super::*;

    /// A configuration without a choice source is refused by name on the
    /// future's first poll, before anything is spawned: this runner never
    /// starts a run that has no session for its threads to attach.
    #[test]
    fn a_configuration_without_a_choice_source_is_refused_by_name() {
        let mut spawned = spawn_tracer(
            Command::new("/bin/true"),
            detcore::Config::default(),
            None,
            false,
            BackendStatsRequest::DISABLED,
        );
        let refusal = match spawned
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
        {
            Poll::Ready(Err(error)) => error.to_string(),
            Poll::Ready(Ok(_)) => panic!("spawned a run without a choice source"),
            Poll::Pending => panic!("started spawning a run without a choice source"),
        };
        assert!(
            refusal.contains("hermit-run-choices runs only a configuration with a choice source"),
            "{refusal}"
        );
    }
}
