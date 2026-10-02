//! Actual local metadata ownership for the existing FD publication authority.
//! Association is neither task/table enrollment nor a native selected-file fact.
use std::sync::Arc;
use std::sync::Mutex;

use super::FilesId;
use super::NetworkReplayEngine;
use super::NetworkReplayError;
use super::NetworkStreamOwner;
use crate::tool_local::FileMetadata;

fn protocol(message: &str) -> NetworkReplayError {
    NetworkReplayError::FdPublicationProtocol(message.into())
}
impl NetworkReplayEngine {
    /// Validate the exact token being delivered by its original grant response.
    /// This does not select a native file or permit a second consuming transfer.
    pub(crate) fn validate_fd_read_grant(
        &self,
        owner: NetworkStreamOwner,
        read: &super::NetworkFdReadAdmission,
    ) -> Result<(), NetworkReplayError> {
        self.validate_fd_read(owner, read)
    }

    /// Capture only private native identity while the exact read grant and
    /// actual metadata Arc still protect this generation. The subsequent
    /// begin transfers the same permit into Call custody without a gap.
    pub(crate) fn native_stream_capture_identity(
        &self,
        owner: NetworkStreamOwner,
        read: &super::NetworkFdReadAdmission,
        actual: &Arc<Mutex<FileMetadata>>,
        observed: &FileMetadata,
    ) -> Result<
        Option<crate::network_runtime::original_installation::FileIdentity>,
        NetworkReplayError,
    > {
        self.validate_fd_metadata(owner, read.publication.permit.files, actual, observed)?;
        self.validate_fd_read(owner, read)?;
        if self.mode() == super::NetworkEngineMode::Replay {
            return Ok(None);
        }
        let binding = read
            .binding
            .ok_or_else(|| protocol("native capture lacks admitted binding"))?;
        observed
            .native_binding_identity(binding)
            .map(Some)
            .ok_or_else(|| protocol("native capture lacks authenticated held-file identity"))
    }

    /// Attach the scheduler's actual external grant to the existing reader.
    /// This changes no physical state and cannot certify syscall entry.
    pub(crate) fn bind_fd_read_external_grant(
        &mut self,
        owner: NetworkStreamOwner,
        mut read: super::NetworkFdReadAdmission,
        operation: crate::resources::ExternalOpId,
    ) -> Result<super::NetworkFdReadAdmission, NetworkReplayError> {
        self.validate_fd_read(owner, &read)?;
        if read.external_grant.is_some() {
            return Err(protocol("reader already belongs to a scheduling grant"));
        }
        read.external_grant = Some(operation);
        self.fd_publications
            .get_mut(&read.publication.permit.files)
            .expect("validated reader table")
            .reader = Some(read.clone());
        Ok(read)
    }

    /// Identify only a retained external selection owner. The scheduler must
    /// still match this operation to its actual blocked-external population.
    /// A notification or a merely prepared command is never that proof.
    pub(crate) fn fd_read_pending_external_selection(
        &self,
        reader: NetworkStreamOwner,
        files: FilesId,
        fd: i32,
    ) -> Option<(NetworkStreamOwner, crate::resources::ExternalOpId)> {
        let state = self.fd_publications.get(&files)?;
        if let Some(active) = state.active {
            if let Some(read) = &state.reader {
                return (read.publication.permit == active)
                    .then_some((active.owner, read.external_grant?));
            }
            return self.original_external_selection_owner(Some(active), None);
        }
        let task = self.publication_owner(reader, files).ok()?;
        let binding = self.lifetime.descriptor_binding(task, fd).ok()?;
        let control = self.socket_controls.get(&binding.open_file)?;
        self.original_external_selection_owner(None, Some(control.lease))
    }

    /// Bind census file authority only at the first real metadata association.
    /// Later clone/exec callbacks retain their actual shared descriptions and
    /// must not replay an old initial census over changed descriptor slots.
    pub(crate) fn associate_authenticated_fd_metadata(
        &mut self,
        owner: NetworkStreamOwner,
        actual: &Arc<Mutex<FileMetadata>>,
        observed: &mut FileMetadata,
        initial: Option<&crate::network_runtime::InitialMetadataIdentity>,
    ) -> Result<(), NetworkReplayError> {
        self.publication_owner(owner, observed.files_id)?;
        let weak = Arc::downgrade(actual);
        let prior = self
            .fd_publications
            .get(&observed.files_id)
            .and_then(|state| state.metadata.as_ref());
        if prior.is_some_and(|prior| !prior.ptr_eq(&weak)) {
            return Err(protocol("live FilesId changed its actual metadata object"));
        }
        if prior.is_none()
            && let Some(initial) = initial
        {
            initial
                .bind(owner, observed)
                .map_err(|error| protocol(&error.to_string()))?;
        }
        self.associate_fd_metadata(owner, actual, observed)
    }

    /// Associate the actual Arc; caller holds metadata before engine.
    pub(crate) fn associate_fd_metadata(
        &mut self,
        owner: NetworkStreamOwner,
        actual: &Arc<Mutex<FileMetadata>>,
        observed: &FileMetadata,
    ) -> Result<(), NetworkReplayError> {
        if !self.fd_table_capability() {
            return Err(protocol("metadata association cannot grant FD capability"));
        }
        let files = observed.files_id;
        let task = self.publication_owner(owner, files)?;
        let weak = Arc::downgrade(actual);
        let state = self.fd_publications.entry(files).or_default();
        if state
            .metadata
            .as_ref()
            .is_some_and(|prior| !prior.ptr_eq(&weak))
        {
            return Err(protocol("live FilesId changed its actual metadata object"));
        }
        self.lifetime
            .mark_task_metadata_ready(task)
            .map_err(|error| protocol(&error.to_string()))?;
        state.metadata = Some(weak);
        Ok(())
    }
    /// Look up custody only; revalidate after dropping engine and taking metadata.
    pub(crate) fn fd_metadata(
        &self,
        owner: NetworkStreamOwner,
        files: FilesId,
    ) -> Result<Arc<Mutex<FileMetadata>>, NetworkReplayError> {
        let task = self.publication_owner(owner, files)?;
        if !self
            .lifetime
            .task_metadata_ready(task)
            .map_err(|error| protocol(&error.to_string()))?
        {
            return Err(protocol(
                "reader task has no successful local state-ready observation",
            ));
        }
        self.fd_publications
            .get(&files)
            .and_then(|state| state.metadata.as_ref())
            .and_then(std::sync::Weak::upgrade)
            .ok_or_else(|| protocol("reader lacks a live backend-owned metadata association"))
    }
    /// Recheck after metadata acquisition. Weak upgrade is not a permit and
    /// cannot exclude exec or owner loss while the engine lock was dropped.
    pub(crate) fn validate_fd_metadata(
        &self,
        owner: NetworkStreamOwner,
        files: FilesId,
        actual: &Arc<Mutex<FileMetadata>>,
        observed: &FileMetadata,
    ) -> Result<(), NetworkReplayError> {
        let task = self.publication_owner(owner, files)?;
        if !self
            .lifetime
            .task_metadata_ready(task)
            .map_err(|error| protocol(&error.to_string()))?
        {
            return Err(protocol(
                "reader task has no successful local state-ready observation",
            ));
        }
        if observed.files_id != files
            || self
                .fd_publications
                .get(&files)
                .and_then(|state| state.metadata.as_ref())
                .is_none_or(|bound| !bound.ptr_eq(&Arc::downgrade(actual)))
        {
            return Err(protocol(
                "reader changed actual metadata owner/table association",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use chrono::Utc;

    use super::*;
    use crate::types::DetTid;
    use crate::types::MmId;
    fn setup() -> (
        NetworkReplayEngine,
        NetworkStreamOwner,
        FilesId,
        Arc<Mutex<FileMetadata>>,
    ) {
        let thread = DetTid::from_raw(91);
        let owner = NetworkStreamOwner {
            thread,
            mm: MmId::initial(thread),
        };
        let mut engine = NetworkReplayEngine::record(Utc.timestamp_opt(1_790_000_000, 0).unwrap());
        // Explicit component authority, not native lifecycle evidence.
        engine.fd_table_fixture_enable();
        let files = engine.fd_publication_fixture_register(owner, None);
        let actual = Arc::new(Mutex::new(FileMetadata::empty_network_fixture(thread)));
        (engine, owner, files, actual)
    }
    #[test]
    fn native_capture_identity_is_private_and_bound_before_reader_transfer() {
        let (mut engine, owner, files, actual) = setup();
        engine
            .associate_fd_metadata(owner, &actual, &actual.lock().unwrap())
            .unwrap();
        let (mut candidate, replacement) = actual
            .lock()
            .unwrap()
            .prepare_original_installation(owner.thread, 7, nix::fcntl::OFlag::O_RDWR, None)
            .unwrap();
        let binding = replacement.after.unwrap().binding;
        let publication = engine.acquire_fd_publication(owner, files).unwrap();
        let effect = engine.fd_publication_fixture_effect(owner, replacement);
        candidate
            .associate_network_installation(replacement.installation_generation, effect)
            .unwrap();
        let batch = candidate.publication_snapshot(&publication).unwrap();
        *actual.lock().unwrap() = candidate;
        engine
            .publish_fd_publication(owner, publication.permit, &batch)
            .unwrap();
        actual
            .lock()
            .unwrap()
            .publication_acknowledge(&batch)
            .unwrap();
        engine
            .acknowledge_fd_publication(owner, publication.permit, &batch)
            .unwrap();
        actual
            .lock()
            .unwrap()
            .publication_server_acknowledge(&batch)
            .unwrap();
        let super::super::NetworkFdReadBegin::Admitted(read) =
            engine.begin_fd_read(owner, files, 7).unwrap()
        else {
            panic!("fully published fixture must admit its exact reader");
        };
        let read = *read;
        let before = format!("{engine:?}");
        assert!(
            engine
                .native_stream_capture_identity(owner, &read, &actual, &actual.lock().unwrap())
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        // Explicit private component authority, never an identity deserializer.
        let identity =
            crate::network_runtime::original_installation::FileIdentity::controlled_fixture(3, 7);
        actual
            .lock()
            .unwrap()
            .bind_native_installation(binding, identity)
            .unwrap();
        assert_eq!(
            engine
                .native_stream_capture_identity(owner, &read, &actual, &actual.lock().unwrap())
                .unwrap(),
            Some(identity)
        );
        let other = Arc::new(Mutex::new(FileMetadata::empty_network_fixture(
            owner.thread,
        )));
        assert!(
            engine
                .native_stream_capture_identity(owner, &read, &other, &other.lock().unwrap())
                .is_err()
        );
        let stale = NetworkStreamOwner {
            mm: owner.mm.for_exec(owner.thread),
            ..owner
        };
        assert!(
            engine
                .native_stream_capture_identity(stale, &read, &actual, &actual.lock().unwrap())
                .is_err()
        );
        let mut changed = read.clone();
        changed.binding.as_mut().unwrap().generation += 1;
        assert!(
            engine
                .native_stream_capture_identity(owner, &changed, &actual, &actual.lock().unwrap())
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        let call = engine
            .begin_native_stream_call_from_read(owner, read.clone())
            .unwrap();
        assert!(call.physical_pin_required);
        assert!(
            engine
                .native_stream_capture_identity(owner, &read, &actual, &actual.lock().unwrap())
                .is_err()
        );
        assert!(
            engine.finish_fd_read(owner, read).is_err(),
            "the same permit now belongs to the Call"
        );
    }
    #[test]
    fn metadata_association_preserves_strong_ownership_and_exact_shared_table_object() {
        let (mut engine, owner, files, actual) = setup();
        let peer = NetworkStreamOwner {
            thread: DetTid::from_raw(92),
            ..owner
        };
        assert_eq!(
            engine.fd_publication_fixture_register(peer, Some(owner)),
            files
        );
        let count = Arc::strong_count(&actual);
        engine
            .associate_fd_metadata(owner, &actual, &actual.lock().unwrap())
            .unwrap();
        engine
            .associate_fd_metadata(peer, &actual, &actual.lock().unwrap())
            .unwrap();
        assert_eq!(Arc::strong_count(&actual), count);
        let retained = engine.fd_metadata(peer, files).unwrap();
        assert!(Arc::ptr_eq(&retained, &actual));
        engine
            .validate_fd_metadata(peer, files, &retained, &retained.lock().unwrap())
            .unwrap();
        drop(retained);
        assert_eq!(Arc::strong_count(&actual), count);
        let wrong = Arc::new(Mutex::new(FileMetadata::empty_network_fixture(
            owner.thread,
        )));
        let before = format!("{engine:?}");
        assert!(
            engine
                .associate_fd_metadata(peer, &wrong, &wrong.lock().unwrap())
                .is_err()
        );
        assert!(
            engine
                .validate_fd_metadata(peer, files, &wrong, &wrong.lock().unwrap())
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
    }
    #[test]
    fn cancelled_shared_child_cannot_inherit_positively_ready_parent_metadata() {
        let (mut engine, parent, files, actual) = setup();
        engine
            .associate_fd_metadata(parent, &actual, &actual.lock().unwrap())
            .unwrap();
        assert!(Arc::ptr_eq(
            &engine.fd_metadata(parent, files).unwrap(),
            &actual
        ));
        let child = NetworkStreamOwner {
            thread: DetTid::from_raw(95),
            ..parent
        };
        assert_eq!(
            engine.fd_publication_fixture_register(child, Some(parent)),
            files
        );
        // Component cancellation frontier: a child whose start was cancelled
        // delivered no successful callback. Its enrolled shared table is real
        // component input; this does not claim a native cancelled-handler run.
        let before = format!("{engine:?}");
        assert!(engine.fd_metadata(child, files).is_err());
        assert!(
            engine
                .validate_fd_metadata(child, files, &actual, &actual.lock().unwrap())
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        assert!(engine.fd_metadata(parent, files).is_ok());
        engine.retire_fd_table_owner(child);
        assert!(engine.fd_metadata(child, files).is_err());
        assert!(engine.fd_metadata(parent, files).is_ok());
        engine.retire_fd_table_owner(parent);
        engine.finish_fd_mutations().unwrap();
    }

    #[test]
    fn metadata_lookup_requires_revalidation_after_table_change_and_owner_loss() {
        let (mut engine, owner, files, actual) = setup();
        engine
            .associate_fd_metadata(owner, &actual, &actual.lock().unwrap())
            .unwrap();
        let retained = engine.fd_metadata(owner, files).unwrap();
        {
            let mut metadata = actual.lock().unwrap();
            metadata.files_id = FilesId::initial(DetTid::from_raw(93));
            assert!(
                engine
                    .validate_fd_metadata(owner, files, &retained, &metadata)
                    .is_err()
            );
            metadata.files_id = files;
        }
        let stale = NetworkStreamOwner {
            mm: owner.mm.for_exec(owner.thread),
            ..owner
        };
        assert!(engine.fd_metadata(stale, files).is_err());
        engine
            .validate_fd_metadata(owner, files, &actual, &actual.lock().unwrap())
            .unwrap();
        engine.retire_fd_table_owner(owner);
        assert!(
            engine
                .validate_fd_metadata(owner, files, &retained, &retained.lock().unwrap())
                .is_err()
        );
        assert!(!engine.fd_publications.contains_key(&files));
        engine.finish_fd_mutations().unwrap();
    }
    #[test]
    fn expired_metadata_never_becomes_current_slot_authority() {
        let (mut engine, owner, files, actual) = setup();
        engine
            .associate_fd_metadata(owner, &actual, &actual.lock().unwrap())
            .unwrap();
        drop(actual);
        assert!(engine.fd_metadata(owner, files).is_err());
        let replacement = Arc::new(Mutex::new(FileMetadata::empty_network_fixture(
            owner.thread,
        )));
        assert!(
            engine
                .associate_fd_metadata(owner, &replacement, &replacement.lock().unwrap())
                .is_err()
        );
        assert!(engine.fd_publications[&files].active.is_none());
        assert!(engine.fd_publications[&files].reader.is_none());
        engine.retire_fd_table_owner(owner);
        assert!(!engine.fd_publications.contains_key(&files));
    }
    #[test]
    fn metadata_object_cannot_create_capability_or_task_table_enrollment() {
        let (mut engine, owner, files, actual) = setup();
        let unknown = NetworkStreamOwner {
            thread: DetTid::from_raw(94),
            ..owner
        };
        let before = format!("{engine:?}");
        assert!(
            engine
                .associate_fd_metadata(unknown, &actual, &actual.lock().unwrap())
                .is_err()
        );
        assert_eq!(format!("{engine:?}"), before);
        let mut dormant = NetworkReplayEngine::record(Utc.timestamp_opt(1_790_000_000, 0).unwrap());
        let before = format!("{dormant:?}");
        assert!(
            dormant
                .associate_fd_metadata(owner, &actual, &actual.lock().unwrap())
                .is_err()
        );
        assert_eq!(format!("{dormant:?}"), before);
        assert!(!dormant.fd_table_capability());
        assert!(dormant.fd_metadata(owner, files).is_err());
    }
}
