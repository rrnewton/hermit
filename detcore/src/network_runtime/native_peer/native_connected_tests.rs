//! Controlled provider messages on actual retained Call/OwnedFd custody. These
//! helpers neither create a production proof nor qualify a native BPF provider.
use super::*;
use crate::network_runtime::NativeCaptureRecovery;
use crate::network_runtime::NetworkRuntimeResources;
use crate::network_runtime::accepted_provider::OriginalEffect;
use crate::network_runtime::accepted_provider::OriginalSelection;
use crate::network_runtime::accepted_provider_ffi as ffi;

#[derive(Clone, Copy, Debug)]
pub(crate) enum ConnectCompletionChange {
    None,
    Command,
    File,
    MissingSecurity,
    MissingCopy,
    SecurityError,
    Peer,
    Family,
}

impl NetworkRuntimeResources {
    pub(crate) fn controlled_connect_capture(
        &self,
        owner: NetworkStreamOwner,
        admission: &OriginalAdmission,
        pin: OwnedFd,
        publication: NativeCaptureRecovery,
    ) -> io::Result<OriginalPin> {
        let class = classify_original(&pin)?;
        let mut calls = self.shared.native_streams.lock().unwrap();
        calls.capture_original(
            owner,
            admission.clone(),
            Some(pin),
            tokio::runtime::Handle::current(),
            publication,
        )?;
        calls.original_classified(owner, admission.call, class)?;
        calls.original_prepared(owner, admission, 101, 103, 107)?;
        Ok(class)
    }

    pub(crate) fn controlled_connect_selection(
        &self,
        owner: NetworkStreamOwner,
        admission: &OriginalAdmission,
        identity: (u64, u64, u64, u64),
    ) -> io::Result<OriginalSelection> {
        let selected: OriginalSelection = ffi::OriginalSelection {
            command: 103,
            call: admission.call.native_command_call(),
            owner_mm: owner.mm.generation(),
            provider: identity.0,
            task: identity.1,
            task_start: identity.2,
            table: identity.3,
            file: 109,
            requested_fd: admission.arguments.fd,
            user_address: admission.arguments.address,
            address_length: admission.arguments.length,
            original_count: admission.arguments.original_count,
            ready: 1,
            ..Default::default()
        }
        .into();
        self.shared
            .native_streams
            .lock()
            .unwrap()
            .original_selected(owner, admission, selected.clone())?;
        Ok(selected)
    }

    pub(crate) fn controlled_connect_complete(
        &self,
        owner: NetworkStreamOwner,
        admission: &OriginalAdmission,
        address: &[u8],
        raw_return: i64,
        change: ConnectCompletionChange,
    ) -> io::Result<()> {
        let mut calls = self.shared.native_streams.lock().unwrap();
        let selection = calls
            .original(owner, admission.call)?
            .selection
            .clone()
            .ok_or_else(|| {
                io::Error::other("fixture completion requires actual retained selection")
            })?;
        let mut effect = OriginalEffect {
            socket: None,
            read_copy: None,
            send: None,
            blocking_send: None,
            command: ffi::CommandResult {
                command: selection.command,
                operation: 7,
                phase: 1,
                returned: raw_return as i32,
                task: selection.task,
                start_boottime: selection.task_start,
                identity: ffi::Identity {
                    provider: selection.provider,
                    ..Default::default()
                },
                ..Default::default()
            }
            .into(),
            original: ffi::OriginalResult {
                returned: raw_return as i32,
                complete: 1,
                copy_entered: 1,
                copy_returned: 1,
                security_entered: 1,
                security_returned: 1,
                ..Default::default()
            }
            .into(),
        };
        effect.original.selection = selection;
        assert!(address.len() <= effect.original.address.len());
        effect.original.address[..address.len()].copy_from_slice(address);
        match change {
            ConnectCompletionChange::None => {}
            ConnectCompletionChange::Command => effect.command.command += 1,
            ConnectCompletionChange::File => effect.original.selection.file += 1,
            ConnectCompletionChange::MissingSecurity => effect.original.security_returned = 0,
            ConnectCompletionChange::MissingCopy => effect.original.copy_returned = 0,
            ConnectCompletionChange::SecurityError => {
                effect.original.security_result = -libc::EPERM
            }
            ConnectCompletionChange::Peer => effect.original.address[3] ^= 1,
            ConnectCompletionChange::Family => {
                effect.original.address[..2].copy_from_slice(&(libc::AF_UNIX as u16).to_ne_bytes())
            }
        }
        calls.original_completed(owner, admission, effect, raw_return)
    }

    /// Controlled provider ACK input, after the engine's separately observed
    /// backend return. This deliberately is not a native controller receipt.
    pub(crate) fn controlled_connect_retirement(
        &self,
        owner: NetworkStreamOwner,
        admission: &OriginalAdmission,
    ) -> io::Result<()> {
        self.controlled_connect_retirement_with_return(owner, admission, 0)
    }

    /// Same controlled ACK input for the separately observed nonzero return.
    /// The old success-only fixture continues to supply exactly zero above.
    pub(crate) fn controlled_connect_retirement_with_return(
        &self,
        owner: NetworkStreamOwner,
        admission: &OriginalAdmission,
        returned: i64,
    ) -> io::Result<()> {
        let publication = {
            let mut calls = self.shared.native_streams.lock().unwrap();
            let original = calls.original(owner, admission.call)?;
            if original.completion.is_none() || original.retired {
                return Err(io::Error::other(
                    "fixture retirement lacks retained completion",
                ));
            }
            original.publication.clone()
        };
        publication
            .engine
            .lock()
            .unwrap()
            .original_connect_provider_retired(owner, admission, returned)
            .map_err(io::Error::other)?;
        self.shared
            .native_streams
            .lock()
            .unwrap()
            .original(owner, admission.call)?
            .retired = true;
        Ok(())
    }

    /// The real release executor moves and closes the actual retained alias.
    /// No Call is forgotten here: publication must borrow it before final ACK.
    pub(crate) fn controlled_connect_close(
        &self,
        owner: NetworkStreamOwner,
        admission: &OriginalAdmission,
    ) -> io::Result<()> {
        let (work, publication, raw) = {
            let mut calls = self.shared.native_streams.lock().unwrap();
            let raw = calls
                .calls
                .get(&admission.call)
                .unwrap()
                .original
                .as_ref()
                .unwrap()
                .as_raw_fd();
            let original = calls.original(owner, admission.call)?;
            let publication = original.publication.clone();
            original.close_queued = true;
            (
                calls.prepare_release(owner, admission.call)?,
                publication,
                raw,
            )
        };
        let release = work.perform();
        assert_eq!(release.original, None);
        assert_eq!(unsafe { libc::fcntl(raw, libc::F_GETFD) }, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
        self.shared.native_streams.lock().unwrap().retain_release(
            owner,
            admission.call,
            release,
        )?;
        publication
            .engine
            .lock()
            .unwrap()
            .original_connect_pin_released(owner, admission)
            .map_err(io::Error::other)
    }

    pub(crate) fn controlled_connect_diagnostic(
        &self,
        owner: NetworkStreamOwner,
        admission: &OriginalAdmission,
    ) -> Vec<u8> {
        let mut calls = self.shared.native_streams.lock().unwrap();
        let original = calls.original(owner, admission.call).unwrap();
        serde_json::to_vec(&(
            original.admission.clone(),
            original.selection.clone(),
            original.completion.clone(),
            original.retired,
            original.close_queued,
        ))
        .unwrap()
    }

    pub(crate) fn controlled_connect_custody(&self) -> String {
        format!("{:?}", self.shared.native_streams.lock().unwrap())
    }

    /// Exercise the actual post-close forget transition, to demonstrate that
    /// byte-identical serialized diagnostics cannot replace the missing borrow.
    pub(crate) fn controlled_connect_forget_closed(
        &self,
        owner: NetworkStreamOwner,
        admission: &OriginalAdmission,
    ) -> io::Result<()> {
        self.shared
            .native_streams
            .lock()
            .unwrap()
            .finish_release(owner, admission.call)
    }
}
