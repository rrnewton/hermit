/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::path::Path;

use detcore_model::config::MountInfoRootRewrite;
use reverie::ExitStatus;
use reverie::process::Command;
use reverie::process::Output;

use crate::consts::EXE_NAME;
use crate::consts::METADATA_NAME;
use crate::error::Context;
use crate::error::Error;
use crate::metadata::Metadata;
use crate::metadata::record_or_replay_config;
use crate::recorder::Recorder;

type RecordTool = detcore::Detcore<Recorder>;
type Tracer = reverie_ptrace::Tracer<detcore::GlobalState>;

/// Represents a recording that is currently running.
pub struct Record {
    /// The running tracee.
    tracer: Tracer,
}

impl Record {
    /// Spawns a recording with exact mountinfo provenance captured from the
    /// completed recording container namespace.
    pub async fn spawn_with_mountinfo(
        command: Command,
        dir: &Path,
        mountinfo_root_rewrites: Vec<MountInfoRootRewrite>,
        mountinfo_mount_ids: Vec<u64>,
        mountinfo_mount_id_prefix_len: usize,
    ) -> Result<Self, Error> {
        let mut metadata = Metadata::new(&command)?;
        metadata.mountinfo_root_rewrites = mountinfo_root_rewrites;
        metadata.mountinfo_mount_ids = mountinfo_mount_ids;
        metadata.mountinfo_mount_id_prefix_len = mountinfo_mount_id_prefix_len;

        let exe = dir.join(EXE_NAME);

        // Record the full program executable to `{hermit_data}/{id}/exe`.
        //
        // TODO: Handle shebang lines.
        fs::copy(&metadata.exe, &exe)
            .with_context(|| format!("Failed to record {:?}", metadata.exe))?;

        serde_json::to_writer_pretty(fs::File::create(dir.join(METADATA_NAME))?, &metadata)
            .context("Failed to serialize metadata")?;

        let mut config = record_or_replay_config(dir);
        config.mountinfo_root_rewrites = metadata.mountinfo_root_rewrites.clone();
        config.mountinfo_mount_ids = metadata.mountinfo_mount_ids.clone();
        config.mountinfo_mount_id_prefix_len = metadata.mountinfo_mount_id_prefix_len;

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
