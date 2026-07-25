/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::io::Write;
use std::path::Path;

use reverie::ExitStatus;
use reverie::process::Command;
use reverie::process::Output;

use crate::Backend;
use crate::KvmToolExecution;
use crate::consts::EXE_NAME;
use crate::consts::METADATA_NAME;
use crate::error::Context;
use crate::error::Error;
use crate::metadata::Metadata;
use crate::metadata::record_or_replay_config;
use crate::recorder::Recorder;
use crate::run_kvm_with_tool;

type RecordTool = detcore::Detcore<Recorder>;
type Tracer = reverie_ptrace::Tracer<detcore::GlobalState>;

/// Represents a recording that is currently running.
pub struct Record {
    /// The running tracee.
    tracer: Tracer,
}

fn prepare_recording(command: &Command, dir: &Path, backend: Backend) -> Result<(), Error> {
    let metadata = Metadata::new(command, backend)?;
    fs::copy(&metadata.exe, dir.join(EXE_NAME))
        .with_context(|| format!("Failed to record {:?}", metadata.exe))?;
    serde_json::to_writer_pretty(fs::File::create(dir.join(METADATA_NAME))?, &metadata)
        .context("Failed to serialize metadata")
}

impl Record {
    /// Spawns a new recording.
    pub async fn spawn(command: Command, dir: &Path) -> Result<Self, Error> {
        prepare_recording(&command, dir, Backend::Ptrace)?;
        let config = record_or_replay_config(dir);

        let tracer = reverie_ptrace::TracerBuilder::<RecordTool>::new(command)
            .config(config)
            .spawn()
            .await?;

        Ok(Self { tracer })
    }

    /// Waits for the replay to finish and returns its exit status.
    pub async fn wait(self) -> Result<ExitStatus, reverie::Error> {
        let (exit_status, global_state) = self.tracer.wait().await?;
        global_state.clean_up(false, &None).await;
        Ok(exit_status)
    }

    /// Waits for the replay to finish and collects its output.
    pub async fn wait_with_output(self) -> Result<Output, reverie::Error> {
        let (output, global_state) = self.tracer.wait_with_output().await?;
        global_state.clean_up(false, &None).await;
        Ok(output)
    }
}

// AUTONOMOUS-BOT-IMPLEMENTED
// TODO-HUMAN-REVIEW(PR-TBD): Review Recorder behavior on the KVM guest personality.
pub async fn run_kvm(command: Command, dir: &Path, capture_output: bool) -> Result<Output, Error> {
    prepare_recording(&command, dir, Backend::Kvm)?;
    let config = record_or_replay_config(dir);
    let KvmToolExecution {
        global_state,
        output,
    } = run_kvm_with_tool::<RecordTool>(&command, config, capture_output, None).await?;
    global_state.clean_up(false, &None).await;

    if !capture_output {
        std::io::stdout().write_all(&output.stdout)?;
        std::io::stderr().write_all(&output.stderr)?;
    }

    Ok(output)
}
