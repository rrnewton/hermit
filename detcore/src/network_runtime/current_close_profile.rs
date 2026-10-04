//! A current physical safety check for an already selected logical Close.
//! The existing controller inbox owns command/target debt before every await.
//! A dropped future cannot turn an unobserved command into a clean fallback.
use std::io;
use std::os::fd::AsFd;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;

use super::ForegroundRoot;
use super::NetworkRuntimeResources;
use super::accepted_controller::Controller;
use super::accepted_controller::Effect;
use super::accepted_provider::Reply;
use super::accepted_provider::Request;
use super::accepted_provider::current_close_profile::Intent;
use super::accepted_provider::current_close_profile::{self as wire};
use super::original_installation::FileIdentity;
use crate::network_replay::NetworkFdReadAdmission;

#[derive(Clone)]
struct Selection {
    root: Arc<ForegroundRoot>,
    epoch: u64,
    read: NetworkFdReadAdmission,
    raw: [usize; 6],
    file: FileIdentity,
}
pub(crate) struct PreparedCurrentCloseProfile {
    selection: Selection,
    controller: Arc<Controller>,
    prepared: u64,
    intent: Intent,
    collect_started: AtomicBool,
    settled: AtomicBool,
}
pub(crate) struct CompletedCurrentCloseProfile {
    selection: Selection,
    // This immutable actual observation is retained; Debug deliberately does
    // not place host addresses or runtime cgroup identities in guest logs.
    _observed: super::accepted_provider::Observation<wire::Effect>,
}
impl std::fmt::Debug for PreparedCurrentCloseProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PreparedCurrentCloseProfile")
            .finish_non_exhaustive()
    }
}
impl std::fmt::Debug for CompletedCurrentCloseProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CompletedCurrentCloseProfile")
            .finish_non_exhaustive()
    }
}
impl PreparedCurrentCloseProfile {
    pub(crate) fn settled(&self) -> bool {
        self.settled.load(Ordering::Acquire)
    }
}
impl CompletedCurrentCloseProfile {
    pub(crate) fn validate_read(
        &self,
        root: &Arc<ForegroundRoot>,
        epoch: u64,
        read: &NetworkFdReadAdmission,
        raw: [usize; 6],
    ) -> io::Result<()> {
        self.selection.validate(root, epoch, read, raw)
    }
}
impl Selection {
    fn validate(
        &self,
        current: &Arc<ForegroundRoot>,
        current_epoch: u64,
        current_read: &NetworkFdReadAdmission,
        current_raw: [usize; 6],
    ) -> io::Result<()> {
        let Self {
            root: original,
            epoch,
            read,
            raw,
            file,
        } = self;
        let permit = read.publication.permit;
        let (provider, _, _, _) = current.native_identity();
        if !Arc::ptr_eq(original, current)
            || *epoch != current_epoch
            || read != current_read
            || *raw != current_raw
            || !current.is_current(permit.owner)
            || !current.has_shared_mm_history()
            || permit.files != current.files()
            || read.external_grant.is_some()
            || read.control.is_none()
            || read
                .binding
                .is_none_or(|b| b.slot.files != permit.files || b.slot.fd != read.fd)
            || read.fd < 0
            || raw[0] != read.fd as usize
            || file.provider_file().0 != provider
        {
            return Err(io::Error::other(
                "current Close changed original root/Normal/read/file/tuple",
            ));
        }
        Ok(())
    }
}
impl NetworkRuntimeResources {
    /// Global retains the real Normal/full-lineage and joined-prefix checks.
    /// This method independently joins physical task/file identities and arms
    /// exactly one command in the already existing controller request ledger.
    pub(crate) async fn prepare_current_close_profile(
        &self,
        root: Arc<ForegroundRoot>,
        epoch: u64,
        read: NetworkFdReadAdmission,
        raw: [usize; 6],
        file: FileIdentity,
    ) -> io::Result<PreparedCurrentCloseProfile> {
        if !self
            .shared
            .copy_wire
            .is_some_and(|w| w.has_current_close_profile())
        {
            return Err(io::Error::other(
                "current Close requires actual ABI12-copy5 provider",
            ));
        }
        let selection = Selection {
            root: root.clone(),
            epoch,
            read: read.clone(),
            raw,
            file,
        };
        selection.validate(&root, epoch, &read, raw)?;
        let permit = read.publication.permit;
        let (provider, task, start, table) = {
            let physical = self.shared.physical.lock().unwrap();
            if !Arc::ptr_eq(&physical.foreground_root(permit.owner)?, &root) {
                return Err(io::Error::other(
                    "current Close changed retained physical root",
                ));
            }
            physical.installation_identity(permit.owner)?
        };
        if root.native_identity() != (provider, task, start, table)
            || file.provider_file().0 != provider
        {
            return Err(io::Error::other("current Close changed native identity"));
        }
        let intent = Intent {
            command: 0,
            registration: root.association().view().registration,
            owner_mm: permit.owner.mm.generation(),
            normal_epoch: epoch,
            expected_table: table,
            expected_file: file.provider_file().1,
            fd: read.fd,
            reserved: 0,
            syscall_nr: 3,
        };
        if !intent.valid_unarmed() {
            return Err(io::Error::other("current Close unarmed intent invalid"));
        }
        let target = self.prepare_native_capture_task(permit.owner)?;
        let controller = self.accepted_controller()?;
        let prepared = controller.prepare(
            Effect::PrepareCurrentCloseProfile(permit.lease),
            permit.owner,
            &Request::PrepareCurrentCloseProfile {
                call: permit.native_command_call(),
                intent: intent.clone(),
            },
            || Ok(vec![target.as_fd().try_clone_to_owned()?]),
        )?;
        let armed = match controller.response(prepared).await? {
            Reply::Prepared(p)
                if p.status.operation == "ap_prepare_current_close_profile"
                    && p.status.returned == 0
                    && p.status.errno.is_none()
                    && p.raw != 0 =>
            {
                p
            }
            _ => {
                return Err(io::Error::other(
                    "current Close preparation failed; original request retained",
                ));
            }
        };
        selection.validate(&root, epoch, &read, raw)?;
        controller.claim_current_close_prepared(permit, prepared)?;
        let mut intent = intent;
        intent.command = armed.raw;
        Ok(PreparedCurrentCloseProfile {
            selection,
            controller,
            prepared,
            intent,
            collect_started: AtomicBool::new(false),
            settled: AtomicBool::new(false),
        })
    }
    pub(crate) async fn collect_current_close_profile(
        &self,
        prepared: &PreparedCurrentCloseProfile,
        register_read_succeeded: bool,
    ) -> io::Result<Arc<CompletedCurrentCloseProfile>> {
        let p = prepared;
        let s = &p.selection;
        let permit = s.read.publication.permit;
        if !Arc::ptr_eq(&self.accepted_controller()?, &p.controller)
            || !register_read_succeeded
            || p.collect_started.swap(true, Ordering::AcqRel)
        {
            return Err(io::Error::other(
                "current Close collection changed runtime/actual register read or was repeated",
            ));
        }
        let (provider, task, start, _) = s.root.native_identity();
        let collected = p.controller.prepare(
            Effect::CollectCurrentCloseProfile(permit.lease),
            permit.owner,
            &Request::CollectCurrentCloseProfile {
                call: permit.native_command_call(),
                command: p.intent.command,
                prepared_request: p.prepared,
            },
            || Ok(vec![]),
        )?;
        let observed = match p.controller.response(collected).await? {
            Reply::CurrentCloseProfile(observed) => observed,
            _ => {
                return Err(io::Error::other(
                    "current Close collection changed response kind; debt retained",
                ));
            }
        };
        wire::validate_collection(&observed, &p.intent, provider, task, start)?;
        // Both endpoints validate this exact full group before the actual C ACK.
        // A complete unsafe result still settles its command before refusal.
        let retired = p.controller.prepare(
            Effect::RetireCurrentCloseProfile(permit.lease),
            permit.owner,
            &Request::RetireCurrentCloseProfile {
                call: permit.native_command_call(),
                prepared: p.prepared,
                completed: collected,
            },
            || Ok(vec![]),
        )?;
        match p.controller.response(retired).await? {
            Reply::CurrentCloseProfileRetired(status)
                if status.operation == "ap_ack_command"
                    && status.returned == 0
                    && status.errno.is_none() => {}
            _ => {
                return Err(io::Error::other(
                    "current Close ACK failed; exact debt retained",
                ));
            }
        }
        p.controller.retire_current_close_profile(
            permit.owner,
            permit,
            [p.prepared, collected, retired],
        )?;
        p.settled.store(true, Ordering::Release);
        wire::validate_supported(&observed)?;
        s.validate(&s.root, s.epoch, &s.read, s.raw)?;
        Ok(Arc::new(CompletedCurrentCloseProfile {
            selection: s.clone(),
            _observed: observed,
        }))
    }
}
#[cfg(test)]
pub(crate) mod tests;
