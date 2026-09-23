/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use clap::Parser;
use hermit::Context;
use hermit::Error;
use hermit::HermitData;
use hermit::Id;
use hermit::Shebang;
use reverie::process::ExitStatus;

use super::container::deterministic_container;
use super::gdb_client::CLIENT_EXITED_BEFORE_CONNECTING;
use super::gdb_client::GdbClientWatch;
use super::global_opts::GlobalOpts;

/// Command-line options for the "replay" subcommand.
#[derive(Debug, Parser, Clone)]
pub struct ReplayOpts {
    /// The ID of the recording to replay. This is obtained by first running
    /// `hermit record`. This is optional. Defaults to the last successful
    /// recording.
    #[clap(value_name = "ID")]
    id: Option<Id>,

    /// Directory where recorded syscall data is stored.
    #[clap(long, value_name = "DIR", env = "HERMIT_DATA_DIR")]
    data_dir: Option<PathBuf>,

    /// The port to use for the gdb server.
    #[clap(long, default_value = "1234")]
    // FIXME: This shouldn't exist. It'd be better to use a Unix domain socket.
    gdbserver_port: u16,

    /// Replay to the end without an attached debugger.
    #[clap(long, short)]
    autopilot: bool,

    /// Serve the replay through the gdb remote protocol without launching gdb.
    #[clap(long, conflicts_with = "autopilot")]
    serve_only: bool,

    /// Additional gdb command passed by `-ex`
    #[clap(long, value_delimiter = ';', conflicts_with = "serve_only")]
    gdbex: Vec<String>,
}

impl ReplayOpts {
    pub fn main(&self, global: &GlobalOpts) -> Result<ExitStatus, Error> {
        let hermit = HermitData::from(self.data_dir.as_ref());

        let id = match self.id {
            Some(id) => id,
            None => hermit
                .last_id()
                .context("Failed to find last recording ID")?,
        };
        let mut prepared_replay = Some(hermit.prepare_replay(id)?);

        if self.autopilot || self.serve_only {
            let (mut container, identity) = deterministic_container()?;
            let options = self.clone();
            let global = global.clone();
            let resources = format!("replay {} and identity mounts", hermit.data_dir().display());
            super::owned_container::run(
                &mut container,
                (identity, hermit, prepared_replay),
                resources,
                true,
                "with_container",
                None,
                move |(_, _, prepared)| {
                    let prepared = prepared
                        .take()
                        .ok_or_else(|| Error::msg("replay trace reservation was consumed twice"))?;
                    options.container_main(&global, options.autopilot, prepared)
                },
            )
            .map(|(value, _guards)| value)
        } else {
            // Find the path to the executable so that GDB can use it to resolve
            // symbols.
            let exe = hermit.data_dir().join(format!("{}/exe", id));
            let real_exe = Shebang::new(&exe).map_or(exe, |s| s.interpreter().into());

            // Run the gdb client outside of the PID namespace. This cannot be done
            // inside of the PID namespace because it would perturb the
            // deterministic PID allocation that is needed for the replay.
            let mut gdb_command = std::process::Command::new("gdb");
            gdb_command
                .arg(real_exe)
                .arg("-quiet")
                .arg("-iex")
                // don't prompt (dialog) when breakpoint symbol doesn't exist.
                .arg("set breakpoint pending on")
                .arg("-ex")
                .arg(format!("target remote :{}", self.gdbserver_port));
            for ex in &self.gdbex {
                gdb_command.arg("-ex").arg(ex);
            }

            // TODO: For replay, we ought to construct the container from
            // `metadata.json`. That logic belongs in `hermit::replay`, but we have
            // to initialize logging inside the container because it may spawn a
            // thread. If we can guarantee that tracing won't spawn a thread, then
            // that restriction be lifted.
            // Same unbounded accept as the record path: the client is spawned
            // before the container that binds the port, so a client that dies
            // early leaves the gdbserver waiting for a peer that cannot arrive.
            // ⚠️ CORRECTION TO WHAT LANDED HERE. This comment used to claim this
            // file's `wait()` "WAS reached on every path ... so it never leaked a
            // zombie", and that only the record path leaked. That is wrong:
            // `deterministic_container()?` on the next line precedes the wait, so
            // an error building the container skipped it here too. BOTH files
            // leaked, and the merged description of hermit#2654 says otherwise.
            // Recorded rather than quietly deleted, because the claim is in a
            // landed commit message where it cannot be edited.
            let gdb_watch = GdbClientWatch::spawn(gdb_command, self.gdbserver_port)?;
            let (mut container, identity) = deterministic_container()?;
            let guards = Rc::new(RefCell::new((identity, hermit, gdb_watch, prepared_replay)));
            let resources = format!(
                "replay {}, identity mounts and GDB watcher",
                guards.borrow().1.data_dir().display()
            );
            let options = self.clone();
            let global = global.clone();
            let result = super::owned_container::run(
                &mut container,
                Rc::clone(&guards),
                resources,
                true,
                "with_container",
                None,
                move |guards| {
                    let prepared =
                        guards.borrow_mut().3.take().ok_or_else(|| {
                            Error::msg("replay trace reservation was consumed twice")
                        })?;
                    options.container_main(&global, options.autopilot, prepared)
                },
            );
            // On unresolved cleanup the factory retains the SAME guard scope.
            // Do not signal watcher completion while its container still owns it.
            if result.as_ref().is_err_and(|e| {
                e.downcast_ref::<super::owned_container::ParentCleanupUnconfirmed>()
                    .is_some()
            }) {
                return result.map(|(status, _)| status);
            }
            let finished = guards.borrow_mut().2.finish();
            match (result, finished) {
                (Ok((status, _)), Ok(_)) => Ok(status),
                (Ok(_), Err(watcher)) => Err(watcher),
                (Err(primary), Err(watcher)) => {
                    Err(primary.context(format!("GDB watcher also failed: {watcher:#}")))
                }
                (Err(primary), Ok(true)) => Err(primary.context(CLIENT_EXITED_BEFORE_CONNECTING)),
                (Err(primary), Ok(false)) => Err(primary),
            }
        }
    }

    fn container_main(
        &self,
        global: &GlobalOpts,
        autopilot: bool,
        prepared_replay: hermit::PreparedFullReplayTrace,
    ) -> Result<ExitStatus, Error> {
        let _guard = global.init_tracing();
        // Both callers enter through deterministic_container/with_container.
        let prepared_replay = prepared_replay.with_owned_controller_shutdown();

        if autopilot {
            hermit::replay_from(prepared_replay)
        } else {
            hermit::replay_with_gdbserver(prepared_replay, self.gdbserver_port)
        }
    }
}
