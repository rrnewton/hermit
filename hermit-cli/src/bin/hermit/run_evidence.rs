use std::fs;
use std::fs::File;
use std::fs::OpenOptions;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::unix::fs::DirBuilderExt;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;

use hermit::Backend;
use hermit::Context;
use hermit::Error;
use hermit::canonical_verdict::RecordEnvelopeReport;
use hermit::run_evidence::CanonicalInfoEvidence;
use hermit::run_evidence::CanonicalInfoPolicy;
use hermit::run_evidence::DispositionLimitation;
use hermit::run_evidence::GuestDisposition;
use hermit::run_evidence::RUN_EVIDENCE_INFO_ARTIFACT;
use hermit::run_evidence::RUN_EVIDENCE_MANIFEST;
use hermit::run_evidence::RUN_EVIDENCE_SCHEMA_VERSION;
use hermit::run_evidence::RunEvidenceBackend;
use hermit::run_evidence::RunEvidenceNoResultReason;
use hermit::run_evidence::RunEvidenceOutcome;
use hermit::run_evidence::RunEvidenceReport;
use reverie::process::ExitStatus;
use uuid::Uuid;

pub(crate) struct RunEvidenceSession {
    directory: PathBuf,
    invocation_id: Uuid,
    backend: Backend,
    raw_log: Arc<File>,
}

impl RunEvidenceSession {
    /// Atomically claim a destination that did not exist before this invocation.
    pub(crate) fn create(directory: &Path, backend: Backend) -> Result<Self, Error> {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(directory)
            .with_context(|| {
                format!(
                    "cannot create --run-evidence-dir {}: the destination must not already exist",
                    directory.display()
                )
            })?;
        let raw_log = tempfile::tempfile().with_context(|| {
            format!(
                "cannot create private run-evidence capture for {}",
                directory.display()
            )
        })?;
        Ok(Self {
            directory: directory.to_path_buf(),
            invocation_id: Uuid::new_v4(),
            backend,
            raw_log: Arc::new(raw_log),
        })
    }

    pub(crate) fn log_handle(&self) -> Arc<File> {
        Arc::clone(&self.raw_log)
    }

    /// Publish the terminal manifest after the guest result and log artifact are final.
    pub(crate) fn finish(self, run_result: Result<&ExitStatus, &Error>) -> Result<(), Error> {
        let observed_disposition = run_result
            .ok()
            .and_then(|status| guest_disposition(self.backend, *status));
        let unsupported = !matches!(
            self.backend,
            Backend::Ptrace | Backend::Liteinst | Backend::Kvm
        );

        let (outcome, canonical_info) = if unsupported {
            (
                RunEvidenceOutcome::NoResult {
                    reason: RunEvidenceNoResultReason::UnsupportedBackend,
                    observed_disposition,
                },
                CanonicalInfoEvidence::no_result(),
            )
        } else if run_result.is_err() {
            (
                RunEvidenceOutcome::NoResult {
                    reason: RunEvidenceNoResultReason::RunFailed,
                    observed_disposition: None,
                },
                CanonicalInfoEvidence::no_result(),
            )
        } else if let Some(disposition) = observed_disposition {
            match self.publish_canonical_info() {
                Ok(canonical_info) => {
                    (RunEvidenceOutcome::Complete { disposition }, canonical_info)
                }
                Err(reason) => (
                    RunEvidenceOutcome::NoResult {
                        reason,
                        observed_disposition,
                    },
                    CanonicalInfoEvidence::no_result(),
                ),
            }
        } else {
            (
                RunEvidenceOutcome::NoResult {
                    reason: RunEvidenceNoResultReason::UnsupportedDisposition,
                    observed_disposition: None,
                },
                CanonicalInfoEvidence::no_result(),
            )
        };

        let report = RunEvidenceReport {
            schema_version: RUN_EVIDENCE_SCHEMA_VERSION,
            invocation_id: self.invocation_id,
            backend: report_backend(self.backend),
            attempt: 1,
            outcome,
            canonical_info,
        };
        self.publish_manifest(&report)
    }

    fn read_private_log(&self) -> Result<Vec<u8>, RunEvidenceNoResultReason> {
        let mut file = self
            .raw_log
            .try_clone()
            .map_err(|_| RunEvidenceNoResultReason::MissingCanonicalInfo)?;
        file.seek(SeekFrom::Start(0))
            .map_err(|_| RunEvidenceNoResultReason::MissingCanonicalInfo)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|_| RunEvidenceNoResultReason::MissingCanonicalInfo)?;
        Ok(bytes)
    }

    fn publish_canonical_info(&self) -> Result<CanonicalInfoEvidence, RunEvidenceNoResultReason> {
        let bytes = self.read_private_log()?;
        if bytes.is_empty() {
            return Err(RunEvidenceNoResultReason::ZeroCanonicalInfo);
        }
        if std::str::from_utf8(&bytes)
            .ok()
            .is_some_and(detcore::logdiff::log_was_truncated)
        {
            return Err(RunEvidenceNoResultReason::TruncatedCanonicalInfo);
        }
        let message_count = detcore::logdiff::write_bitwise_info_v1_bytes(
            &bytes,
            "private run-evidence log",
            &mut std::io::sink(),
        )
        .map_err(|_| RunEvidenceNoResultReason::MalformedCanonicalInfo)?
            as u64;
        if message_count == 0 {
            return Err(RunEvidenceNoResultReason::ZeroCanonicalInfo);
        }

        let digest = detcore::Digest::new(&bytes).to_string();
        let artifact_path = self.directory.join(RUN_EVIDENCE_INFO_ARTIFACT);
        let mut artifact = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&artifact_path)
            .map_err(|_| RunEvidenceNoResultReason::ArtifactWriteFailed)?;
        artifact
            .write_all(&bytes)
            .and_then(|()| artifact.flush())
            .and_then(|()| artifact.sync_all())
            .map_err(|_| RunEvidenceNoResultReason::ArtifactWriteFailed)?;
        drop(artifact);

        let published =
            fs::read(&artifact_path).map_err(|_| RunEvidenceNoResultReason::ArtifactWriteFailed)?;
        if published.len() != bytes.len() || detcore::Digest::new(&published).to_string() != digest
        {
            return Err(RunEvidenceNoResultReason::ArtifactWriteFailed);
        }

        Ok(CanonicalInfoEvidence {
            policy: CanonicalInfoPolicy::BitwiseInfoV1,
            record_envelope: RecordEnvelopeReport::AllRecordsV1,
            message_count,
            byte_count: bytes.len() as u64,
            sha256: Some(digest),
            artifact: RUN_EVIDENCE_INFO_ARTIFACT.to_string(),
        })
    }

    fn publish_manifest(&self, report: &RunEvidenceReport) -> Result<(), Error> {
        let destination = self.directory.join(RUN_EVIDENCE_MANIFEST);
        let mut staged = tempfile::Builder::new()
            .prefix(".run-evidence-manifest-")
            .rand_bytes(8)
            .tempfile_in(&self.directory)
            .with_context(|| {
                format!(
                    "creating a staged run-evidence manifest in {}",
                    self.directory.display()
                )
            })?;
        serde_json::to_writer(&mut staged, report)?;
        staged.write_all(b"\n")?;
        staged.flush()?;
        staged.as_file().sync_all()?;
        staged
            .persist_noclobber(&destination)
            .map_err(|error| error.error)
            .with_context(|| {
                format!(
                    "publishing terminal run-evidence manifest {} without replacement",
                    destination.display()
                )
            })?;
        Ok(())
    }
}

fn report_backend(backend: Backend) -> RunEvidenceBackend {
    match backend {
        Backend::Ptrace => RunEvidenceBackend::Ptrace,
        Backend::Dbt => RunEvidenceBackend::Dbt,
        Backend::Liteinst => RunEvidenceBackend::Liteinst,
        Backend::Sabre => RunEvidenceBackend::Sabre,
        Backend::Kvm => RunEvidenceBackend::Kvm,
        Backend::E9patch => RunEvidenceBackend::E9patch,
    }
}

fn guest_disposition(backend: Backend, status: ExitStatus) -> Option<GuestDisposition> {
    match (backend, status) {
        (Backend::Kvm, ExitStatus::Exited(code)) => Some(GuestDisposition::ExitCodeOnly {
            code,
            limitation: DispositionLimitation::KvmExitCodeOnly,
        }),
        (Backend::Kvm, ExitStatus::Signaled(_, _)) => None,
        (_, ExitStatus::Exited(code)) => Some(GuestDisposition::Exited { code }),
        (_, ExitStatus::Signaled(signal, core_dumped)) => Some(GuestDisposition::Signaled {
            signal: signal as i32,
            core_dumped,
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Barrier;
    use std::thread;

    use hermit::run_evidence::RunEvidenceInspection;
    use hermit::run_evidence::RunEvidenceInspectionFailure;
    use hermit::run_evidence::inspect_run_evidence;

    use super::*;

    const VALID_LOG: &[u8] = b"Apr 09 06:08:01.100  INFO hermit_test: evidence record\n";

    #[test]
    fn destination_creation_is_concurrent_and_no_clobber() {
        let parent = tempfile::tempdir().unwrap();
        let destination = Arc::new(parent.path().join("one-run"));
        let barrier = Arc::new(Barrier::new(3));
        let mut joins = Vec::new();
        for _ in 0..2 {
            let destination = Arc::clone(&destination);
            let barrier = Arc::clone(&barrier);
            joins.push(thread::spawn(move || {
                barrier.wait();
                RunEvidenceSession::create(&destination, Backend::Ptrace).is_ok()
            }));
        }
        barrier.wait();
        let winners = joins
            .into_iter()
            .map(|join| join.join().unwrap())
            .filter(|won| *won)
            .count();
        assert_eq!(winners, 1, "exactly one invocation may claim a path");
    }

    #[test]
    fn every_artifact_is_no_clobber_and_manifest_is_last() {
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("one-run");
        let session = RunEvidenceSession::create(&destination, Backend::Ptrace).unwrap();
        (&*session.raw_log).write_all(VALID_LOG).unwrap();
        assert!(!destination.join(RUN_EVIDENCE_MANIFEST).exists());
        fs::write(
            destination.join(RUN_EVIDENCE_INFO_ARTIFACT),
            b"do-not-replace",
        )
        .unwrap();

        let status = ExitStatus::Exited(0);
        session.finish(Ok(&status)).unwrap();
        assert_eq!(
            fs::read(destination.join(RUN_EVIDENCE_INFO_ARTIFACT)).unwrap(),
            b"do-not-replace"
        );
        assert_eq!(
            inspect_run_evidence(&destination),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::ReportedNoResult(
                RunEvidenceNoResultReason::ArtifactWriteFailed
            ))
        );
    }

    #[test]
    fn kvm_disposition_states_its_exit_code_only_limit() {
        assert_eq!(
            guest_disposition(Backend::Kvm, ExitStatus::Exited(23)),
            Some(GuestDisposition::ExitCodeOnly {
                code: 23,
                limitation: DispositionLimitation::KvmExitCodeOnly,
            })
        );

        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("unexpected-signal");
        let session = RunEvidenceSession::create(&destination, Backend::Kvm).unwrap();
        (&*session.raw_log).write_all(VALID_LOG).unwrap();
        let status = ExitStatus::Signaled(reverie::process::Signal::SIGTERM, false);
        session.finish(Ok(&status)).unwrap();
        assert_eq!(
            inspect_run_evidence(&destination),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::ReportedNoResult(
                RunEvidenceNoResultReason::UnsupportedDisposition
            ))
        );
    }
}
