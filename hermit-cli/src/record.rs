/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 * All rights reserved.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree.
 */

use std::fs;
use std::path::Path;
use std::path::PathBuf;

use detcore::network_replay::NetworkTracePublication;
use detcore_model::config::Epoch;
use detcore_model::config::MountInfoRootRewrite;
use reverie::ExitStatus;
use reverie::process::Command;
use reverie::process::Output;

use crate::consts::EXE_NAME;
use crate::consts::METADATA_NAME;
use crate::error::Context;
use crate::error::Error;
use crate::metadata::FullReplayPhase;
use crate::metadata::Metadata;
use crate::metadata::NETWORK_TRACE_NAME;
use crate::metadata::finalize_network_trace_recording;
use crate::metadata::prepare_network_trace_recording;
use crate::metadata::record_or_replay_config;
use crate::recorder::Recorder;

type RecordTool = detcore::Detcore<Recorder>;
type Tracer = reverie_ptrace::Tracer<detcore::GlobalState>;

/// Host-namespace reservation for one full-record network sidecar.
///
/// Construct this before entering any Hermit container. It holds the private
/// no-follow/no-clobber publication file open until [`Record`] has finished
/// producing, validating, and atomically publishing the trace.
#[derive(Debug)]
pub struct PreparedFullRecordTrace {
    data: PathBuf,
    publication: NetworkTracePublication,
}

impl PreparedFullRecordTrace {
    /// Reserve the full-record sidecar in the caller's current namespace.
    pub fn reserve(data: &Path) -> Result<Self, Error> {
        prepare_network_trace_recording(data)?;
        let publication = NetworkTracePublication::reserve(&data.join(NETWORK_TRACE_NAME))
            .with_context(
                || "Failed to reserve the full-record network trace in the host namespace",
            )?;
        Ok(Self {
            data: data.to_path_buf(),
            publication,
        })
    }
}

/// Represents a recording that is currently running.
pub struct Record {
    /// The running tracee.
    tracer: Tracer,
    metadata: Metadata,
    metadata_path: PathBuf,
    network_publication: NetworkTracePublication,
}

impl Record {
    /// Spawns a recording with exact mountinfo provenance captured from the
    /// completed recording container namespace.
    pub async fn spawn_with_mountinfo(
        command: Command,
        prepared_trace: PreparedFullRecordTrace,
        mountinfo_root_rewrites: Vec<MountInfoRootRewrite>,
        mountinfo_mount_ids: Option<Vec<u64>>,
        epoch: Epoch,
    ) -> Result<Self, Error> {
        let PreparedFullRecordTrace {
            data,
            publication: network_publication,
        } = prepared_trace;
        let dir = data.as_path();
        let mut metadata = Metadata::new(&command, epoch)?;
        metadata.mountinfo_root_rewrites = mountinfo_root_rewrites;
        metadata.mountinfo_mount_ids_captured = mountinfo_mount_ids.is_some();
        metadata.mountinfo_mount_ids = mountinfo_mount_ids.unwrap_or_default();

        let exe = dir.join(EXE_NAME);

        // Record the full program executable to `{hermit_data}/{id}/exe`.
        //
        // TODO: Handle shebang lines.
        fs::copy(&metadata.exe, &exe)
            .with_context(|| format!("Failed to record {:?}", metadata.exe))?;

        let metadata_path = dir.join(METADATA_NAME);
        serde_json::to_writer_pretty(fs::File::create(&metadata_path)?, &metadata)
            .context("Failed to serialize metadata")?;

        let mut config = record_or_replay_config(dir, FullReplayPhase::Record, metadata.epoch);
        config.network_trace_output_fd = Some(network_publication.writer_fd());
        config.mountinfo_root_rewrites = metadata.mountinfo_root_rewrites.clone();
        config.mountinfo_mount_ids = metadata.mountinfo_mount_ids.clone();
        config.mountinfo_mount_ids_captured = metadata.mountinfo_mount_ids_captured;
        config.fdinfo_unlisted_mount_ids = metadata.fdinfo_unlisted_mount_ids.clone();

        let tracer = reverie_ptrace::TracerBuilder::<RecordTool>::new(command)
            .config(config)
            .spawn()
            .await?;

        Ok(Self {
            tracer,
            metadata,
            metadata_path,
            network_publication,
        })
    }

    fn persist_mount_identity_provenance(
        metadata: &mut Metadata,
        metadata_path: &Path,
        global_state: &detcore::GlobalState,
    ) -> Result<(), Error> {
        if let Some(provenance) = global_state
            .mount_identity_provenance()
            .map_err(Error::msg)?
        {
            metadata.mountinfo_mount_ids = provenance.mountinfo_order;
            metadata.mountinfo_mount_ids_captured = true;
            metadata.fdinfo_unlisted_mount_ids = provenance.unlisted_order;
        }
        let directory = metadata_path
            .parent()
            .ok_or_else(|| Error::msg("recording metadata path has no parent"))?;
        let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
        serde_json::to_writer_pretty(temporary.as_file_mut(), metadata)
            .context("Failed to serialize final recording metadata")?;
        temporary.as_file().sync_all()?;
        temporary
            .persist(metadata_path)
            .map_err(|error| error.error)
            .context("Failed to persist final recording metadata")?;
        fs::File::open(directory)?.sync_all()?;
        Ok(())
    }

    /// Waits for the recording to finish and returns its exit status.
    pub async fn wait(self) -> Result<ExitStatus, Error> {
        let Self {
            tracer,
            mut metadata,
            metadata_path,
            network_publication,
        } = self;
        let (exit_status, mut global_state) = tracer.wait().await?;
        let persist = global_state
            .finalize_network_trace()
            .map_err(Error::msg)
            .and_then(|()| finalize_network_trace_recording(network_publication, metadata.epoch))
            .and_then(|artifact| {
                metadata.network_trace = Some(artifact);
                Self::persist_mount_identity_provenance(
                    &mut metadata,
                    &metadata_path,
                    &global_state,
                )
            });
        global_state.clean_up(false, &None).await;
        persist?;
        Ok(exit_status)
    }

    /// Waits for the recording to finish and collects its output.
    pub async fn wait_with_output(self) -> Result<Output, Error> {
        let Self {
            tracer,
            mut metadata,
            metadata_path,
            network_publication,
        } = self;
        let (output, mut global_state) = tracer.wait_with_output().await?;
        let persist = global_state
            .finalize_network_trace()
            .map_err(Error::msg)
            .and_then(|()| finalize_network_trace_recording(network_publication, metadata.epoch))
            .and_then(|artifact| {
                metadata.network_trace = Some(artifact);
                Self::persist_mount_identity_provenance(
                    &mut metadata,
                    &metadata_path,
                    &global_state,
                )
            });
        global_state.clean_up(false, &None).await;
        persist?;
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepared_trace_refuses_destination_symlinks_without_clobbering() {
        let directory = tempfile::tempdir().unwrap();
        let victim = directory.path().join("victim");
        fs::write(&victim, b"unchanged").unwrap();
        std::os::unix::fs::symlink(&victim, directory.path().join(NETWORK_TRACE_NAME)).unwrap();

        let error = PreparedFullRecordTrace::reserve(directory.path()).unwrap_err();
        assert!(error.to_string().contains("refusing to overwrite"));
        assert_eq!(fs::read(victim).unwrap(), b"unchanged");
    }

    #[test]
    fn prepared_trace_refuses_a_symlinked_parent_directory() {
        let outer = tempfile::tempdir().unwrap();
        let real = outer.path().join("real");
        fs::create_dir(&real).unwrap();
        let alias = outer.path().join("alias");
        std::os::unix::fs::symlink(&real, &alias).unwrap();

        let error = PreparedFullRecordTrace::reserve(&alias).unwrap_err();
        assert!(
            error.to_string().contains("host namespace"),
            "unexpected refusal: {error:#}"
        );
        assert!(!real.join(NETWORK_TRACE_NAME).exists());
    }
}
