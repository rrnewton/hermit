use std::ffi::CString;
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::RawFd;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
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

#[cfg(test)]
use super::tracing::LatchedWriter;
use super::tracing::WriteErrorLatch;

const MANIFEST_STAGING: &str = ".manifest.json.pending";

#[derive(Default)]
struct DirectorySync {
    #[cfg(test)]
    injected_failure: Option<(Arc<std::sync::atomic::AtomicUsize>, usize)>,
}

impl DirectorySync {
    fn sync(&self, directory: &File) -> io::Result<()> {
        #[cfg(test)]
        if let Some((calls, fail_on)) = &self.injected_failure {
            let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            if call == *fail_on {
                return Err(io::Error::other("injected directory fsync failure"));
            }
        }
        directory.sync_all()
    }

    #[cfg(test)]
    fn fail_on(call: usize) -> Self {
        Self {
            injected_failure: Some((Arc::new(std::sync::atomic::AtomicUsize::new(0)), call)),
        }
    }
}

pub(crate) struct RunEvidenceSession {
    directory: File,
    invocation_id: Uuid,
    backend: Backend,
    raw_log: Arc<File>,
    write_error: WriteErrorLatch,
    directory_sync: DirectorySync,
}

impl RunEvidenceSession {
    /// Atomically claim a destination that did not exist before this invocation.
    pub(crate) fn create(directory: &Path, backend: Backend) -> Result<Self, Error> {
        let invocation_id = Uuid::new_v4();
        let raw_log = tempfile::tempfile().with_context(|| {
            format!(
                "cannot create private run-evidence capture for {}",
                directory.display()
            )
        })?;
        let write_error = WriteErrorLatch::new().with_context(|| {
            format!(
                "cannot create process-shared run-evidence error latch for {}",
                directory.display()
            )
        })?;
        let claimed = create_claimed_directory(directory, invocation_id).with_context(|| {
            format!(
                "cannot create --run-evidence-dir {}: the destination must not already exist",
                directory.display()
            )
        })?;
        Ok(Self {
            directory: claimed,
            invocation_id,
            backend,
            raw_log: Arc::new(raw_log),
            write_error,
            directory_sync: DirectorySync::default(),
        })
    }

    pub(crate) fn log_handle(&self) -> Arc<File> {
        Arc::clone(&self.raw_log)
    }

    pub(crate) fn write_error_latch(&self) -> WriteErrorLatch {
        self.write_error.clone()
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
        } else if self.write_error.failed() {
            (
                RunEvidenceOutcome::NoResult {
                    reason: RunEvidenceNoResultReason::MissingCanonicalInfo,
                    observed_disposition,
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
        if self.write_error.failed() {
            return Err(RunEvidenceNoResultReason::MissingCanonicalInfo);
        }
        let mut file = self
            .raw_log
            .try_clone()
            .map_err(|_| RunEvidenceNoResultReason::MissingCanonicalInfo)?;
        file.seek(SeekFrom::Start(0))
            .map_err(|_| RunEvidenceNoResultReason::MissingCanonicalInfo)?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|_| RunEvidenceNoResultReason::MissingCanonicalInfo)?;
        if self.write_error.failed() {
            return Err(RunEvidenceNoResultReason::MissingCanonicalInfo);
        }
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
        let mut artifact = create_new_file_at(
            self.directory.as_raw_fd(),
            OsStr::new(RUN_EVIDENCE_INFO_ARTIFACT),
        )
        .map_err(|_| RunEvidenceNoResultReason::ArtifactWriteFailed)?;
        artifact
            .write_all(&bytes)
            .and_then(|()| artifact.flush())
            .and_then(|()| artifact.sync_all())
            .map_err(|_| RunEvidenceNoResultReason::ArtifactWriteFailed)?;

        artifact
            .seek(SeekFrom::Start(0))
            .map_err(|_| RunEvidenceNoResultReason::ArtifactWriteFailed)?;
        let mut published = Vec::new();
        artifact
            .read_to_end(&mut published)
            .map_err(|_| RunEvidenceNoResultReason::ArtifactWriteFailed)?;
        if published.len() != bytes.len() || detcore::Digest::new(&published).to_string() != digest
        {
            return Err(RunEvidenceNoResultReason::ArtifactWriteFailed);
        }
        self.directory_sync
            .sync(&self.directory)
            .map_err(|_| RunEvidenceNoResultReason::ArtifactWriteFailed)?;

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
        let mut contents = serde_json::to_vec(report)?;
        contents.push(b'\n');
        let directory_fd = self.directory.as_raw_fd();
        let staging = OsStr::new(MANIFEST_STAGING);
        let destination = OsStr::new(RUN_EVIDENCE_MANIFEST);
        let mut staged = create_new_file_at(directory_fd, staging)
            .context("creating a no-clobber staged run-evidence manifest")?;
        let mut published = false;
        let result = (|| -> io::Result<()> {
            staged.write_all(&contents)?;
            staged.flush()?;
            staged.sync_all()?;
            rename_noreplace_at(directory_fd, staging, destination)?;
            published = true;
            self.directory_sync.sync(&self.directory)?;
            Ok(())
        })();
        if let Err(error) = result {
            if published {
                // The open staging handle still names the published inode. Poison
                // it before unlinking so an observed directory-sync failure cannot
                // leave a locally readable complete verdict.
                let _ = staged.set_len(0);
                let _ = staged.sync_all();
                let _ = unlink_file_at(directory_fd, destination);
                let _ = self.directory.sync_all();
            } else {
                let _ = unlink_file_at(directory_fd, staging);
            }
            return Err(error).context("publishing terminal run-evidence manifest");
        }
        Ok(())
    }
}

fn path_cstring(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains NUL"))
}

fn component_cstring(component: &OsStr) -> io::Result<CString> {
    CString::new(component.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path component contains NUL"))
}

fn owned_file(fd: RawFd) -> io::Result<File> {
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: a nonnegative open/openat return is one newly owned fd.
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn open_directory(path: &Path) -> io::Result<File> {
    let path = path_cstring(path)?;
    owned_file(unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    })
}

fn open_directory_at(parent: RawFd, component: &OsStr) -> io::Result<File> {
    let component = component_cstring(component)?;
    owned_file(unsafe {
        libc::openat(
            parent,
            component.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    })
}

fn create_new_file_at(parent: RawFd, component: &OsStr) -> io::Result<File> {
    let component = component_cstring(component)?;
    owned_file(unsafe {
        libc::openat(
            parent,
            component.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    })
}

fn unlink_file_at(parent: RawFd, component: &OsStr) -> io::Result<()> {
    let component = component_cstring(component)?;
    if unsafe { libc::unlinkat(parent, component.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn remove_directory_at(parent: RawFd, component: &OsStr) {
    if let Ok(component) = component_cstring(component) {
        unsafe {
            libc::unlinkat(parent, component.as_ptr(), libc::AT_REMOVEDIR);
        }
    }
}

fn rename_noreplace_at(parent: RawFd, source: &OsStr, destination: &OsStr) -> io::Result<()> {
    let source = component_cstring(source)?;
    let destination = component_cstring(destination)?;
    if unsafe {
        libc::renameat2(
            parent,
            source.as_ptr(),
            parent,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Claim NEW_PATH by atomically renaming an already-open private directory.
///
/// The returned fd, not the pathname, is the authority for every later child
/// operation. Renaming NEW_PATH and installing a replacement therefore cannot
/// redirect either artifact or manifest publication.
fn create_claimed_directory(path: &Path, invocation_id: Uuid) -> io::Result<File> {
    let destination = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "destination has no basename")
    })?;
    let parent_path = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = open_directory(parent_path)?;
    let staging_name = format!(".run-evidence-claim-{invocation_id}");
    let staging = OsStr::new(&staging_name);
    let staging_c = component_cstring(staging)?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), staging_c.as_ptr(), 0o700) } != 0 {
        return Err(io::Error::last_os_error());
    }

    let claimed = match open_directory_at(parent.as_raw_fd(), staging) {
        Ok(claimed) => claimed,
        Err(error) => {
            remove_directory_at(parent.as_raw_fd(), staging);
            return Err(error);
        }
    };
    if let Err(error) = rename_noreplace_at(parent.as_raw_fd(), staging, destination) {
        remove_directory_at(parent.as_raw_fd(), staging);
        return Err(error);
    }
    Ok(claimed)
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
    use std::fs;
    use std::io::Write;
    use std::sync::Barrier;
    use std::thread;

    use hermit::run_evidence::RunEvidenceInspection;
    use hermit::run_evidence::RunEvidenceInspectionFailure;
    use hermit::run_evidence::inspect_run_evidence;

    use super::*;

    const VALID_LOG: &[u8] = b"Apr 09 06:08:01.100  INFO hermit_test: evidence record\n";

    struct FailAfterN<W> {
        inner: W,
        remaining: usize,
    }

    impl<W: Write> Write for FailAfterN<W> {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            if self.remaining == 0 {
                return Err(io::Error::other("injected capture write failure"));
            }
            let take = self.remaining.min(buf.len());
            let written = self.inner.write(&buf[..take])?;
            self.remaining -= written;
            Ok(written)
        }

        fn flush(&mut self) -> io::Result<()> {
            self.inner.flush()
        }
    }

    fn write_valid_log(session: &RunEvidenceSession) {
        (&*session.raw_log).write_all(VALID_LOG).unwrap();
    }

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
        write_valid_log(&session);
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
    fn valid_prefix_followed_by_capture_error_is_no_result() {
        let mut canonical = Vec::new();
        assert_eq!(
            detcore::logdiff::write_bitwise_info_v1_bytes(
                VALID_LOG,
                "valid prefix",
                &mut canonical,
            )
            .unwrap(),
            1
        );
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("write-error");
        let session = RunEvidenceSession::create(&destination, Backend::Ptrace).unwrap();
        let file = session.raw_log.try_clone().unwrap();
        let fail_after = FailAfterN {
            inner: file,
            remaining: VALID_LOG.len(),
        };
        let mut writer = LatchedWriter::new(fail_after, session.write_error.clone());
        writer.write_all(VALID_LOG).unwrap();
        assert!(writer.write_all(b"lost record").is_err());
        drop(writer);

        let status = ExitStatus::Exited(0);
        session.finish(Ok(&status)).unwrap();
        assert_eq!(
            inspect_run_evidence(&destination),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::ReportedNoResult(
                RunEvidenceNoResultReason::MissingCanonicalInfo
            ))
        );
        assert!(
            !destination.join(RUN_EVIDENCE_INFO_ARTIFACT).exists(),
            "a syntactically valid prefix must not be published as complete"
        );
    }

    #[test]
    fn renamed_destination_cannot_redirect_publication() {
        let parent = tempfile::tempdir().unwrap();
        let destination = parent.path().join("claimed");
        let original = parent.path().join("original-claimed-directory");
        let session = RunEvidenceSession::create(&destination, Backend::Ptrace).unwrap();
        write_valid_log(&session);

        fs::rename(&destination, &original).unwrap();
        fs::create_dir(&destination).unwrap();

        let status = ExitStatus::Exited(0);
        session.finish(Ok(&status)).unwrap();
        assert_eq!(
            fs::read_dir(&destination).unwrap().count(),
            0,
            "replacement path received evidence"
        );
        assert!(matches!(
            inspect_run_evidence(&original),
            RunEvidenceInspection::Complete(_)
        ));
    }

    #[test]
    fn directory_fsync_failures_never_leave_complete_evidence() {
        let parent = tempfile::tempdir().unwrap();

        let artifact_failure = parent.path().join("artifact-fsync");
        let mut session = RunEvidenceSession::create(&artifact_failure, Backend::Ptrace).unwrap();
        write_valid_log(&session);
        session.directory_sync = DirectorySync::fail_on(1);
        let status = ExitStatus::Exited(0);
        session.finish(Ok(&status)).unwrap();
        assert_eq!(
            inspect_run_evidence(&artifact_failure),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::ReportedNoResult(
                RunEvidenceNoResultReason::ArtifactWriteFailed
            ))
        );

        let manifest_failure = parent.path().join("manifest-fsync");
        let mut session = RunEvidenceSession::create(&manifest_failure, Backend::Ptrace).unwrap();
        write_valid_log(&session);
        session.directory_sync = DirectorySync::fail_on(2);
        assert!(session.finish(Ok(&status)).is_err());
        assert_eq!(
            inspect_run_evidence(&manifest_failure),
            RunEvidenceInspection::NoResult(RunEvidenceInspectionFailure::MissingManifest)
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
        write_valid_log(&session);
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
