//! Logical provenance of an actual original IPv4 TCP Socket. This does not
//! certify current socket options or finite Close. Close requires its own fresh
//! kernel release profile under the retained original Normal/read grant.
use std::io;
use std::sync::Arc;

use super::ForegroundRoot;
use super::original_installation::Installation;
use super::original_installation::Source;
use crate::network_replay::NetworkStreamOwner;
use crate::network_replay::original_connect::Admission;
use crate::scheduler::ordinary_fd::OrdinaryFdObservation;

/// Minted by the real sole-initial Normal borrower, not by a socket argument.
/// This owns no new descriptor and grants no policy-absence fact by itself.
#[derive(Clone)]
pub(crate) struct SocketBirthAuthority {
    root: Arc<ForegroundRoot>,
    owner: NetworkStreamOwner,
    epoch: u64,
    admission: Admission,
}
impl std::fmt::Debug for SocketBirthAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SocketBirthAuthority(private)")
    }
}
impl SocketBirthAuthority {
    pub(crate) fn from_original(
        root: Arc<ForegroundRoot>,
        grant: &OrdinaryFdObservation<'_>,
        admission: &Admission,
    ) -> io::Result<Self> {
        let a = &admission.arguments;
        if !grant.admits_sole_initial_root(&root)
            || a.kind != crate::network_replay::original_connect::Kind::Socket
            || a.fd != libc::AF_INET
            || a.address as u32 as i32 & !(libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC)
                != libc::SOCK_STREAM
            || !matches!(a.length, 0 | libc::IPPROTO_TCP)
            || a.files != root.files()
        {
            return Err(io::Error::other(
                "Socket birth lacks original sole Normal authority",
            ));
        }
        Ok(Self {
            owner: grant.owner(),
            epoch: grant.epoch(),
            root,
            admission: admission.clone(),
        })
    }
    pub(crate) fn validate(
        &self,
        grant: &OrdinaryFdObservation<'_>,
        admission: &Admission,
    ) -> io::Result<()> {
        if grant.owner() != self.owner
            || grant.epoch() != self.epoch
            || !grant.admits_sole_initial_root(&self.root)
            || admission != &self.admission
        {
            return Err(io::Error::other(
                "Socket birth changed original Normal/call",
            ));
        }
        Ok(())
    }
    pub(crate) fn root(&self) -> &Arc<ForegroundRoot> {
        &self.root
    }
}

/// Retains only the existing original Socket authority. No host query, file
/// acquisition, or guest operation occurs while constructing this intent.
#[derive(Clone, Debug)]
pub(super) struct Plan {
    pub(super) authority: SocketBirthAuthority,
}
impl Plan {
    pub(super) fn capture(authority: SocketBirthAuthority) -> Self {
        Self { authority }
    }

    /// The caller has positively joined original preparation and constructed
    /// this Installation from the exact successful kernel/journal observation.
    /// An observed option value or public profile cannot replace that receipt.
    pub(super) fn complete(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
        installation: &Installation,
    ) -> io::Result<Arc<Completed>> {
        if installation.source() != Source::Socket(admission.call)
            || installation.original_owner() != owner
            || !installation.matches_socket_origin_root(&self.authority.root)
            || installation.fd() < 0
        {
            return Err(io::Error::other(
                "Socket origin changed original installation",
            ));
        }
        self.complete_origin(owner, admission)
    }

    fn complete_origin(
        &self,
        owner: NetworkStreamOwner,
        admission: &Admission,
    ) -> io::Result<Arc<Completed>> {
        if owner != self.authority.owner || admission != &self.authority.admission {
            return Err(io::Error::other("Socket origin changed original call"));
        }
        Ok(Arc::new(Completed {
            admission: admission.clone(),
            owner,
            root: self.authority.root.clone(),
        }))
    }
}

/// Only original Socket provenance. In particular, this type says nothing
/// about linger, native setter effects, protocol replacement, or Close timing.
#[derive(Clone)]
pub(crate) struct Completed {
    root: Arc<ForegroundRoot>,
    admission: Admission,
    owner: NetworkStreamOwner,
}
impl std::fmt::Debug for Completed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("CompletedSocketOrigin(private)")
    }
}
impl Completed {
    pub(crate) fn matches_initial_root(&self, root: &ForegroundRoot) -> bool {
        std::ptr::eq(self.root.as_ref(), root.initial_ancestor())
    }
    pub(crate) fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.root, &other.root)
            && self.admission == other.admission
            && self.owner == other.owner
    }
    pub(crate) fn validates(
        &self,
        owner: NetworkStreamOwner,
        call: crate::network_replay::NetworkStreamCallId,
    ) -> bool {
        self.owner == owner && self.admission.call == call
    }
}

/// Explicit controlled original-installation premise, never production
/// physical-release authority. The actual publisher is still exercised.
#[cfg(test)]
pub(crate) fn controlled_birth_receipt(
    authority: SocketBirthAuthority,
    owner: NetworkStreamOwner,
    admission: &Admission,
) -> io::Result<Arc<Completed>> {
    Plan::capture(authority).complete_origin(owner, admission)
}

#[cfg(test)]
#[path = "socket_origin/tests.rs"]
mod tests;
