//! Accepted-only provider bootstrap. The owned Container callback invokes this
//! before workload permission, outside the guest namespaces. No capability is
//! inferred from Config or from an ordinary tracer's credentials.

use std::io;
use std::num::NonZeroU64;
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
    /// Existing finite enclosing bound, or the exact controller-owned product
    /// lifetime. Startup and terminal deadlines remain independently finite.
    pub lifetime: super::capability_unit::CapabilityServiceLifetime,
    /// Separately bounded helper stdout destination.
    pub stdout: OwnedFd,
    /// Separately bounded helper stderr destination.
    pub stderr: OwnedFd,
}

/// Exact artifact identity and resource counts from its qualified complete load.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderArtifact {
    /// Required package topology metadata; never a grouped broker capability.
    pub topology: super::ProviderTopology,
    /// Native grammar verified against the actual adapter before READY.
    pub wire_format: super::ProviderWireFormat,
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

/// A startup failure reports only the original cause. It grants no provider,
/// completion, inventory, or terminal authority; both endpoints retain custody.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct BootstrapFailure {
    pub error: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(super) enum BootstrapReply {
    Ready(ProviderReady),
    Failed(BootstrapFailure),
}

impl ProviderReady {
    pub(super) fn validate(&self, run: [u8; 16], expected: &ProviderArtifact) -> io::Result<()> {
        expected.topology.validate()?;
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
    grouped_bootstrap: Option<GroupedBootstrapTransport>,
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
    startup_cutoff_ns: NonZeroU64,
) -> io::Result<Vec<std::ffi::OsString>> {
    if !launch.helper.is_absolute()
        || !launch.object.is_absolute()
        || !launch.library.is_absolute()
        || launch.lifetime == super::capability_unit::CapabilityServiceLifetime::Bounded(0)
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
        "--startup-cutoff-ns".into(),
        startup_cutoff_ns.to_string().into(),
    ])
}

// Sample CLOCK_MONOTONIC before Instant: sampling latency can only shorten
// the caller's original deadline. Neither spawn nor helper receipt renews it.
fn startup_cutoff_ns(deadline: Instant) -> io::Result<NonZeroU64> {
    let mut clock: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut clock) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "accepted startup expired before spawn",
            )
        })?;
    let observed = u64::try_from(clock.tv_sec)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1_000_000_000))
        .and_then(|ns| {
            u64::try_from(clock.tv_nsec)
                .ok()
                .and_then(|part| ns.checked_add(part))
        });
    observed
        .and_then(|ns| {
            u64::try_from(remaining.as_nanos())
                .ok()
                .and_then(|part| ns.checked_add(part))
        })
        .and_then(NonZeroU64::new)
        .ok_or_else(|| io::Error::other("accepted startup cutoff overflow"))
}

/// Borrow of the actual retained wrapper before the first bootstrap request.
/// Only ParentAcceptedService constructs this value after Child retention.
/// A hook must keep every acquired grouped owner in its outer recovery scope
/// even when it returns an error; this borrow does not transfer the Child wait.
pub struct AcceptedSpawned<'a> {
    /// Original retained service wrapper; the parent keeps its wait ownership.
    pub wrapper: &'a Child,
    /// Exact transient service unit selected by the original launch.
    pub unit: &'a str,
    /// Original run incarnation shared with the accepted service.
    pub incarnation: [u8; 16],
    /// Original accepted provider package and helper launch configuration.
    pub launch: &'a AcceptedProviderLaunch,
    /// Exact arguments used to launch the retained service wrapper.
    pub arguments: &'a [std::ffi::OsString],
    /// Borrow of the original container controller pidfd.
    pub controller: std::os::fd::BorrowedFd<'a>,
    /// Existing bootstrap cutoff; the hook must not extend it.
    pub deadline: Instant,
}

/// Separate grouped bootstrap transport. This endpoint is only a transport;
/// the service must authenticate every native capability received through it.
#[derive(Debug)]
pub struct GroupedBootstrapTransport {
    endpoint: OwnedFd,
}
impl GroupedBootstrapTransport {
    /// Retain the original channel without granting capability authority.
    pub fn retain(endpoint: OwnedFd) -> Self {
        Self { endpoint }
    }
}

/// Invoked inside the original owner, between spawn and Bootstrap submission.
/// The callback's enclosing owner is retained on both success and failure.
pub trait AcceptedPostSpawn {
    /// Install grouped ownership after wrapper retention and before bootstrap.
    fn after_spawn(
        &mut self,
        spawned: AcceptedSpawned<'_>,
    ) -> io::Result<Option<GroupedBootstrapTransport>>;
    /// Advance retained ownership under the caller's existing deadline.
    fn progress(&mut self, _deadline: Instant) -> io::Result<()> {
        Ok(())
    }
}
struct NoGroupedBootstrap;
impl AcceptedPostSpawn for NoGroupedBootstrap {
    fn after_spawn(
        &mut self,
        _: AcceptedSpawned<'_>,
    ) -> io::Result<Option<GroupedBootstrapTransport>> {
        Ok(None)
    }
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
        unsafe {
            Self::start_after_clone_with_hook(
                launch,
                endpoint,
                controller,
                incarnation,
                deadline,
                &mut NoGroupedBootstrap,
            )
        }
    }

    /// Same owned launch with a typed post-spawn capability handoff. The safety
    /// requirements of start_after_clone apply unchanged.
    pub unsafe fn start_after_clone_with_hook(
        launch: AcceptedProviderLaunch,
        endpoint: OwnedFd,
        controller: OwnedFd,
        incarnation: [u8; 16],
        deadline: Instant,
        hook: &mut dyn AcceptedPostSpawn,
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
            grouped_bootstrap: None,
        };
        if let Err(error) = owner.start(deadline, hook) {
            return Err(ParentAcceptedStartFailure { error, owner });
        }
        Ok(owner)
    }

    fn start(&mut self, deadline: Instant, hook: &mut dyn AcceptedPostSpawn) -> io::Result<()> {
        let cutoff = startup_cutoff_ns(deadline)?;
        let arguments = helper_arguments(&self.launch, self.incarnation, cutoff)?;
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
        let mut command = super::capability_unit::CapabilityUnitLaunch {
            kind: super::capability_unit::CapabilityServiceKind::Accepted,
            unit: &self.unit,
            executable: &self.launch.helper,
            arguments: &arguments,
            lifetime: self.launch.lifetime,
            writable_directories: &[],
        }
        .command(&stdin, &self.launch.stdout, &self.launch.stderr)?;
        // This wrapper group belongs to this owner, not to the caller's shell.
        // The provider's systemd unit is a distinct ownership boundary.
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = command.spawn()?;
        self.wrapper = Some(child); // retained before any fallible handshake step
        self.grouped_bootstrap = hook.after_spawn(AcceptedSpawned {
            wrapper: self.wrapper.as_ref().unwrap(),
            unit: &self.unit,
            incarnation: self.incarnation,
            launch: &self.launch,
            arguments: &arguments,
            controller: self.controller.as_fd(),
            deadline,
        })?;
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "accepted startup deadline elapsed",
            ));
        }
        let mut rights = vec![
            duplicate(&self.controller)?,
            duplicate(self.endpoint.as_ref().unwrap())?,
        ];
        let operation = if let Some(grouped) = &self.grouped_bootstrap {
            rights.push(duplicate(&grouped.endpoint)?);
            Operation::GroupedBootstrap
        } else {
            Operation::Bootstrap
        };
        let session = self.session.as_mut().unwrap();
        let request = Envelope {
            run: self.incarnation,
            sequence: 0,
            owner: None,
            accept: None,
            operation,
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
            hook.progress(deadline)?;
            if !sent {
                sent = session.try_send(sequence)?;
            }
            if let Some(received) = session.try_receive()? {
                if received != Received::Acknowledged(sequence) {
                    return Err(io::Error::other("unexpected accepted startup request"));
                }
                let reply: BootstrapReply = serde_json::from_slice(
                    session
                        .response(sequence)?
                        .ok_or_else(|| io::Error::other("missing accepted startup reply"))?,
                )?;
                let ready = match reply {
                    BootstrapReply::Ready(ready) => ready,
                    BootstrapReply::Failed(failure) => {
                        return Err(io::Error::other(failure.error));
                    }
                };
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

    /// Borrow the same controller description owned since the Container's
    /// authenticated startup exchange; no caller-provided numeric PID.
    pub fn controller_pidfd(&self) -> std::os::fd::BorrowedFd<'_> {
        self.controller.as_fd()
    }
    /// EOF is never enough to permit provider or pin cleanup. This checks the
    /// actual retained controller pidfd; it does not reap or guess from a PID.
    pub fn controller_has_exited(&self) -> io::Result<bool> {
        super::accepted_transport::controller_exited(self.controller.as_fd())
    }
    /// The original authenticated complete inventory, in the provider ABI's
    /// zero-based kind namespace: map=0, program=1, link=2.
    pub fn original_ids(&self) -> Option<Vec<(u32, u32)>> {
        self.ready.as_ref().map(|ready| {
            [(0, &ready.maps), (1, &ready.programs), (2, &ready.links)]
                .into_iter()
                .flat_map(|(kind, ids)| ids.iter().map(move |id| (kind, *id)))
                .collect()
        })
    }
    /// Artifact identity required by this owned provider launch.
    pub fn expected_artifact(&self) -> &ProviderArtifact {
        &self.launch.expected
    }
    /// Authenticated run incarnation shared by this launch and its READY receipt.
    pub fn incarnation(&self) -> [u8; 16] {
        self.incarnation
    }
    /// Retained launcher process ID used by the existing owner's group check.
    pub fn launcher_group(&self) -> Option<u32> {
        self.wrapper.as_ref().map(Child::id)
    }
    /// Reap only this retained wrapper, never the service or a reconstructed PID.
    pub fn poll_launcher_terminal(&mut self) -> io::Result<Option<ExitStatus>> {
        if self.wrapper_status.is_none() {
            self.wrapper_status = match self.wrapper.as_mut() {
                Some(child) => child.try_wait()?,
                None => return Ok(None),
            };
        }
        Ok(self.wrapper_status)
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
    fn prebootstrap_expired_parent_deadline_is_rejected_before_spawn() {
        let expired = Instant::now() - std::time::Duration::from_millis(1);
        let error = startup_cutoff_ns(expired).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    }

    #[test]
    fn accepted_inventory_capacity_is_the_exact_artifact_sum() {
        let mut artifact = ProviderArtifact {
            topology: super::super::ProviderTopology::ClassicV40,
            wire_format: super::super::ProviderWireFormat::Abi7Copy4,
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
            topology: super::super::ProviderTopology::ClassicV40,
            wire_format: super::super::ProviderWireFormat::Abi7Copy4,
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
        let mut changed_wire = ready.clone();
        changed_wire.artifact.wire_format = super::super::ProviderWireFormat::Abi8Copy5;
        assert!(changed_wire.validate([3; 16], &expected).is_err());
        ready.artifact.btf_sha256 = [5; 32];
        assert!(ready.validate([3; 16], &expected).is_err());
        ready.artifact.btf_sha256 = [0; 32];
        assert!(ready.validate([3; 16], &ready.artifact).is_err());
    }

    #[test]
    fn accepted_ready_requires_exact_artifact_and_complete_nonzero_unique_inventory() {
        let expected = ProviderArtifact {
            topology: super::super::ProviderTopology::ClassicV40,
            wire_format: super::super::ProviderWireFormat::Abi7Copy4,
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
    #[test]
    fn original_read_inventory_requires_all_115_ids_including_all_five_shared_links() {
        let expected = ProviderArtifact {
            topology: super::super::ProviderTopology::ClassicV40,
            wire_format: super::super::ProviderWireFormat::Abi7Copy4,
            object_sha256: [1; 32],
            library_sha256: [2; 32],
            btf_sha256: [4; 32],
            maps: 22,
            programs: 44,
            links: 49,
        };
        assert_eq!(expected.inventory_capacity().unwrap().get(), 115);
        let ready = ProviderReady {
            incarnation: [3; 16],
            provider_incarnation: u64::from_le_bytes([3; 8]),
            artifact: expected.clone(),
            maps: (1..=22).collect(),
            programs: (1..=44).collect(),
            links: (1..=49).collect(),
        };
        ready.validate([3; 16], &expected).unwrap();
        // Each existing inline link and each new Read link is required independently.
        // Any prior 110..114-ID receipt cannot qualify this 115-object artifact.
        for index in 44..49 {
            let mut missing = ready.clone();
            missing.links.remove(index);
            assert!(missing.validate([3; 16], &expected).is_err());
            let mut duplicate = ready.clone();
            duplicate.links[index] = duplicate.links[index - 1];
            assert!(duplicate.validate([3; 16], &expected).is_err());
        }
        let mut extra = ready.clone();
        extra.links.push(50);
        assert!(extra.validate([3; 16], &expected).is_err());
        for links in 44..49 {
            let mut stale = expected.clone();
            stale.links = links;
            assert!(ready.validate([3; 16], &stale).is_err());
        }
    }
    #[test]
    fn epoll_copy_inventory_requires_117_ids_and_refuses_old_or_extra_attachments() {
        let expected = ProviderArtifact {
            topology: super::super::ProviderTopology::ClassicV40,
            wire_format: super::super::ProviderWireFormat::Abi7Copy4,
            object_sha256: [1; 32],
            library_sha256: [2; 32],
            btf_sha256: [4; 32],
            maps: 22,
            programs: 45,
            links: 50,
        };
        assert_eq!(expected.inventory_capacity().unwrap().get(), 117);
        let ready = ProviderReady {
            incarnation: [3; 16],
            provider_incarnation: u64::from_le_bytes([3; 8]),
            artifact: expected.clone(),
            maps: (1..=22).collect(),
            programs: (1..=45).collect(),
            links: (1..=50).collect(),
        };
        ready.validate([3; 16], &expected).unwrap();
        for index in 0..50 {
            let mut missing = ready.clone();
            missing.links.remove(index);
            assert!(missing.validate([3; 16], &expected).is_err());
            let mut duplicate = ready.clone();
            duplicate.links[index] = duplicate.links[(index + 1) % 50];
            assert!(duplicate.validate([3; 16], &expected).is_err());
        }
        let mut missing_program = ready.clone();
        missing_program.programs.pop();
        assert!(missing_program.validate([3; 16], &expected).is_err());
        let mut old = ready.clone();
        old.programs.pop();
        old.links.pop();
        old.artifact.programs = 44;
        old.artifact.links = 49;
        assert!(old.validate([3; 16], &expected).is_err());
        let mut separate_link = ready.clone();
        separate_link.links.push(51);
        separate_link.artifact.links = 51;
        assert!(separate_link.validate([3; 16], &expected).is_err());
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
    #[test]
    fn accepted_ready_requires_explicit_topology_and_exact_grouped_contract() {
        let artifact = ProviderArtifact {
            topology: super::super::ProviderTopology::GroupedV1 {
                contract_sha256: [9; 32],
            },
            wire_format: super::super::ProviderWireFormat::Abi7Copy4,
            object_sha256: [1; 32],
            library_sha256: [2; 32],
            btf_sha256: [4; 32],
            maps: 23,
            programs: 44,
            links: 44,
        };
        let ready = ProviderReady {
            incarnation: [3; 16],
            provider_incarnation: u64::from_le_bytes([3; 8]),
            artifact: artifact.clone(),
            maps: (1..=23).collect(),
            programs: (1..=44).collect(),
            links: (1..=44).collect(),
        };
        assert_eq!(artifact.inventory_capacity().unwrap().get(), 111);
        ready.validate([3; 16], &artifact).unwrap();
        let encoded = serde_json::to_vec(&ready).unwrap();
        let decoded: ProviderReady = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, ready);
        let mut ftrace_artifact = artifact.clone();
        ftrace_artifact.topology = super::super::ProviderTopology::FtraceV1 {
            contract_sha256: [7; 32],
        };
        let mut ftrace_ready = ready.clone();
        ftrace_ready.artifact = ftrace_artifact.clone();
        ftrace_ready.validate([3; 16], &ftrace_artifact).unwrap();
        for topology in [
            super::super::ProviderTopology::ClassicV40,
            super::super::ProviderTopology::GroupedV1 {
                contract_sha256: [8; 32],
            },
            super::super::ProviderTopology::GroupedV1 {
                contract_sha256: [0; 32],
            },
            super::super::ProviderTopology::FtraceV1 {
                contract_sha256: [9; 32],
            },
        ] {
            let mut changed = ready.clone();
            changed.artifact.topology = topology;
            assert!(changed.validate([3; 16], &artifact).is_err());
        }
        let mut zero = ready.clone();
        zero.artifact.topology = super::super::ProviderTopology::GroupedV1 {
            contract_sha256: [0; 32],
        };
        assert!(zero.validate([3; 16], &zero.artifact).is_err());
        let mut absent = serde_json::to_value(&artifact).unwrap();
        absent.as_object_mut().unwrap().remove("topology");
        assert!(serde_json::from_value::<ProviderArtifact>(absent).is_err());
        // Counts cannot turn classic metadata into grouped metadata.
        let mut classic = artifact.clone();
        classic.topology = super::super::ProviderTopology::ClassicV40;
        assert!(ready.validate([3; 16], &classic).is_err());
        for i in 0..16 {
            let mut changed = ready.clone();
            changed.incarnation[i] ^= 1;
            assert!(changed.validate([3; 16], &artifact).is_err());
        }
    }
}
