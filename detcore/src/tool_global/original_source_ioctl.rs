//! Read-only join of one original backend ENTRY to the retained initial OFD.
use reverie::OriginalIoctlEffect;
use reverie::OriginalIoctlEntry;
use reverie::syscalls::Syscall;
use reverie::syscalls::Sysno;

use super::GlobalState;
use crate::network_replay::NetworkStreamOwner;
use crate::types::DetPid;
use crate::types::DetTid;

impl GlobalState {
    pub(crate) fn classify_original_source_ioctl(
        &self,
        process: DetPid,
        entry: &OriginalIoctlEntry,
    ) -> Option<OriginalIoctlEffect> {
        if !self.cfg.sequentialize_threads {
            return None;
        }
        let thread = DetTid::from_raw(entry.tid().as_raw());
        let mm = *self.registered_exec_mms.lock().unwrap().get(&thread)?;
        let owner = NetworkStreamOwner { thread, mm };
        let root = self.network_runtime.as_ref()?.foreground_root(owner).ok()?;
        let actual = root.metadata().ok()?;
        let sched = self.sched.lock().unwrap();
        if root.thread() != entry.tid().as_raw()
            || sched.registered_process(thread) != Some(process)
            || self.registered_exec_mms.lock().unwrap().get(&thread) != Some(&mm)
        {
            return None;
        }
        let grant = sched.foreground_native_observation(owner, &root).ok()?;
        let metadata = actual.lock().unwrap();
        let engine = self.network_engine.as_ref()?.lock().unwrap();
        if !engine.native_receive_version()
            || !root.matches_metadata(&actual)
            || !crate::memory::preserves_foreground_terminal_query(
                Sysno::ioctl,
                entry.args(),
                &root,
                &metadata,
            )
        {
            return None;
        }
        engine
            .validate_fd_metadata(owner, root.files(), &actual, &metadata)
            .ok()?;
        let Syscall::Ioctl(call) = Syscall::from_raw(Sysno::ioctl, entry.args()) else {
            return None;
        };
        let binding = metadata.descriptor_binding(call.fd()).ok()?;
        let identity = metadata.file_handles.get(&call.fd())?.native_file()?;
        if metadata.native_binding_identity(binding) != Some(identity)
            || !grant.admits_sole_initial_root(&root)
        {
            return None;
        }
        let dispatch = root.association().source_ioctl_dispatch(identity)?;
        // TODO-HUMAN-REVIEW(PR-3464): https://github.com/rrnewton/hermit/pull/3464
        // SAFETY: this actual stopped ENTRY is joined to the registered task/MM,
        // sole initial root, borrowed Normal grant and current metadata/FD
        // generation. Its current native OFD matches the authenticated census
        // provider/file, whose exact immutable f_op table and supported profile
        // were observed on pinned Linux 295ad0959f344afbc813d7962007d36db1dd8e8b.
        // Authentication covers those tables/handlers. Security-hook non-use
        // relies on the existing trusted-kernel/privileged-hook conformance
        // premise: security/security.c:2503-2506 at that revision says a user
        // pointer arg "should never be used by the security module". This does
        // not authenticate LSM program bytes or prove hostile privileged hooks
        // absent. All guards remain held here. This issues neither
        // completion nor source access; the backend still executes and joins
        // the original ENOTTY return, and never resets a revoked history.
        unsafe { entry.certify_dispatch(dispatch) }
    }
}

#[cfg(test)]
mod tests;
