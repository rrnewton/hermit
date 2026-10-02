//! Original CLI-side child/unit ownership. This module is nested in owner so
//! its bounded command flights use the same retained native query primitive.
//! It does not change the isolated S1 Launcher's global P_ALL/ECHILD predicate.
use std::ffi::OsString;
use std::os::unix::process::ExitStatusExt;
use std::path::PathBuf;
use std::time::Duration;

use serde_json::Value;
use serde_json::json;

use super::super::guardian;
use super::super::hex;
use super::super::journal;
use super::super::parent_launch;
use super::super::runtime_cleanup as common;
use super::super::wire;
use super::*;
use crate::network_runtime::accepted_parent::AcceptedPostSpawn;
use crate::network_runtime::accepted_parent::AcceptedSpawned;
use crate::network_runtime::accepted_parent::GroupedBootstrapTransport;
use crate::network_runtime::capability_unit::CAPABILITY_ENVIRONMENT;
use crate::network_runtime::capability_unit::CAPABILITY_SUDO;
use crate::network_runtime::capability_unit::CapabilityServiceKind;
use crate::network_runtime::capability_unit::CapabilityServiceLifetime;
use crate::network_runtime::capability_unit::CapabilityUnitLaunch;

fn random_nonce() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    let raw = unsafe { libc::syscall(libc::SYS_getrandom, bytes.as_mut_ptr(), bytes.len(), 0) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    require(
        raw == 16 && bytes != [0; 16],
        "broker nonce generation short or zero",
    )?;
    Ok(hex(&bytes))
}
fn pair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut raw = [-1; 2];
    if unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
            raw.as_mut_ptr(),
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let left = unsafe { OwnedFd::from_raw_fd(raw[0]) };
    let right = unsafe { OwnedFd::from_raw_fd(raw[1]) };
    for fd in [&left, &right] {
        let one = 1i32;
        if unsafe {
            libc::setsockopt(
                fd.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PASSCRED,
                (&one as *const i32).cast(),
                std::mem::size_of_val(&one) as libc::socklen_t,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
    }
    Ok((left, right))
}
fn directory(root: BorrowedFd<'_>, name: &str) -> io::Result<OwnedFd> {
    require(
        !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
        "broker directory name is not a fixed basename",
    )?;
    let name = CString::new(name).unwrap();
    if unsafe { libc::mkdirat(root.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let raw = unsafe {
        libc::openat(
            root.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let file = unsafe { OwnedFd::from_raw_fd(raw) };
    let identity = stat(file.as_raw_fd())?;
    require(
        identity.mode & libc::S_IFMT == libc::S_IFDIR
            && identity.mode & 0o7777 == 0o700
            && identity.uid == unsafe { libc::getuid() },
        "broker directory changed native owner or mode",
    )?;
    if unsafe { libc::fsync(root.as_raw_fd()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(file)
}

/// One retained CLI owner, shared only by the before-READY callback and the
/// finalizer in the same thread. It always owns the original Child objects.
#[must_use = "broker children, exact unit and partial startup remain retained"]
pub struct GroupedParentOwner {
    root: OwnedFd,
    helper: PathBuf,
    bridge: OwnedFd,
    bridge_sha256: [u8; 32],
    namespace_setup: OwnedFd,
    namespace_setup_sha256: [u8; 32],
    image: Option<OwnedFd>,
    self_pidfd: Option<OwnedFd>,
    run_controller: Option<OwnedFd>,
    directories: Vec<OwnedFd>,
    run: Option<[u8; 16]>,
    native: Option<u64>,
    deadline: Option<Instant>,
    source_unit: Option<String>,
    leaf_unit: Option<String>,
    keeper_unit: Option<String>,
    startup: Option<Launcher>,
    keeper: Option<Launcher>,
    source_authority: Option<SourceAuthorityUnit>,
    source_authority_channel: Option<wire::Channel>,
    startup_channel: Option<wire::Channel>,
    keeper_channel: Option<wire::Channel>,
    retained_endpoints: Vec<OwnedFd>,
    service_bootstrap: Option<wire::Channel>,
    coordination: Option<wire::Channel>,
    coordination_peer: Option<wire::Credentials>,
    provider_lease: Option<LauncherLease>,
    provider_capture: Option<parent_launch::ParentLaunchCustody>,
    keeper_capture: Option<parent_launch::ParentLaunchCustody>,
    keeper_handles: Vec<OwnedFd>,
    keeper_identity: Option<Value>,
    provider_identity: Option<Value>,
    queries: Vec<ManagerQuery>,
    commands: Vec<CommandQuery>,
    pending_join: Option<Value>,
    pending_from_keeper: bool,
    startup_joined: bool,
    startup_failed_joined: bool,
    keeper_joined: bool,
    retained_startup_join: Option<Value>,
    startup_success_observed: bool,
    attempted: bool,
    refusal: Option<Failure>,
    failure_origin: Option<u64>,
    last_join: Option<Value>,
    failed_startup_observation: Option<Value>,
    source_authority_retirement: Option<Value>,
    source_authority_retirement_recorded: bool,
}
impl GroupedParentOwner {
    /// Custody only. The root must be a fresh private sibling of the authenticated
    /// accepted receipt directory, inside the same bounded run output tree.
    pub fn retain(
        root: OwnedFd,
        helper: PathBuf,
        bridge: OwnedFd,
        bridge_sha256: [u8; 32],
        namespace_setup: OwnedFd,
        namespace_setup_sha256: [u8; 32],
    ) -> Self {
        Self {
            root,
            helper,
            bridge,
            bridge_sha256,
            namespace_setup,
            namespace_setup_sha256,
            image: None,
            self_pidfd: None,
            run_controller: None,
            directories: Vec::new(),
            run: None,
            native: None,
            deadline: None,
            source_unit: None,
            leaf_unit: None,
            keeper_unit: None,
            startup: None,
            keeper: None,
            source_authority: None,
            source_authority_channel: None,
            startup_channel: None,
            keeper_channel: None,
            retained_endpoints: Vec::new(),
            service_bootstrap: None,
            coordination: None,
            coordination_peer: None,
            provider_lease: None,
            provider_capture: None,
            keeper_capture: None,
            keeper_handles: Vec::new(),
            keeper_identity: None,
            provider_identity: None,
            queries: Vec::new(),
            commands: Vec::new(),
            pending_join: None,
            pending_from_keeper: false,
            startup_joined: false,
            startup_failed_joined: false,
            keeper_joined: false,
            retained_startup_join: None,
            startup_success_observed: false,
            attempted: false,
            refusal: None,
            failure_origin: None,
            last_join: None,
            failed_startup_observation: None,
            source_authority_retirement: None,
            source_authority_retirement_recorded: false,
        }
    }
    fn remember<T>(&mut self, result: io::Result<T>) -> io::Result<T> {
        if let Err(error) = &result
            && self.refusal.is_none()
        {
                self.refusal = Some(Failure::capture(error));
                self.failure_origin = guardian::monotonic_ns().ok();
            }
        result
    }
    fn run_fields(&self) -> io::Result<(String, u64, u64)> {
        let run = self
            .run
            .ok_or_else(|| io::Error::other("broker original run absent"))?;
        Ok((
            hex(&run),
            u64::from_le_bytes(run[..8].try_into().unwrap()),
            self.native
                .ok_or_else(|| io::Error::other("broker native stage absent"))?,
        ))
    }
    fn pause(&mut self, cutoff: u64) -> io::Result<()> {
        common::before(cutoff)?;
        if let Some(owner) = &mut self.source_authority {
            owner.drain()?;
        }
        for launcher in [&mut self.startup, &mut self.keeper].into_iter().flatten() {
            launcher.drain()?;
        }
        std::thread::sleep(Duration::from_millis(1));
        common::before(cutoff)
    }
    fn configure(&mut self, spawned: AcceptedSpawned<'_>) -> io::Result<GroupedBootstrapTransport> {
        require(!self.attempted, "grouped parent startup cannot repeat")?;
        self.attempted = true;
        self.run = Some(spawned.incarnation);
        // Pin the broker's shorter phase before its first effect. The caller's
        // thirty-second startup bound remains unchanged and is never refreshed.
        let phase_start = Instant::now();
        let deadline = spawned.deadline.min(
            phase_start
                .checked_add(Duration::from_secs(20))
                .ok_or_else(|| io::Error::other("broker original startup overflow"))?,
        );
        self.deadline = Some(deadline);
        let now = guardian::monotonic_ns()?;
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .as_nanos();
        require(
            remaining > 0 && remaining <= 20_000_000_000,
            "grouped parent original startup exceeds20s",
        )?;
        self.native = Some(
            now.checked_add(u64::try_from(remaining).map_err(io::Error::other)?)
                .ok_or_else(|| io::Error::other("broker stage overflow"))?,
        );
        let (nonce, incarnation, native) = self.run_fields()?;
        require(
            incarnation != 0
                && self.helper == spawned.launch.helper
                && self.bridge_sha256 == spawned.launch.expected.library_sha256,
            "grouped parent artifacts differ from actual accepted launch",
        )?;
        // This hook runs in the actual post-clone CLI parent. Do not change the
        // guest clone or the existing subreaper discipline.
        if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        protected_holder()?;
        self.run_controller = Some(common::duplicate(spawned.controller)?);
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        self.self_pidfd = Some(unsafe { OwnedFd::from_raw_fd(fd as i32) });
        self.image = Some(std::fs::File::open(&self.helper)?.into());
        for name in [
            "startup-logs",
            "keeper-logs",
            "source",
            "runtime-keeper",
            "runtime-service",
            "source-owner-logs",
            "source-owner",
        ] {
            self.directories.push(directory(self.root.as_fd(), name)?);
        }
        self.source_unit = Some(format!("hermit-accepted-{}.service", random_nonce()?));
        self.leaf_unit = Some(format!("hermit-accepted-{}.service", random_nonce()?));
        let keeper_nonce = random_nonce()?;
        self.keeper_unit = Some(format!("hermit-accepted-{keeper_nonce}.service"));
        // Both owners are installed before initialization/capture. The Keeper
        // helper lives in its own capability unit; its wrapper is our Child.
        let (parent, input) = pair()?;
        self.keeper_channel = Some(wire::Channel::retain(parent));
        self.retained_endpoints.push(input);
        let args: Vec<OsString> = [
            "--grouped-runtime-keeper-private-stdin-v1".to_owned(),
            "--run".into(),
            nonce.clone(),
            "--deadline-ns".into(),
            native.to_string(),
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        let spec = CapabilityUnitLaunch {
            kind: CapabilityServiceKind::AcceptedKeeper,
            unit: self.keeper_unit.as_ref().unwrap(),
            executable: &self.helper,
            arguments: &args,
            lifetime: CapabilityServiceLifetime::ControllerOwned,
            writable_directories: &[],
        };
        let mut command = Command::new(CAPABILITY_SUDO);
        command
            .env_clear()
            .envs(CAPABILITY_ENVIRONMENT.iter().copied())
            .args(spec.arguments()?)
            .stdin(Stdio::from(common::duplicate(
                self.retained_endpoints.last().unwrap().as_fd(),
            )?))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        self.keeper = Some(Launcher::retain(command.spawn()?));
        self.keeper
            .as_mut()
            .unwrap()
            .initialize(self.directories[1].as_raw_fd())?;
        self.keeper_capture = Some(parent_launch::ParentLaunchCustody::retain(
            self.keeper_unit.as_ref().unwrap().clone(),
            keeper_nonce,
            deadline,
            self.keeper.as_ref().unwrap().source_lease()?,
        ));
        self.keeper_capture.as_mut().unwrap().start()?;
        while !self.keeper_capture.as_mut().unwrap().progress()? {
            self.pause(native)?;
        }
        let (record, fds) =
            self.keeper_capture
                .as_mut()
                .unwrap()
                .captured_transfer(&nonce, native, &[])?;
        self.keeper_identity = Some(record);
        for fd in fds {
            self.keeper_handles.push(common::duplicate(fd)?);
        }
        let keeper_pid = self.keeper_identity.as_ref().unwrap()["pid"]
            .as_i64()
            .unwrap() as i32;
        let keeper_peer = common::credentials(keeper_pid)?;
        let header = json!({"schema":"hermit-grouped-runtime-keeper-bootstrap-v1","run":nonce,
            "incarnation":incarnation,"stage_deadline":native,"library_sha256":hex(&self.bridge_sha256)});
        common::send(
            self.keeper_channel.as_mut().unwrap(),
            &header,
            &[
                self.bridge.as_fd(),
                self.directories[3].as_fd(),
                self.self_pidfd.as_ref().unwrap().as_fd(),
            ],
            native,
        )?;
        // Actual Keeper creates these pairs, so SO_PEERCRED remains the real
        // helper even after SCM forwarding by this unrelated CLI process.
        let mut exported = Vec::new();
        for schema in [
            "hermit-grouped-runtime-creation-channel-v1",
            "hermit-grouped-runtime-provider-channel-v1",
        ] {
            let index = common::receive(
                self.keeper_channel.as_mut().unwrap(),
                keeper_peer,
                1,
                4096,
                native,
            )?;
            let packet = &mut self.keeper_channel.as_mut().unwrap().packets[index];
            require(
                packet.bytes
                    == journal::canonical(
                        &json!({"schema":schema,"nonce":nonce,"incarnation":incarnation,
                "stage_deadline":native}),
                    )?,
                "runtime Keeper channel export differs",
            )?;
            exported.push(packet.rights.pop().unwrap());
        }
        self.retained_endpoints.extend(exported);
        let creation_endpoint = self.retained_endpoints.len() - 2;
        let runtime_endpoint = creation_endpoint + 1;
        let (parent, input) = pair()?;
        self.startup_channel = Some(wire::Channel::retain(parent));
        self.retained_endpoints.push(input);
        let mut command = Command::new(&self.helper);
        command
            .env_clear()
            .envs(CAPABILITY_ENVIRONMENT.iter().copied())
            .args([
                "--grouped-startup-controller-private-stdin-v1",
                "--run",
                &nonce,
                "--deadline-ns",
                &native.to_string(),
            ])
            .stdin(Stdio::from(common::duplicate(
                self.retained_endpoints.last().unwrap().as_fd(),
            )?))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        self.startup = Some(Launcher::retain(command.spawn()?));
        self.startup
            .as_mut()
            .unwrap()
            .initialize(self.directories[0].as_raw_fd())?;
        let startup_pid = self.startup.as_ref().unwrap().child.id() as i32;
        // Complete provider capture while its exact original Child remains in
        // ParentAcceptedService; no wrapper PID is used as service identity.
        LauncherLease::capture_child(spawned.wrapper, &mut self.provider_lease)?;
        self.provider_capture = Some(parent_launch::ParentLaunchCustody::retain(
            spawned.unit.to_owned(),
            nonce.clone(),
            deadline,
            self.provider_lease.take().unwrap(),
        ));
        self.provider_capture.as_mut().unwrap().start()?;
        while !self.provider_capture.as_mut().unwrap().progress()? {
            self.pause(native)?;
        }
        let mut arguments = vec![self.helper.to_string_lossy().into_owned()];
        for argument in spawned.arguments {
            arguments.push(
                argument
                    .to_str()
                    .ok_or_else(|| io::Error::other("provider argv is not UTF8"))?
                    .to_owned(),
            );
        }
        let (record, _) = self
            .provider_capture
            .as_mut()
            .unwrap()
            .captured_transfer(&nonce, native, &arguments)?;
        self.provider_identity = Some(record);
        let provider_pid = self.provider_identity.as_ref().unwrap()["pid"]
            .as_i64()
            .unwrap() as i32;
        self.coordination_peer = Some(common::credentials(provider_pid)?);
        // This ordinary owner runs in a separate user-manager unit. It keeps
        // real source sudo/Child custody outside the NNP1 capability Keeper.
        let owner_nonce = random_nonce()?;
        let owner_unit = format!("hermit-source-owner-{owner_nonce}.service");
        let (parent, input) = pair()?;
        self.source_authority_channel = Some(wire::Channel::retain(parent));
        let owner_args: Vec<OsString> = [
            "--grouped-source-owner-private-stdin-v1".to_owned(),
            "--run".into(),
            nonce.clone(),
            "--deadline-ns".into(),
            native.to_string(),
        ]
        .into_iter()
        .map(Into::into)
        .collect();
        self.source_authority = Some(SourceAuthorityUnit::retain(
            owner_unit,
            owner_nonce,
            self.helper.clone(),
            owner_args,
            (
                input,
                common::duplicate(self.directories[5].as_fd())?,
                common::duplicate(self.image.as_ref().unwrap().as_fd())?,
            ),
            deadline,
            native,
        ));
        self.source_authority.as_mut().unwrap().start()?;
        while !self.source_authority.as_mut().unwrap().progress()? {
            self.pause(native)?;
        }
        let owner_peer = self.source_authority.as_ref().unwrap().credentials()?;
        common::send(
            self.source_authority_channel.as_mut().unwrap(),
            &json!({"schema":"hermit-grouped-source-owner-bootstrap-v1",
            "nonce":nonce,"incarnation":incarnation,"stage_deadline":native,"source_unit":self.source_unit}),
            &[
                self.directories[6].as_fd(),
                self.self_pidfd.as_ref().unwrap().as_fd(),
                self.image.as_ref().unwrap().as_fd(),
            ],
            native,
        )?;
        let index = common::receive(
            self.source_authority_channel.as_mut().unwrap(),
            owner_peer,
            1,
            4096,
            native,
        )?;
        let packet = &mut self.source_authority_channel.as_mut().unwrap().packets[index];
        require(
            packet.bytes
                == journal::canonical(&json!({"schema":"hermit-grouped-source-owner-channel-v1",
            "nonce":nonce,"incarnation":incarnation,"stage_deadline":native}))?,
            "source authority actual endpoint export differs",
        )?;
        self.retained_endpoints.push(packet.rights.pop().unwrap());
        let source_authority_endpoint = self.retained_endpoints.len() - 1;
        for (sequence, role, pid, pin) in [
            (1, "keeper", keeper_pid, self.keeper_handles[0].as_fd()),
            (
                2,
                "startup",
                startup_pid,
                self.startup
                    .as_ref()
                    .unwrap()
                    .pidfd
                    .as_ref()
                    .unwrap()
                    .as_fd(),
            ),
        ] {
            common::send(
                self.source_authority_channel.as_mut().unwrap(),
                &json!({"schema":"hermit-grouped-source-owner-link-v1",
                "sequence":sequence,"role":role,"nonce":nonce,"incarnation":incarnation,"stage_deadline":native,"pid":pid}),
                &[pin],
                native,
            )?;
        }
        let ready = common::receive_value(
            self.source_authority_channel.as_mut().unwrap(),
            owner_peer,
            native,
        )?;
        require(
            ready
                == json!({"schema":"hermit-grouped-source-owner-ready-v1","nonce":nonce,
            "incarnation":incarnation,"stage_deadline":native}),
            "source authority original bootstrap ready differs",
        )?;
        for (sequence, role, pid, pin) in [
            (
                1,
                "startup",
                startup_pid,
                self.startup
                    .as_ref()
                    .unwrap()
                    .pidfd
                    .as_ref()
                    .unwrap()
                    .as_fd(),
            ),
            (
                2,
                "provider",
                provider_pid,
                self.provider_capture
                    .as_mut()
                    .unwrap()
                    .captured_transfer(&nonce, native, &arguments)?
                    .1[0],
            ),
        ] {
            common::send(
                self.keeper_channel.as_mut().unwrap(),
                &json!({"schema":"hermit-grouped-runtime-keeper-link-v1",
                "sequence":sequence,"role":role,"run":nonce,"stage_deadline":native,"pid":pid}),
                &[pin],
                native,
            )?;
        }
        common::send(
            self.keeper_channel.as_mut().unwrap(),
            &json!({"schema":"hermit-grouped-runtime-keeper-link-v1",
            "sequence":3,"role":"run_controller","run":nonce,"stage_deadline":native}),
            &[self.run_controller.as_ref().unwrap().as_fd()],
            native,
        )?;
        common::send(
            self.keeper_channel.as_mut().unwrap(),
            &json!({"schema":"hermit-grouped-runtime-keeper-link-v1",
            "sequence":4,"role":"source_owner","run":nonce,"stage_deadline":native,"pid":owner_peer.pid}),
            &[
                self.source_authority.as_ref().unwrap().pidfd()?,
                self.retained_endpoints[source_authority_endpoint].as_fd(),
            ],
            native,
        )?;
        let ready =
            common::receive_value(self.keeper_channel.as_mut().unwrap(), keeper_peer, native)?;
        require(
            ready
                == json!({"schema":"hermit-grouped-runtime-keeper-bootstrap-ready-v1","run":nonce,
            "incarnation":incarnation,"stage_deadline":native}),
            "runtime Keeper bootstrap changed identity",
        )?;
        common::send(
            self.startup_channel.as_mut().unwrap(),
            &json!({"schema":"hermit-grouped-startup-entry-v1",
            "role":"controller","nonce":nonce,"incarnation":incarnation,"stage_deadline":native}),
            &[],
            native,
        )?;
        common::send(
            self.startup_channel.as_mut().unwrap(),
            &json!({"schema":"hermit-grouped-startup-controller-config-v2",
            "nonce":nonce,"incarnation":incarnation,"stage_deadline":native,"executable":self.helper,
            "bridge_sha256":hex(&self.bridge_sha256),"namespace_setup_sha256":hex(&self.namespace_setup_sha256),
            "source_unit":self.source_unit,"leaf_unit":self.leaf_unit}),
            &[
                self.image.as_ref().unwrap().as_fd(),
                self.bridge.as_fd(),
                self.directories[2].as_fd(),
            ],
            native,
        )?;
        common::send(
            self.startup_channel.as_mut().unwrap(),
            &json!({"schema":"hermit-grouped-controller-setup-image-v1",
            "nonce":nonce,"incarnation":incarnation,"stage_deadline":native}),
            &[self.namespace_setup.as_fd()],
            native,
        )?;
        let (record, fds) = self
            .provider_capture
            .as_mut()
            .unwrap()
            .captured_transfer(&nonce, native, &arguments)?;
        common::send(
            self.startup_channel.as_mut().unwrap(),
            &record,
            &fds,
            native,
        )?;
        common::send(
            self.startup_channel.as_mut().unwrap(),
            &json!({"schema":"hermit-grouped-provider-image-v1",
            "run":nonce,"stage_deadline":native}),
            &[self.image.as_ref().unwrap().as_fd()],
            native,
        )?;
        common::send(
            self.startup_channel.as_mut().unwrap(),
            &json!({"schema":"hermit-grouped-controller-runtime-v1",
            "nonce":nonce,"incarnation":incarnation,"stage_deadline":native,"keeper_pid":keeper_peer.pid,
            "keeper_uid":keeper_peer.uid,"keeper_gid":keeper_peer.gid}),
            &[
                self.retained_endpoints[creation_endpoint].as_fd(),
                self.keeper_handles[0].as_fd(),
            ],
            native,
        )?;
        let index = common::receive(
            self.startup_channel.as_mut().unwrap(),
            common::credentials(startup_pid)?,
            1,
            4096,
            native,
        )?;
        let packet = &mut self.startup_channel.as_mut().unwrap().packets[index];
        require(
            packet.bytes
                == journal::canonical(&json!({"schema":"hermit-grouped-successor-channel-v1",
            "nonce":nonce,"incarnation":incarnation,"stage_deadline":native}))?,
            "actual S2 endpoint export differs",
        )?;
        self.retained_endpoints.push(packet.rights.pop().unwrap());
        let s2_endpoint = self.retained_endpoints.len() - 1;
        let (parent, input) = pair()?;
        self.service_bootstrap = Some(wire::Channel::retain(parent));
        self.retained_endpoints.push(input);
        let bootstrap_endpoint = self.retained_endpoints.len() - 1;
        let (parent, input) = pair()?;
        self.coordination = Some(wire::Channel::retain(parent));
        self.retained_endpoints.push(input);
        let coordinator_endpoint = self.retained_endpoints.len() - 1;
        common::send(
            self.service_bootstrap.as_mut().unwrap(),
            &json!({"schema":"hermit-grouped-runtime-service-bootstrap-v1",
            "run":nonce,"incarnation":incarnation,"stage_deadline":native,"unit":spawned.unit}),
            &[self.directories[4].as_fd()],
            native,
        )?;
        for (sequence, role, pid, pin, endpoint) in [
            (
                1,
                "source",
                startup_pid,
                self.startup
                    .as_ref()
                    .unwrap()
                    .pidfd
                    .as_ref()
                    .unwrap()
                    .as_fd(),
                self.retained_endpoints[s2_endpoint].as_fd(),
            ),
            (
                2,
                "keeper",
                keeper_pid,
                self.keeper_handles[0].as_fd(),
                self.retained_endpoints[runtime_endpoint].as_fd(),
            ),
            (
                3,
                "parent",
                unsafe { libc::getpid() },
                self.self_pidfd.as_ref().unwrap().as_fd(),
                self.retained_endpoints[coordinator_endpoint].as_fd(),
            ),
        ] {
            common::send(
                self.service_bootstrap.as_mut().unwrap(),
                &json!({"schema":"hermit-grouped-runtime-service-link-v1",
                "run":nonce,"stage_deadline":native,"sequence":sequence,"role":role,"pid":pid}),
                &[pin, endpoint],
                native,
            )?;
        }
        common::before(native)?;
        Ok(GroupedBootstrapTransport::retain(common::duplicate(
            self.retained_endpoints[bootstrap_endpoint].as_fd(),
        )?))
    }
    fn current_snapshot(&mut self, unit: &str, cutoff: u64) -> io::Result<ManagerSnapshot> {
        require(self.queries.len() < 64, "broker manager query count bound")?;
        self.queries.push(ManagerQuery::retain(unit.to_owned()));
        let index = self.queries.len() - 1;
        self.queries[index].start()?;
        loop {
            common::before(cutoff)?;
            let now = guardian::monotonic_ns()?;
            let deadline = Instant::now()
                + Duration::from_nanos(
                    cutoff
                        .checked_sub(now)
                        .filter(|v| *v > 0)
                        .ok_or_else(|| io::Error::other("original broker cutoff elapsed"))?,
                );
            if let Some(snapshot) = self.queries[index].poll(deadline)? {
                common::before(cutoff)?;
                return Ok(snapshot);
            }
            self.pause(cutoff)?;
        }
    }
    fn stop_keeper_after_terminal(&mut self, cutoff: u64) -> io::Result<()> {
        require(
            terminal(self.keeper_handles[0].as_raw_fd())?,
            "runtime Keeper still live before unit retirement",
        )?;
        let unit = self.keeper_unit.as_ref().unwrap().clone();
        let snapshot = self.current_snapshot(&unit, cutoff)?;
        let captured = self.keeper_identity.as_ref().unwrap().clone();
        require(
            snapshot.unit() == unit
                && snapshot.property("Id") == Some(unit.as_str())
                && snapshot.property("LoadState") == Some("loaded")
                && snapshot.property("InvocationID") == captured["invocation"].as_str()
                && snapshot.property("MainPID") == Some("0")
                && snapshot.property("Result") == Some("success")
                && snapshot.property("ExecMainCode") == Some("1")
                && snapshot.property("ExecMainStatus") == Some("0"),
            "runtime Keeper original successful terminal manager identity differs",
        )?;
        let held = stat(self.keeper_handles[1].as_raw_fd())?;
        require(
            held.device == captured["device"].as_u64().unwrap()
                && held.inode == captured["inode"].as_u64().unwrap(),
            "runtime Keeper original cgroup changed before stop",
        )?;
        require(
            matches!(read_retained_cgroup(self.keeper_handles[0].as_fd(),self.keeper_handles[1].as_fd(),&held)?,
            CgroupReadbackProgress::Observed(ref actual) if actual.creator_terminal && (actual.unlinked
                || (actual.procs.as_deref()==Some("")&&actual.events.as_ref().is_some_and(|v|v.lines().any(|s|s=="populated 0"))))),
            "runtime Keeper actual cgroup not empty before stop",
        )?;
        require(
            self.commands.is_empty(),
            "runtime Keeper stop intent cannot repeat",
        )?;
        self.commands.push(CommandQuery::retain(vec![
            "-n".into(),
            "/usr/bin/systemctl".into(),
            "--no-ask-password".into(),
            "stop".into(),
            unit.clone(),
        ]));
        self.commands[0].start()?;
        loop {
            common::before(cutoff)?;
            let now = guardian::monotonic_ns()?;
            if self.commands[0].poll(
                Instant::now()
                    + Duration::from_nanos(
                        cutoff
                            .checked_sub(now)
                            .filter(|v| *v > 0)
                            .ok_or_else(|| io::Error::other("original broker cutoff elapsed"))?,
                    ),
            )? {
                break;
            }
            self.pause(cutoff)?;
        }
        loop {
            let current = self.current_snapshot(&unit, cutoff)?;
            if current.property("LoadState") == Some("not-found") {
                require(
                    current.property("MainPID") == Some("0")
                        && current.property("ControlGroup") == Some(""),
                    "runtime Keeper absent-unit fields differ",
                )?;
                let identity = stat(self.keeper_handles[1].as_raw_fd())?;
                require(
                    identity.device == captured["device"].as_u64().unwrap()
                        && identity.inode == captured["inode"].as_u64().unwrap()
                        && matches!(read_retained_cgroup(self.keeper_handles[0].as_fd(),self.keeper_handles[1].as_fd(),&identity)?,
                        CgroupReadbackProgress::Observed(ref observed) if observed.creator_terminal&&observed.unlinked),
                    "runtime Keeper original held cgroup is not unlinked",
                )?;
                break;
            }
            require(
                current.property("InvocationID") == captured["invocation"].as_str(),
                "runtime Keeper unit replaced while retiring",
            )?;
            self.pause(cutoff)?;
        }
        Ok(())
    }
    /// Scoped join of this actual Child. The original S1 global finalizer is
    /// unchanged; this CLI also owns the unrelated still-running guest child.
    pub(super) fn join_child(
        launcher: &mut Launcher,
        directory: RawFd,
        cutoff: u64,
    ) -> io::Result<bool> {
        common::before(cutoff)?;
        launcher.drain()?;
        if launcher.eof != [true, true]
            || !terminal(
                launcher
                    .pidfd
                    .as_ref()
                    .ok_or_else(|| io::Error::other("original child pidfd absent"))?
                    .as_raw_fd(),
            )?
        {
            return Ok(false);
        }
        if launcher.reaped.is_none() {
            launcher.reaped = launcher.child.try_wait()?;
        }
        require(
            launcher.reaped.is_some_and(|wait| wait.success()),
            "original broker child exited unsuccessfully",
        )?;
        let pid = launcher.child.id() as i32;
        let raw = unsafe { libc::kill(-pid, 0) };
        let error = (raw == -1).then(io::Error::last_os_error);
        require(
            raw == -1 && error.as_ref().and_then(io::Error::raw_os_error) == Some(libc::ESRCH),
            "original broker child group remains",
        )?;
        for fd in launcher.log_files.iter().flatten() {
            if unsafe { libc::fsync(fd.as_raw_fd()) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        if unsafe { libc::fsync(directory) } != 0 {
            return Err(io::Error::last_os_error());
        }
        launcher.logs_synced = true;
        common::before(cutoff)?;
        Ok(true)
    }
    /// Advance the original startup and Keeper joins without replacing their cutoffs.
    pub fn progress_owned(&mut self, deadline: Instant) -> io::Result<()> {
        let result = (|| {
            if self.coordination.is_none() {
                return Ok(());
            }
            let (nonce, incarnation, startup) = self.run_fields()?;
            if self.pending_join.is_none() {
                let keeper_request = if !self.startup_failed_joined
                    && !self.keeper_joined
                    && !self.startup_success_observed
                {
                    self.keeper_channel.as_mut().unwrap().receive(4096)?
                } else {
                    None
                };
                let observed = if let Some(index) = keeper_request {
                    self.pending_from_keeper = true;
                    Some((true, index))
                } else {
                    self.coordination
                        .as_mut()
                        .unwrap()
                        .receive(4096)?
                        .map(|index| (false, index))
                };
                let Some((from_keeper, index)) = observed else {
                    return Ok(());
                };
                self.pending_from_keeper = from_keeper;
                let packet = if from_keeper {
                    &self.keeper_channel.as_ref().unwrap().packets[index]
                } else {
                    &self.coordination.as_ref().unwrap().packets[index]
                };
                let peer = if from_keeper {
                    common::credentials(
                        self.keeper_identity.as_ref().unwrap()["pid"]
                            .as_i64()
                            .unwrap() as i32,
                    )?
                } else {
                    self.coordination_peer.unwrap()
                };
                packet.exact(0, peer)?;
                self.pending_join = Some(serde_json::from_slice(&packet.bytes)?);
                require(
                    journal::canonical(self.pending_join.as_ref().unwrap())? == packet.bytes,
                    "broker parent request noncanonical",
                )?;
            }
            let request = self.pending_join.as_ref().unwrap().clone();
            require(
                request["nonce"] == nonce && request["incarnation"] == incarnation,
                "broker parent request changed run",
            )?;
            let response = if request["schema"] == "hermit-grouped-runtime-startup-failed-join-v1"
                && self.startup_joined
            {
                require(
                    self.pending_from_keeper
                        && !self.startup_success_observed
                        && !self.keeper_joined,
                    "original completed startup readback repeated or came from another peer",
                )?;
                let origin = request["original_start"]
                    .as_u64()
                    .ok_or_else(|| io::Error::other("completed startup recovery origin absent"))?;
                let cutoff = request["cutoff"]
                    .as_u64()
                    .ok_or_else(|| io::Error::other("completed startup recovery cutoff absent"))?;
                let now = guardian::monotonic_ns()?;
                require(
                    request
                        == json!({"schema":"hermit-grouped-runtime-startup-failed-join-v1","nonce":nonce,
                    "incarnation":incarnation,"original_start":origin,"cutoff":cutoff})
                        && origin <= now
                        && now < cutoff
                        && cutoff <= startup
                        && Instant::now() < deadline
                        && cutoff
                            <= origin.checked_add(1_000_000_000).ok_or_else(|| {
                                io::Error::other("completed startup recovery bound overflow")
                            })?,
                    "completed startup readback changed original failure bound",
                )?;
                if let Some(first) = self.failure_origin {
                    require(
                        cutoff
                            <= first.checked_add(1_000_000_000).ok_or_else(|| {
                                io::Error::other("earlier parent failure overflow")
                            })?,
                        "completed startup readback extends original failure",
                    )?;
                }
                let launcher = self.startup.as_ref().unwrap();
                require(
                    launcher.reaped.is_some_and(|wait| wait.code() == Some(0))
                        && launcher.eof == [true, true]
                        && launcher.logs_synced
                        && terminal(launcher.pidfd.as_ref().unwrap().as_raw_fd())?,
                    "original completed startup custody changed",
                )?;
                let pid = launcher.child.id() as i32;
                let raw = unsafe { libc::kill(-pid, 0) };
                let error = (raw == -1).then(io::Error::last_os_error);
                require(
                    raw == -1
                        && error.as_ref().and_then(io::Error::raw_os_error) == Some(libc::ESRCH),
                    "original completed startup group reappeared",
                )?;
                let retained = self.retained_startup_join.as_ref().ok_or_else(|| {
                    io::Error::other("original successful startup receipt absent")
                })?;
                common::before(cutoff)?;
                self.startup_success_observed = true;
                json!({"schema":"hermit-grouped-runtime-startup-already-joined-v1","nonce":nonce,"incarnation":incarnation,
                    "original_start":origin,"cutoff":cutoff,"pid":pid,"raw_wait_status":0,"eof":launcher.eof,
                    "group_absent":true,"logs_synced":launcher.logs_synced,"global_ECHILD_claimed":false,"retained_join":retained})
            } else if request["schema"] == "hermit-grouped-runtime-startup-failed-join-v1" {
                require(
                    self.pending_from_keeper && !self.startup_joined && !self.startup_failed_joined,
                    "failed startup join must be original Keeper request before a completed join",
                )?;
                let origin = request["original_start"]
                    .as_u64()
                    .ok_or_else(|| io::Error::other("failed startup origin absent"))?;
                let cutoff = request["cutoff"]
                    .as_u64()
                    .ok_or_else(|| io::Error::other("failed startup cutoff absent"))?;
                let now = guardian::monotonic_ns()?;
                require(
                    request
                        == json!({"schema":"hermit-grouped-runtime-startup-failed-join-v1","nonce":nonce,
                    "incarnation":incarnation,"original_start":origin,"cutoff":cutoff})
                        && origin <= now
                        && now < cutoff
                        && cutoff <= startup
                        && cutoff
                            <= origin
                                .checked_add(1_000_000_000)
                                .ok_or_else(|| io::Error::other("failed startup bound overflow"))?
                        && Instant::now() < deadline,
                    "failed startup changed original request/deadline",
                )?;
                if let Some(first) = self.failure_origin {
                    require(
                        cutoff
                            <= first.checked_add(1_000_000_000).ok_or_else(|| {
                                io::Error::other("original parent failure overflow")
                            })?,
                        "failed startup extends original parent refusal",
                    )?;
                }
                if self.failed_startup_observation.is_none() {
                    let actual_deadline =
                        deadline.min(Instant::now() + Duration::from_nanos(cutoff - now));
                    let Some(proof) = self
                        .startup
                        .as_mut()
                        .unwrap()
                        .failed_source_terminal(self.directories[0].as_raw_fd(), actual_deadline)?
                    else {
                        return Ok(());
                    };
                    self.failed_startup_observation = Some(proof.observation()?);
                }
                let observed = self.failed_startup_observation.as_ref().unwrap();
                let launcher = self.startup.as_mut().unwrap();
                if launcher.reaped.is_none() {
                    launcher.reaped = launcher.child.try_wait()?;
                }
                let wait = launcher
                    .reaped
                    .ok_or_else(|| io::Error::other("terminal startup Child wait disappeared"))?;
                require(
                    wait.code().is_some_and(|code| {
                        code > 0 && Some(code as i64) == observed["waitid_status"].as_i64()
                    }),
                    "original failed startup consumed wait differs from retained WNOWAIT",
                )?;
                let pid = launcher.child.id() as i32;
                let raw = unsafe { libc::kill(-pid, 0) };
                let error = (raw == -1).then(io::Error::last_os_error);
                require(
                    raw == -1
                        && error.as_ref().and_then(io::Error::raw_os_error) == Some(libc::ESRCH),
                    "failed original startup process group remains",
                )?;
                common::before(cutoff)?;
                let original_eof = launcher.eof;
                let original_logs_synced = launcher.logs_synced;
                if self.refusal.is_none() {
                    self.refusal = Some(Failure::capture(&io::Error::other(format!(
                        "original startup controller exited with retained status {}",
                        wait.into_raw()
                    ))));
                    self.failure_origin = Some(origin);
                }
                // This exact failed-startup request supplies the original cut;
                // no new clock is sampled for the ordinary source-owner unit.
                if self.source_authority_retirement.is_none() {
                    let owner = self.source_authority.as_mut().ok_or_else(|| {
                        io::Error::other("failed startup lost original source authority")
                    })?;
                    let outcome = owner.retire(cutoff);
                    if let Some(actual) = owner.retired_observation() {
                        self.source_authority_retirement = Some(
                            json!({"schema":"hermit-grouped-parent-source-authority-retired-v1",
                            "nonce":nonce,"incarnation":incarnation,"original_start":origin,"cutoff":cutoff,
                            "original_source_owner_error":outcome.as_ref().err().map(ToString::to_string),"actual":actual}),
                        );
                    } else {
                        match outcome {
                            Ok(false) => return Ok(()),
                            Err(error) => return Err(error),
                            Ok(true) => {
                                return Err(io::Error::other(
                                    "source authority successful return lacks actual retirement readback",
                                ));
                            }
                        }
                    }
                    let bytes =
                        journal::canonical(self.source_authority_retirement.as_ref().unwrap())?;
                    require(
                        bytes.len() <= 65_536,
                        "source authority failed-startup retirement receipt bound",
                    )?;
                    use std::io::Write;
                    let mut stderr = std::io::stderr().lock();
                    stderr.write_all(&bytes)?;
                    stderr.write_all(b"\n")?;
                    stderr.flush()?;
                    common::before(cutoff)?;
                    self.source_authority_retirement_recorded = true;
                }
                require(
                    self.source_authority_retirement_recorded,
                    "original source authority retirement receipt incomplete",
                )?;
                common::before(cutoff)?;
                self.startup_failed_joined = true;
                json!({"schema":"hermit-grouped-runtime-startup-failed-joined-v1","nonce":nonce,"incarnation":incarnation,
                    "original_start":origin,"cutoff":cutoff,"pid":pid,"raw_wait_status":wait.into_raw(),
                    "waitid":observed,"eof":original_eof,"group_absent":true,"logs_synced":original_logs_synced,
                    "global_ECHILD_claimed":false})
            } else if request["schema"] == "hermit-grouped-parent-join-startup-v1" {
                require(
                    !self.pending_from_keeper,
                    "successful startup join must come from original service",
                )?;
                require(
                    !self.startup_joined
                        && request
                            == json!({"schema":"hermit-grouped-parent-join-startup-v1",
                    "nonce":nonce,"incarnation":incarnation,"stage_deadline":startup}),
                    "startup original join repeated or altered",
                )?;
                common::before(startup)?;
                // The outside ordinary helper exits only after the Keeper has
                // validated the complete source archive and ACKed its actual
                // source terminal custody. Join its original unit/wrapper too.
                if !self
                    .source_authority
                    .as_mut()
                    .ok_or_else(|| io::Error::other("original source authority owner absent"))?
                    .retire(startup)?
                {
                    return Ok(());
                }
                if !Self::join_child(
                    self.startup.as_mut().unwrap(),
                    self.directories[0].as_raw_fd(),
                    startup,
                )? {
                    return Ok(());
                }
                self.startup_joined = true;
                let response = json!({"schema":"hermit-grouped-parent-startup-joined-v1","nonce":nonce,"incarnation":incarnation,
                    "stage_deadline":startup,"raw_wait_status":0,"eof":[true,true],"group_absent":true});
                self.retained_startup_join = Some(response.clone());
                response
            } else {
                require(
                    !self.pending_from_keeper
                        && request["schema"] == "hermit-grouped-parent-join-runtime-v1"
                        && self.startup_joined
                        && !self.keeper_joined,
                    "broker parent request is not the original next runtime join",
                )?;
                let origin = request["original_start"]
                    .as_u64()
                    .ok_or_else(|| io::Error::other("runtime original origin absent"))?;
                let cutoff = request["cutoff"]
                    .as_u64()
                    .ok_or_else(|| io::Error::other("runtime original cutoff absent"))?;
                require(
                    Instant::now() < deadline
                        && cutoff
                            <= origin
                                .checked_add(1_000_000_000)
                                .ok_or_else(|| io::Error::other("runtime origin overflow"))?
                        && terminal(self.run_controller.as_ref().unwrap().as_raw_fd())?,
                    "runtime original controller or cutoff differs",
                )?;
                common::before(cutoff)?;
                if !terminal(self.keeper_handles[0].as_raw_fd())? {
                    return Ok(());
                }
                self.stop_keeper_after_terminal(cutoff)?;
                while !Self::join_child(
                    self.keeper.as_mut().unwrap(),
                    self.directories[1].as_raw_fd(),
                    cutoff,
                )? {
                    self.pause(cutoff)?;
                }
                self.keeper_joined = true;
                json!({"schema":"hermit-grouped-parent-runtime-joined-v1","nonce":nonce,"incarnation":incarnation,
                    "original_start":origin,"cutoff":cutoff,"raw_wait_status":0,"eof":[true,true],"group_absent":true,"unit_absent":true})
            };
            let cutoff = response["cutoff"].as_u64().unwrap_or(startup);
            let channel = if self.pending_from_keeper {
                self.keeper_channel.as_mut().unwrap()
            } else {
                self.coordination.as_mut().unwrap()
            };
            common::send(channel, &response, &[], cutoff)?;
            self.last_join = Some(response);
            self.pending_join = None;
            Ok(())
        })();
        self.remember(result)
    }
    /// Whether both original children joined successfully with no retained refusal.
    pub fn completed(&self) -> bool {
        self.startup_joined
            && self.keeper_joined
            && self
                .source_authority
                .as_ref()
                .is_some_and(SourceAuthorityUnit::completed)
            && self.refusal.is_none()
    }
}
impl AcceptedPostSpawn for GroupedParentOwner {
    fn after_spawn(
        &mut self,
        spawned: AcceptedSpawned<'_>,
    ) -> io::Result<Option<GroupedBootstrapTransport>> {
        let result = self.configure(spawned).map(Some);
        let result = self.remember(result);
        if let Err(error) = &result {
            // Report the original hook refusal before enclosing Container
            // cleanup can wait. This hook precedes any accepted Bootstrap send.
            // Observations borrow retained owners only; no query is repeated.
            let observation = json!({"schema":"hermit-grouped-parent-hook-failure-v1",
                "run":self.run,"error":error.to_string(),"errno":error.raw_os_error(),
                "first_failure_origin":self.failure_origin,"original_stage_deadline":self.native,
                "accepted_bootstrap_submitted":false,"startup_child_retained":self.startup.is_some(),
                "source_authority":self.source_authority.as_ref().map(SourceAuthorityUnit::diagnostics),
                "keeper_capture":self.keeper_capture.as_ref().map(parent_launch::ParentLaunchCustody::diagnostics),
                "provider_capture":self.provider_capture.as_ref().map(parent_launch::ParentLaunchCustody::diagnostics)});
            if let Ok(mut bytes) = serde_json::to_vec(&observation) {
                // Keep this diagnostic within the existing bounded stderr.
                // Oversize raw query evidence stays in its original owner.
                if bytes.len() > 65_536 {
                    bytes=serde_json::to_vec(&json!({"schema":"hermit-grouped-parent-hook-failure-v1",
                        "run":self.run,"error":error.to_string(),"errno":error.raw_os_error(),
                        "first_failure_origin":self.failure_origin,"accepted_bootstrap_submitted":false,
                        "diagnostic_omitted_over_bound":true,"diagnostic_bytes":bytes.len()})).unwrap_or_default();
                }
                if bytes.len() <= 65_536 {
                    bytes.push(b'\n');
                    use std::io::Write;
                    let _ = std::io::stderr().lock().write_all(&bytes);
                }
            }
        }
        result
    }
    fn progress(&mut self, deadline: Instant) -> io::Result<()> {
        self.progress_owned(deadline)
    }
}
