//! Real initial-root authority for optional original Socket provenance.
use std::os::fd::OwnedFd;

use super::*;
use crate::network_replay::original_connect::Admission;
use crate::network_replay::original_connect::Kind;
use crate::network_runtime::socket_birth_policy::SocketBirthAuthority;

impl GlobalState {
    pub(super) fn original_socket_birth_authority(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        _target: &OwnedFd,
    ) -> Result<Option<SocketBirthAuthority>, NetworkRpcError> {
        let a = &admission.arguments;
        if !self.cfg.sequentialize_threads
            || a.kind != Kind::Socket
            || a.fd != libc::AF_INET
            || a.address as u32 as i32 & !(libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC)
                != libc::SOCK_STREAM
            || !matches!(a.length, 0 | libc::IPPROTO_TCP)
        {
            return Ok(None);
        }
        let Some(runtime) = &self.network_runtime else {
            return Ok(None);
        };
        let Ok(root) = runtime.foreground_root(owner) else {
            return Ok(None);
        };
        if !root.is_sole_initial_root(owner) {
            return Ok(None);
        }
        let actual = root
            .metadata()
            .map_err(|e| NetworkRpcError::internal(e.to_string()))?;
        let sched = self.sched.lock().unwrap();
        if self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm) {
            return Err(NetworkRpcError::internal(
                "Socket birth lost registered original MM",
            ));
        }
        let grant = sched
            .foreground_native_observation(owner, &root)
            .map_err(|e| NetworkRpcError::internal(e.to_string()))?;
        let metadata = actual.lock().unwrap();
        let engine = self
            .network_engine
            .as_ref()
            .ok_or_else(|| NetworkRpcError::internal("Socket birth engine absent"))?
            .lock()
            .unwrap();
        if !engine.uses_shared_mm_attempts() {
            return Ok(None);
        }
        engine
            .validate_fd_metadata(owner, a.files, &actual, &metadata)
            .map_err(|e| NetworkRpcError::internal(e.to_string()))?;
        if engine
            .original_connect_cancellation(owner, admission)
            .map_err(|e| NetworkRpcError::internal(e.to_string()))?
            .0
        {
            return Err(NetworkRpcError::internal(
                "Socket birth was canceled before capture",
            ));
        }
        SocketBirthAuthority::from_original(root.clone(), &grant, admission)
            .map(Some)
            .map_err(|e| NetworkRpcError::internal(e.to_string()))
    }

    /// Reborrow current Normal after asynchronous preparation and at actual
    /// Prepared. The callback does not define the expected owner/epoch/tuple.
    pub(super) fn check_original_socket_birth(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> Result<(), NetworkRpcError> {
        let Some(runtime) = &self.network_runtime else {
            return Ok(());
        };
        let Some(authority) = runtime
            .original_socket_birth_authority(owner, admission)
            .map_err(|e| NetworkRpcError::internal(e.to_string()))?
        else {
            return Ok(());
        };
        let sched = self.sched.lock().unwrap();
        if self.registered_exec_mms.lock().unwrap().get(&owner.thread) != Some(&owner.mm) {
            return Err(NetworkRpcError::internal(
                "Socket birth changed registered MM",
            ));
        }
        let grant = sched
            .foreground_native_observation(owner, authority.root())
            .map_err(|e| NetworkRpcError::internal(e.to_string()))?;
        authority
            .validate(&grant, admission)
            .map_err(|e| NetworkRpcError::internal(e.to_string()))
    }
}
