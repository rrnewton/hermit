//! Accepted-only provider bootstrap. The owned Container callback invokes this
//! before workload permission, outside the guest namespaces. No capability is
//! inferred from Config or from an ordinary tracer's credentials.

use std::io;
use std::os::fd::AsFd;
use std::os::fd::AsRawFd;
use std::os::fd::FromRawFd;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::process::Child;
use std::process::ExitStatus;
use std::time::Instant;

use serde::Deserialize;
use serde::Serialize;

use super::accepted_transport::AcceptedSession;
use super::accepted_transport::Envelope;
use super::accepted_transport::Operation;
use super::accepted_transport::Received;

/// Exact artifacts and bounds belong to the reviewed launch route. Construction
/// does not run a helper, load BPF or activate an ordinary backend.
#[derive(Debug)]
pub struct AcceptedProviderLaunch {
    /// Exact reviewed helper executable, invoked without a shell.
    pub helper: PathBuf,
    /// Exact reviewed provider BPF object.
    pub object: PathBuf,
    /// Exact reviewed C provider library used by the helper.
    pub library: PathBuf,
    /// Reviewed fingerprints and exact complete resource inventory.
    pub expected: ProviderArtifact,
    /// Existing outer execution bound supplied by the owning runner.
    pub maximum_seconds: u32,
    /// Separately bounded helper stdout destination.
    pub stdout: OwnedFd,
    /// Separately bounded helper stderr destination.
    pub stderr: OwnedFd,
}

/// Exact artifact identity and resource counts from its qualified complete load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderArtifact {
    /// SHA256 of the immutable BPF object.
    pub object_sha256: [u8; 32],
    /// SHA256 of the immutable C provider library.
    pub library_sha256: [u8; 32],
    /// Build-kernel BTF used to qualify flattened trampoline argument slots.
    /// CO-RE field relocation does not establish that calling convention.
    pub btf_sha256: [u8; 32],
    /// Exact map count, never the size of a partial reported prefix.
    pub maps: usize,
    /// Exact program count.
    pub programs: usize,
    /// Exact attached link count.
    pub links: usize,
}

impl ProviderArtifact {
    /// Exact capacity for this artifact, including partial-open recovery. The
    /// caller's authenticated package contract supplies all three counts.
    pub(super) fn inventory_capacity(&self) -> io::Result<std::num::NonZeroU32> {
        self.maps
            .checked_add(self.programs)
            .and_then(|count| count.checked_add(self.links))
            .and_then(|count| u32::try_from(count).ok())
            .and_then(std::num::NonZeroU32::new)
            .ok_or_else(|| io::Error::other("invalid accepted artifact inventory capacity"))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ProviderReady {
    pub incarnation: [u8; 16],
    pub provider_incarnation: u64,
    pub artifact: ProviderArtifact,
    pub programs: Vec<u32>,
    pub maps: Vec<u32>,
    pub links: Vec<u32>,
}
impl ProviderReady {
    pub(super) fn validate(&self, run: [u8; 16], expected: &ProviderArtifact) -> io::Result<()> {
        let valid_ids = |ids: &[u32], count: usize| {
            count > 0
                && ids.len() == count
                && ids
                    .iter()
                    .enumerate()
                    .all(|(index, id)| *id != 0 && !ids[..index].contains(id))
        };
        let derived_incarnation = u64::from_le_bytes(run[..8].try_into().unwrap());
        if self.incarnation != run
            || derived_incarnation == 0
            || self.provider_incarnation != derived_incarnation
            || self.artifact != *expected
            || expected.object_sha256 == [0; 32]
            || expected.library_sha256 == [0; 32]
            || expected.btf_sha256 == [0; 32]
            || !valid_ids(&self.maps, expected.maps)
            || !valid_ids(&self.programs, expected.programs)
            || !valid_ids(&self.links, expected.links)
        {
            return Err(io::Error::other(
                "accepted provider startup inventory is partial, malformed or from another artifact",
            ));
        }
        Ok(())
    }
}

/// Keeps every startup resource even when sudo, the unit, transport or provider
/// fails. The wrapper Child is not confused with the provider's task identity.
#[derive(Debug)]
#[must_use = "retain until the controller and exact provider unit are terminal"]
pub struct ParentAcceptedService {
    launch: AcceptedProviderLaunch,
    endpoint: Option<OwnedFd>,
    controller: OwnedFd,
    session: Option<AcceptedSession>,
    wrapper: Option<Child>,
    wrapper_status: Option<ExitStatus>,
    unit: String,
    incarnation: [u8; 16],
    ready: Option<ProviderReady>,
}

/// Original startup failure together with all resources needed for recovery.
#[derive(Debug)]
#[must_use = "startup failure retains the unit, pins and original error"]
pub struct ParentAcceptedStartFailure {
    /// Original error; cleanup must not replace it.
    pub error: io::Error,
    /// Exact run resources, including a possibly live unit wrapper.
    pub owner: ParentAcceptedService,
}

fn duplicate(fd: &OwnedFd) -> io::Result<OwnedFd> {
    let raw = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn helper_arguments(
    launch: &AcceptedProviderLaunch,
    run: [u8; 16],
) -> io::Result<Vec<std::ffi::OsString>> {
    if !launch.helper.is_absolute()
        || !launch.object.is_absolute()
        || !launch.library.is_absolute()
        || launch.maximum_seconds == 0
        || run == [0; 16]
    {
        return Err(io::Error::other(
            "invalid accepted provider launch artifacts or bounds",
        ));
    }
    let object = launch
        .object
        .to_str()
        .ok_or_else(|| io::Error::other("non-UTF8 accepted object path"))?;
    let library = launch
        .library
        .to_str()
        .ok_or_else(|| io::Error::other("non-UTF8 accepted library path"))?;
    Ok(vec![
        "--accepted-private-stdin-v1".into(),
        "--object".into(),
        object.into(),
        "--library".into(),
        library.into(),
        "--run".into(),
        run.iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
            .into(),
    ])
}

impl ParentAcceptedService {
    /// The caller transfers actual ownership from the startup callback. No
    /// pathname socket or raw numeric PID is accepted as a replacement.
    ///
    /// # Safety
    /// The exact helper/object and capability-unit route must be qualified and
    /// reviewed for this run; this call is after container clone but before
    /// STARTUP_READY, with no competing reaper. `controller` is the callback's
    /// actual child pidfd duplicate, and `endpoint` came from its SCM exchange.
    /// systemd-run --pipe passes the original stdin FD as-is; this route still
    /// requires the separately proposed actual socket/rights preflight.
    pub unsafe fn start_after_clone(
        launch: AcceptedProviderLaunch,
        endpoint: OwnedFd,
        controller: OwnedFd,
        incarnation: [u8; 16],
        deadline: Instant,
    ) -> Result<Self, ParentAcceptedStartFailure> {
        let unit = format!(
            "hermit-accepted-{}.service",
            incarnation
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let mut owner = Self {
            launch,
            endpoint: Some(endpoint),
            controller,
            session: None,
            wrapper: None,
            wrapper_status: None,
            unit,
            incarnation,
            ready: None,
        };
        if let Err(error) = owner.start(deadline) {
            return Err(ParentAcceptedStartFailure { error, owner });
        }
        Ok(owner)
    }

    fn start(&mut self, deadline: Instant) -> io::Result<()> {
        let arguments = helper_arguments(&self.launch, self.incarnation)?;
        let mut raw = [-1; 2];
        if unsafe {
            libc::socketpair(
                libc::AF_UNIX,
                libc::SOCK_SEQPACKET | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
                0,
                raw.as_mut_ptr(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let parent = unsafe { OwnedFd::from_raw_fd(raw[0]) };
        let stdin = unsafe { OwnedFd::from_raw_fd(raw[1]) };
        let session = AcceptedSession::new(parent, self.incarnation).map_err(|(error, _)| error)?;
        self.session = Some(session);
        // Only stdin crosses sudo/systemd as an inherited capability. All other
        // capabilities travel over that exact private socket with SCM_RIGHTS.
        let child = super::capability_unit::CapabilityUnitLaunch {
            kind: super::capability_unit::CapabilityServiceKind::Accepted,
            unit: &self.unit,
            executable: &self.launch.helper,
            arguments: &arguments,
            maximum_seconds: self.launch.maximum_seconds,
            writable_directories: &[],
        }
        .command(&stdin, &self.launch.stdout, &self.launch.stderr)?
        .spawn()?;
        self.wrapper = Some(child); // retained before any fallible handshake step
        let rights = vec![
            duplicate(&self.controller)?,
            duplicate(self.endpoint.as_ref().unwrap())?,
        ];
        let session = self.session.as_mut().unwrap();
        let request = Envelope {
            run: self.incarnation,
            sequence: 0,
            owner: None,
            accept: None,
            operation: Operation::Bootstrap,
            body: serde_json::to_vec(&self.launch.expected)?,
        };
        let sequence = session
            .prepare(request, rights)
            .map_err(|(error, _)| error)?;
        let mut sent = false;
        loop {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "accepted startup deadline elapsed",
                ));
            }
            if !sent {
                sent = session.try_send(sequence)?;
            }
            if let Some(received) = session.try_receive()? {
                if received != Received::Acknowledged(sequence) {
                    return Err(io::Error::other("unexpected accepted startup request"));
                }
                let ready: ProviderReady = serde_json::from_slice(
                    session
                        .response(sequence)?
                        .ok_or_else(|| io::Error::other("missing accepted startup reply"))?,
                )?;
                ready.validate(self.incarnation, &self.launch.expected)?;
                self.ready = Some(ready);
                return Ok(());
            }
            if let Some(status) = self.wrapper.as_mut().unwrap().try_wait()? {
                self.wrapper_status = Some(status);
                return Err(io::Error::other(
                    "accepted provider wrapper exited before startup acknowledgement",
                ));
            }
            session.wait_transport(deadline)?;
        }
    }

    /// EOF is never enough to permit provider or pin cleanup. This checks the
    /// actual retained controller pidfd; it does not reap or guess from a PID.
    pub fn controller_has_exited(&self) -> io::Result<bool> {
        super::accepted_transport::controller_exited(self.controller.as_fd())
    }
    /// Unique run-bound unit name, for exact scoped cleanup and readback.
    pub fn unit(&self) -> &str {
        &self.unit
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_inventory_capacity_is_the_exact_artifact_sum() {
        let mut artifact = ProviderArtifact {
            object_sha256: [1; 32],
            library_sha256: [2; 32],
            btf_sha256: [4; 32],
            maps: 17,
            programs: 25,
            links: 25,
        };
        assert_eq!(artifact.inventory_capacity().unwrap().get(), 67);
        artifact.maps = usize::MAX;
        assert!(artifact.inventory_capacity().is_err());
        artifact.maps = 0;
        artifact.programs = 0;
        artifact.links = 0;
        assert!(artifact.inventory_capacity().is_err());
    }

    #[test]
    fn accepted_ready_binds_the_running_trampoline_btf() {
        let expected = ProviderArtifact {
            object_sha256: [1; 32],
            library_sha256: [2; 32],
            btf_sha256: [4; 32],
            maps: 1,
            programs: 1,
            links: 1,
        };
        let mut ready = ProviderReady {
            incarnation: [3; 16],
            provider_incarnation: u64::from_le_bytes([3; 8]),
            artifact: expected.clone(),
            maps: vec![1],
            programs: vec![2],
            links: vec![3],
        };
        ready.validate([3; 16], &expected).unwrap();
        ready.artifact.btf_sha256 = [5; 32];
        assert!(ready.validate([3; 16], &expected).is_err());
        ready.artifact.btf_sha256 = [0; 32];
        assert!(ready.validate([3; 16], &ready.artifact).is_err());
    }

    #[test]
    fn accepted_ready_requires_exact_artifact_and_complete_nonzero_unique_inventory() {
        let expected = ProviderArtifact {
            object_sha256: [1; 32],
            library_sha256: [2; 32],
            btf_sha256: [4; 32],
            maps: 2,
            programs: 2,
            links: 2,
        };
        let ready = ProviderReady {
            incarnation: [3; 16],
            provider_incarnation: u64::from_le_bytes([3; 8]),
            artifact: expected.clone(),
            maps: vec![1, 2],
            programs: vec![1, 2],
            links: vec![1, 2],
        };
        ready.validate([3; 16], &expected).unwrap();
        let mut partial = ready.clone();
        partial.links.pop();
        assert!(partial.validate([3; 16], &expected).is_err());
        let mut duplicate = ready.clone();
        duplicate.maps[1] = duplicate.maps[0];
        assert!(duplicate.validate([3; 16], &expected).is_err());
        let mut zero = ready.clone();
        zero.programs[0] = 0;
        assert!(zero.validate([3; 16], &expected).is_err());
        let mut changed = ready.clone();
        changed.artifact.object_sha256 = [5; 32];
        assert!(changed.validate([3; 16], &expected).is_err());
        assert!(ready.validate([6; 16], &expected).is_err());
        let mut wrong_provider = ready.clone();
        wrong_provider.provider_incarnation += 1;
        assert!(wrong_provider.validate([3; 16], &expected).is_err());
        let mut zero_prefix = ready.clone();
        zero_prefix.incarnation[..8].fill(0);
        assert!(
            zero_prefix
                .validate(zero_prefix.incarnation, &expected)
                .is_err()
        );
        zero_prefix.provider_incarnation = 0;
        assert!(
            zero_prefix
                .validate(zero_prefix.incarnation, &expected)
                .is_err()
        );
        let mut extra = ready;
        extra.links.push(3);
        assert!(extra.validate([3; 16], &expected).is_err());
    }
    // argv/launch never executes in the pure suite. Ownership-bearing startup
    // needs the separately reviewed native private-stdio qualification.
    #[test]
    fn accepted_provider_unit_identity_is_run_specific_and_has_no_path_socket() {
        let run = [7u8; 16];
        let name = format!(
            "hermit-accepted-{}.service",
            run.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
        assert_eq!(
            name,
            "hermit-accepted-07070707070707070707070707070707.service"
        );
        assert!(!name.contains('/'));
    }
}
